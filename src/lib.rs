use crossbeam_channel::Receiver;
use crossbeam_channel::Select;
use crossbeam_channel::Sender;
use io::{stdout, BufRead, BufReader, Read, Write};
use lazy_static::lazy_static;
use parking_lot::Condvar;
use parking_lot::Mutex;
use std::{
    io,
    process::{Command, Output, Stdio},
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

fn child_stream_to_lines<R>(stream: R) -> Arc<Mutex<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let out = Arc::new(Mutex::new(vec![]));
    let vec = out.clone();
    let mut reader = BufReader::new(stream);
    thread::Builder::new()
        .name("child_stream_to_vec".into())
        .spawn(move || loop {
            let mut buf = Vec::with_capacity(1);
            match reader.read_until(b'\n', &mut buf) {
                Err(e) => {
                    eprintln!("{}] Error reading from stream: {}", line!(), e);
                    break;
                }
                Ok(got) => match got {
                    0 => break,
                    _ => *vec.lock() = buf,
                },
            }
        })
        .expect("!thread");
    out
}

pub fn run_with_status(
    mut command: Command,
    deactivate_sender: &Sender<()>,
    show_output: bool,
) -> io::Result<Output> {
    if show_output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if show_output {
        let out = child_stream_to_lines(child.stdout.take().unwrap());
        let err = child_stream_to_lines(child.stderr.take().unwrap());
        while child.try_wait()? == None {
            let mut out_lock = out.lock();
            let mut err_lock = err.lock();
            if !out_lock.is_empty() || !err_lock.is_empty() {
                let mut stdout = stdout();
                stdout.write_all(b"\x1b[2K\x1b[G")?;
                stdout.write_all(&out_lock)?;
                stdout.write_all(&err_lock)?;
                stdout.flush()?;
                // stdout.write_all(b"\n")?;
            }
            out_lock.clear();
            err_lock.clear();
        }
    }
    let output = child.wait_with_output()?;
    deactivate_sender.send(()).expect("!send");
    Ok(output)
}

pub struct SpinnerThreads {
    _spinner_thread: JoinHandle<()>,
    _ticker_thread: JoinHandle<()>,
}

lazy_static! {
    static ref SPINNER_MUTEX: Mutex<()> = Mutex::new(());
}
static SPINNER_TICKER: Condvar = Condvar::new();

fn spinner_thread(
    spinner_chars: Vec<char>,
    message_receiver: Option<Receiver<String>>,
    done_message_receiver: Option<Receiver<String>>,
    activator: Receiver<()>,
    deactivator: Receiver<()>,
    stopper: Receiver<()>,
) {
    let mut i: usize = 0;
    let mut selector = Select::new();
    let op1 = selector.recv(&activator);
    let op2 = selector.recv(&stopper);
    'spinner: loop {
        let op = selector.select();
        match op.index() {
            i if i == op1 => op.recv(&activator).unwrap(),
            i if i == op2 => {
                op.recv(&stopper).unwrap();
                break 'spinner;
            }
            _ => unreachable!(),
        }
        let message = if let Some(ref message_receiver) = message_receiver {
            message_receiver.recv().unwrap()
        } else {
            String::new()
        };
        let done_message = if let Some(ref done_message_receiver) = done_message_receiver {
            done_message_receiver.recv().unwrap()
        } else {
            String::new()
        };
        while deactivator.try_recv().is_err() {
            {
                let mut lock = SPINNER_MUTEX.lock();
                SPINNER_TICKER.wait(&mut lock);
            }
            eprint!(
                "\x1b[2K\x1b[G\x1b[1;32m{}\x1b[0m {}",
                spinner_chars[i], message
            );
            i = (i + 1) % (spinner_chars.len() - 1);
        }
        eprintln!(
            "\x1b[2K\x1b[G\x1b[1;32m{}\x1b[0m {}",
            spinner_chars[spinner_chars.len() - 1],
            done_message
        );
    }
}

fn ticker_thread(interval: Duration, stopper: Receiver<()>) {
    while stopper.try_recv().is_err() {
        SPINNER_TICKER.notify_one();
        thread::sleep(interval);
    }
}

impl SpinnerThreads {
    pub fn new<S: AsRef<str> + Send + Sync>(
        spinner_chars: S,
        spinner_tick_interval: Duration,
        status_message_receiver: Option<Receiver<String>>,
        done_message_receiver: Option<Receiver<String>>,
        spinner_activator: Receiver<()>,
        spinner_deactivator: Receiver<()>,
        stopper: Receiver<()>,
    ) -> Self {
        let stopper_2 = stopper.clone();
        let spinner_chars: Vec<char> = spinner_chars.as_ref().chars().collect();
        Self {
            _spinner_thread: thread::spawn(move || {
                spinner_thread(
                    spinner_chars,
                    status_message_receiver,
                    done_message_receiver,
                    spinner_activator,
                    spinner_deactivator,
                    stopper,
                )
            }),
            _ticker_thread: thread::spawn(move || ticker_thread(spinner_tick_interval, stopper_2)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn test_run() {
        let (status_message_sender, status_message_receiver) = unbounded::<String>();
        let (done_message_sender, done_message_receiver) = unbounded::<String>();
        let (spinner_activator_sender, spinner_activator) = unbounded::<()>();
        let (spinner_deactivator_sender, spinner_deactivator) = unbounded::<()>();
        let (stop_sender, stopper) = unbounded::<()>();
        let _threads = SpinnerThreads::new(
            "/-\\|*",
            Duration::from_millis(100),
            Some(status_message_receiver),
            Some(done_message_receiver),
            spinner_activator,
            spinner_deactivator,
            stopper,
        );

        let mut command = Command::new("yum");
        command.arg("info").arg("test");
        status_message_sender.send("Sleeping...".into()).unwrap();
        done_message_sender.send("Slept!".into()).unwrap();
        spinner_activator_sender.send(()).unwrap();
        run_with_status(command, &spinner_deactivator_sender, true).unwrap();

        thread::sleep(Duration::from_millis(100));

        stop_sender.send(()).unwrap();
    }
}
