use crossbeam::{bounded, RecvError, SendError, TryRecvError};
use crossbeam_channel::Receiver;
use crossbeam_channel::Select;
use crossbeam_channel::Sender;
use crossterm::terminal;
use crossterm::terminal::ClearType;
use crossterm::{cursor, execute, style};
use io::Write;
use lazy_static::lazy_static;
use parking_lot::Condvar;
use parking_lot::Mutex;
use std::{
    io,
    thread::{self, JoinHandle},
    time::Duration,
};

pub fn run_closure<T, F: Fn() -> T>(
    message: Option<&str>,
    done_message: Option<&str>,
    communicator: &SpinnerCommunicator,
    closure: F,
) -> Result<T, SendError<()>> {
    if let Some(message) = message {
        communicator
            .send_status_message(message)
            .map_err(|_| SendError(()))?;
    }
    if let Some(done_message) = done_message {
        communicator
            .send_done_message(done_message)
            .map_err(|_| SendError(()))?;
    }
    communicator.activate()?;
    let res = closure();
    communicator.deactivate()?;
    Ok(res)
}

pub struct SpinnerThreads {
    _spinner_thread: JoinHandle<()>,
    _ticker_thread: JoinHandle<()>,
}

lazy_static! {
    static ref SPINNER_MUTEX: Mutex<()> = Mutex::new(());
}
static SPINNER_TICKER: Condvar = Condvar::new();

pub struct Spinner {
    states: Vec<String>,
    tick_interval: Duration,
    message_receiver: Receiver<String>,
    done_message_receiver: Receiver<String>,
    activator: Receiver<()>,
    deactivator: Receiver<()>,
    stopper: Receiver<()>,
    state_index: usize,
}

impl Spinner {
    fn new(
        states: &[&str],
        tick_interval: Duration,
        message_receiver: Receiver<String>,
        done_message_receiver: Receiver<String>,
        activator: Receiver<()>,
        deactivator: Receiver<()>,
        stopper: Receiver<()>,
    ) -> Self {
        assert!(states.len() > 1);
        Self {
            states: states.iter().map(|s| s.to_string()).collect(),
            tick_interval,
            message_receiver,
            done_message_receiver,
            activator,
            deactivator,
            stopper,
            state_index: 0,
        }
    }

    fn wait_for_tick(&mut self) {
        let mut lock = SPINNER_MUTEX.lock();
        SPINNER_TICKER.wait(&mut lock);
        self.state_index = (self.state_index + 1) % (self.states.len() - 1);
    }

    fn state(&self) -> &str {
        &self.states[self.state_index]
    }

    fn done_state(&self) -> &str {
        &self.states.last().unwrap()
    }
}

pub struct SpinnerCommunicator {
    activate_sender: Sender<()>,
    deactivate_sender: Sender<()>,
    status_message_sender: Sender<String>,
    done_message_sender: Sender<String>,
    stop_sender: Sender<()>,
}

impl SpinnerCommunicator {
    fn new(
        activate_sender: Sender<()>,
        deactivate_sender: Sender<()>,
        status_message_sender: Sender<String>,
        done_message_sender: Sender<String>,
        stop_sender: Sender<()>,
    ) -> Self {
        Self {
            activate_sender,
            deactivate_sender,
            status_message_sender,
            done_message_sender,
            stop_sender,
        }
    }

    pub fn activate(&self) -> Result<(), SendError<()>> {
        self.activate_sender.send(())
    }
    pub fn deactivate(&self) -> Result<(), SendError<()>> {
        self.deactivate_sender.send(())
    }
    pub fn send_status_message(&self, msg: &str) -> Result<(), SendError<String>> {
        self.status_message_sender.send(msg.to_string())
    }
    pub fn send_done_message(&self, msg: &str) -> Result<(), SendError<String>> {
        self.done_message_sender.send(msg.to_string())
    }
    pub fn stop(&self) -> Result<(), SendError<()>> {
        self.stop_sender.send(())
    }
}

pub fn spawn_spinner(states: &[&str], tick_interval: Duration) -> SpinnerCommunicator {
    let (message_sender, message_receiver) = bounded::<String>(1);
    let (done_message_sender, done_message_receiver) = bounded::<String>(1);
    let (activate_sender, activator) = bounded::<()>(1);
    let (deactivate_sender, deactivator) = bounded::<()>(1);
    let (stop_sender, stopper) = bounded::<()>(1);
    let spinner = Spinner::new(
        states,
        tick_interval,
        message_receiver,
        done_message_receiver,
        activator,
        deactivator,
        stopper.clone(),
    );

    thread::spawn(move || ticker_thread(tick_interval, stopper));
    thread::spawn(move || spinner_thread(spinner));

    SpinnerCommunicator::new(
        activate_sender,
        deactivate_sender,
        message_sender,
        done_message_sender,
        stop_sender,
    )
}

fn receive_multiple<T>(rs: &[&Receiver<T>]) -> Result<usize, RecvError> {
    let mut sel = Select::new();
    for r in rs {
        sel.recv(r);
    }

    let op = sel.select();
    let i = op.index();
    op.recv(&rs[i])?;
    Ok(i)
}

fn spinner_thread(mut spinner: Spinner) {
    'spinner: loop {
        match receive_multiple(&[&spinner.activator, &spinner.stopper]) {
            Ok(i) => match i {
                0 => {}
                1 => break 'spinner,
                _ => unreachable!(),
            },
            Err(_) => continue,
        }
        let mut message = match spinner.message_receiver.try_recv() {
            Ok(m) => m,
            Err(TryRecvError::Empty) => String::new(),
            Err(e) => panic!(e),
        };
        let done_message = match spinner.done_message_receiver.try_recv() {
            Ok(m) => m,
            Err(TryRecvError::Empty) => String::new(),
            Err(e) => panic!(e),
        };
        while spinner.deactivator.try_recv().is_err() {
            spinner.wait_for_tick();
            {
                let mut stderr = io::stderr();
                execute!(
                    stderr,
                    terminal::Clear(ClearType::CurrentLine),
                    cursor::MoveToColumn(0),
                    style::SetForegroundColor(style::Color::Green),
                )
                .unwrap();
                if let Ok(m) = spinner.message_receiver.try_recv() {
                    message = m;
                }
                write!(stderr, "{}", spinner.state()).unwrap();
                execute!(stderr, style::ResetColor {},).unwrap();
                write!(stderr, " {}", message).unwrap();
            }
        }
        let mut stderr = io::stderr();
        execute!(
            stderr,
            terminal::Clear(ClearType::CurrentLine),
            cursor::MoveToColumn(0),
            style::SetForegroundColor(style::Color::Green),
        )
        .unwrap();
        write!(stderr, "{}", spinner.done_state()).unwrap();
        execute!(stderr, style::ResetColor {},).unwrap();
        writeln!(stderr, " {}", done_message).unwrap();
    }
}

fn ticker_thread(interval: Duration, stopper: Receiver<()>) {
    while stopper.try_recv().is_err() {
        SPINNER_TICKER.notify_one();
        thread::sleep(interval);
    }
}

impl SpinnerThreads {
    pub fn new(spinner: Spinner) -> Self {
        let stopper_2 = spinner.stopper.clone();
        let tick_interval = spinner.tick_interval;
        Self {
            _spinner_thread: thread::spawn(move || spinner_thread(spinner)),
            _ticker_thread: thread::spawn(move || ticker_thread(tick_interval, stopper_2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[test]
    fn test_run() {
        let spinner_communicator =
            spawn_spinner(&["/", "-", "\\", "|", "*"], Duration::from_millis(100));

        let res = run_closure(
            Some("Sleeping..."),
            Some("Slept"),
            &spinner_communicator,
            || Command::new("sleep").arg("1").output().unwrap(),
        )
        .unwrap();

        assert!(res.status.success());
    }

    #[test]
    fn changing_message() {
        let spinner_communicator =
            spawn_spinner(&["/", "-", "\\", "|", "*"], Duration::from_millis(100));

        let res = run_closure(
            Some("Sleeping again..."),
            Some("Finally done"),
            &spinner_communicator,
            || {
                let mut child = Command::new("sleep").arg("5").spawn().unwrap();
                thread::sleep(Duration::from_millis(2500));
                spinner_communicator
                    .status_message_sender
                    .send("Still sleeping...".into())
                    .unwrap();
                child.wait().unwrap()
            },
        )
        .unwrap();

        assert!(res.success());
    }
}
