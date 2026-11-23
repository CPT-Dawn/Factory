//! The CLI's side of the daemon protocol: connect, starting the daemon when it isn't running,
//! send one request and read its response.
//!
//! The CLI holds no state. Concurrent CLIs that find no daemon queue on a start lock, so one of
//! them starts it and the rest connect to it.

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use factory_core::exit::Exit;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::output::Failure;
use crate::paths::Paths;
use crate::protocol::{Outcome, Request, Response};

/// How long a started daemon may take to say it is ready.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a CLI waits for another CLI that is starting the daemon.
const START_LOCK_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a stopping daemon may take to exit: its drain, then its runtime's shutdown.
const STOPPING_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the daemon may take to answer a request that doesn't wait on an event.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// The next step when the daemon fails to start for no reason it gave.
const RUN_IT: &str = "run `factory daemon` to see why";

/// The herdr pane this CLI runs in, which a wait names as its waiter; none outside herdr.
pub fn waiter() -> Option<String> {
    env::var("HERDR_PANE_ID")
        .ok()
        .filter(|pane| !pane.is_empty())
}

/// Sends one request on a connection of its own and returns its result, or the daemon's error
/// as a [`Failure`].
pub fn call<P: Serialize, R: DeserializeOwned>(
    paths: &Paths,
    method: &str,
    params: &P,
) -> Result<R, Failure> {
    Session::open(paths)?.call(method, params)
}

/// Sends a request the daemon answers when an event fires, at most `limit` from now. The daemon
/// keeps the time; past `limit` and a grace period the CLI gives up with exit 124 on its own.
pub fn wait<P: Serialize, R: DeserializeOwned>(
    paths: &Paths,
    method: &str,
    params: &P,
    limit: Duration,
) -> Result<R, Failure> {
    Session::open(paths)?.exchange(
        method,
        params,
        Some(limit + ANSWER_TIMEOUT),
        Exit::Timeout,
        &mut |_| {},
    )
}

/// [`call`] for a request that waits: each note the daemon sends before its answer goes to
/// `on_note`, and each line may take up to `read_timeout` (none waits as long as it takes).
pub fn call_streaming<P: Serialize, R: DeserializeOwned>(
    paths: &Paths,
    method: &str,
    params: &P,
    read_timeout: Option<Duration>,
    mut on_note: impl FnMut(&str),
) -> Result<R, Failure> {
    Session::open(paths)?.call_streaming(method, params, read_timeout, &mut on_note)
}

/// One connection to the daemon, kept open across requests: what the daemon grants on it (a
/// review slot) is held until the session drops.
pub struct Session {
    reader: BufReader<UnixStream>,
    paths: Paths,
    next_id: u64,
}

impl Session {
    pub fn open(paths: &Paths) -> Result<Session, Failure> {
        Ok(Session {
            reader: BufReader::new(connect(paths)?),
            paths: paths.clone(),
            next_id: 1,
        })
    }

    /// Sends a request the daemon answers at once.
    pub fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, Failure> {
        self.exchange(
            method,
            params,
            Some(ANSWER_TIMEOUT),
            Exit::Precondition,
            &mut |_| {},
        )
    }

    /// [`call_streaming`] on this session's connection.
    pub fn call_streaming<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
        read_timeout: Option<Duration>,
        on_note: &mut dyn FnMut(&str),
    ) -> Result<R, Failure> {
        self.exchange(method, params, read_timeout, Exit::Precondition, on_note)
    }

    /// A second handle on the connection, to send notifications on (requests the daemon doesn't
    /// answer) from another thread, even while a request waits for its answer.
    pub fn notifier(&self) -> std::io::Result<UnixStream> {
        self.reader.get_ref().try_clone()
    }

    /// Sends a request the daemon answers when what it waits on happens, however long that is.
    pub fn wait<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, Failure> {
        self.exchange(method, params, None, Exit::Precondition, &mut |_| {})
    }

    /// Sends a request and reads its answer, giving up with `impatient` once `patience` (if
    /// any) has passed without one.
    fn exchange<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
        patience: Option<Duration>,
        impatient: Exit,
        on_note: &mut dyn FnMut(&str),
    ) -> Result<R, Failure> {
        let paths = &self.paths;
        let broken = |e: &dyn std::fmt::Display| {
            Failure::precondition(format!(
                "lost the daemon at {}: {e}; see {} and retry",
                paths.socket().display(),
                paths.daemon_log().display()
            ))
        };
        let request = Request {
            id: self.next_id,
            method: method.to_owned(),
            params: serde_json::to_value(params).map_err(|e| broken(&e))?,
        };
        self.next_id += 1;
        let mut line = serde_json::to_string(&request).map_err(|e| broken(&e))?;
        line.push('\n');
        let stream = self.reader.get_ref();
        (&*stream)
            .write_all(line.as_bytes())
            .map_err(|e| broken(&e))?;
        stream.set_read_timeout(patience).map_err(|e| broken(&e))?;
        // Notes may come before the answer; each line gets the whole patience.
        loop {
            let mut answer = String::new();
            match self.reader.read_line(&mut answer) {
                Ok(0) => return Err(broken(&"it closed the connection without answering")),
                Ok(_) => {}
                Err(e) if is_timeout(&e) => {
                    return Err(Failure::new(
                        impatient,
                        format!(
                            "the daemon did not answer within {}s; see {}",
                            patience.unwrap_or_default().as_secs(),
                            paths.daemon_log().display()
                        ),
                    ));
                }
                Err(e) => return Err(broken(&e)),
            }
            let response: Response = serde_json::from_str(&answer).map_err(|e| broken(&e))?;
            match response.outcome {
                Outcome::Note(note) => on_note(&note),
                Outcome::Result(value) => {
                    return serde_json::from_value(value).map_err(|e| broken(&e));
                }
                Outcome::Error(error) => {
                    let exit = Exit::from_code(error.code).unwrap_or(Exit::Precondition);
                    return Err(Failure::new(exit, error.message));
                }
            }
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Connects to the daemon, starting it first when nothing answers on the socket.
fn connect(paths: &Paths) -> Result<UnixStream, Failure> {
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        return Ok(stream);
    }
    let unreachable = |reason: String| {
        Failure::precondition(format!(
            "daemon unreachable at {} and could not start: {reason}",
            paths.socket().display()
        ))
    };
    paths
        .create_runtime()
        .map_err(|failure| unreachable(failure.message))?;
    let _start_lock = take_start_lock(paths).map_err(unreachable)?;
    // Another CLI may have started it while this one queued on the lock.
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        return Ok(stream);
    }
    await_stopping_daemon(paths).map_err(unreachable)?;
    start_daemon(paths).map_err(unreachable)?;
    UnixStream::connect(paths.socket()).map_err(|e| unreachable(format!("{e}; retry")))
}

/// Waits for a daemon that holds the daemon lock but no longer answers, as one does while it
/// stops, to exit, so the next can start.
fn await_stopping_daemon(paths: &Paths) -> Result<(), String> {
    let path = paths.daemon_lock();
    let cannot = |e: &dyn std::fmt::Display| {
        format!("cannot lock {}: {e}; check XDG_RUNTIME_DIR", path.display())
    };
    let lock = match File::options().write(true).open(&path) {
        Ok(lock) => lock,
        // No daemon has run in this session.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(cannot(&e)),
    };
    match within(STOPPING_TIMEOUT, move || lock.lock()) {
        // Dropping the file frees the lock for the daemon this CLI starts.
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(cannot(&e)),
        None => Err(format!(
            "a daemon holds {} without answering for over {}s; stop it, then retry",
            path.display(),
            STOPPING_TIMEOUT.as_secs()
        )),
    }
}

/// Takes the start lock, which a CLI holds while it starts the daemon. Dropping the file
/// releases it.
pub(crate) fn take_start_lock(paths: &Paths) -> Result<File, String> {
    let path = paths.start_lock();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}; check XDG_RUNTIME_DIR", path.display()))?;
    match within(START_LOCK_TIMEOUT, move || file.lock().map(|()| file)) {
        Some(Ok(file)) => Ok(file),
        Some(Err(e)) => Err(format!(
            "cannot lock {}: {e}; check XDG_RUNTIME_DIR",
            path.display()
        )),
        None => Err(format!(
            "another factory held {} for over {}s; retry",
            path.display(),
            START_LOCK_TIMEOUT.as_secs()
        )),
    }
}

/// Starts `factory daemon` in its own process group and waits for its ready line, killing the
/// whole group when it isn't ready in time.
fn start_daemon(paths: &Paths) -> Result<(), String> {
    let log_path = paths.daemon_log();
    let log = File::create(&log_path).map_err(|e| {
        format!(
            "cannot create {}: {e}; check XDG_RUNTIME_DIR",
            log_path.display()
        )
    })?;
    let exe = env::current_exe()
        .map_err(|e| format!("cannot find the factory binary: {e}; reinstall factory"))?;
    let mut daemon = Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot run factory daemon: {e}; reinstall factory"))?;
    let Some(stdout) = daemon.stdout.take() else {
        return Err(format!("factory daemon has no stdout; {RUN_IT}"));
    };
    let ready = within(START_TIMEOUT, move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).map(|n| n > 0)
    });
    match ready {
        Some(Ok(true)) => Ok(()),
        Some(_) => {
            let _ = daemon.wait();
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            let last = log.lines().rev().find(|line| !line.trim().is_empty());
            Err(last.map_or_else(
                || format!("it exited without a word; {RUN_IT}"),
                |line| line.strip_prefix("factory: ").unwrap_or(line).to_owned(),
            ))
        }
        None => {
            // The daemon leads its own process group (process_group(0) above).
            if let Some(group) = i32::try_from(daemon.id()).ok().and_then(Pid::from_raw) {
                let _ = kill_process_group(group, Signal::KILL);
            }
            let _ = daemon.wait();
            Err(format!(
                "it was not ready within {}s; see {}",
                START_TIMEOUT.as_secs(),
                log_path.display()
            ))
        }
    }
}

/// Runs `work` on its own thread and returns its result, or none once `limit` has passed.
pub(crate) fn within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(limit).ok()
}
