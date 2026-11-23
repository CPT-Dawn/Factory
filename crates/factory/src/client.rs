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
/// How long the daemon may take to answer a request that doesn't wait on an event.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// The next step when the daemon fails to start for no reason it gave.
const RUN_IT: &str = "run `factory daemon` to see why";

/// Sends one request and returns its result, or the daemon's error as a [`Failure`].
pub fn call<P: Serialize, R: DeserializeOwned>(
    paths: &Paths,
    method: &str,
    params: &P,
) -> Result<R, Failure> {
    exchange(paths, method, params, ANSWER_TIMEOUT, Exit::Precondition)
}

/// Sends a request the daemon answers when an event fires, at most `limit` from now. The daemon
/// keeps the time; past `limit` and a grace period the CLI gives up with exit 124 on its own.
pub fn wait<P: Serialize, R: DeserializeOwned>(
    paths: &Paths,
    method: &str,
    params: &P,
    limit: Duration,
) -> Result<R, Failure> {
    exchange(paths, method, params, limit + ANSWER_TIMEOUT, Exit::Timeout)
}

fn exchange<P: Serialize, R: DeserializeOwned>(
    paths: &Paths,
    method: &str,
    params: &P,
    patience: Duration,
    impatient: Exit,
) -> Result<R, Failure> {
    let stream = connect(paths)?;
    let broken = |e: &dyn std::fmt::Display| {
        Failure::precondition(format!(
            "lost the daemon at {}: {e}; see {} and retry",
            paths.socket().display(),
            paths.daemon_log().display()
        ))
    };
    let request = Request {
        id: 1,
        method: method.to_owned(),
        params: serde_json::to_value(params).map_err(|e| broken(&e))?,
    };
    let mut line = serde_json::to_string(&request).map_err(|e| broken(&e))?;
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .map_err(|e| broken(&e))?;
    stream
        .set_read_timeout(Some(patience))
        .map_err(|e| broken(&e))?;
    let mut answer = String::new();
    match BufReader::new(&stream).read_line(&mut answer) {
        Ok(0) => return Err(broken(&"it closed the connection without answering")),
        Ok(_) => {}
        Err(e) if is_timeout(&e) => {
            return Err(Failure::new(
                impatient,
                format!(
                    "the daemon did not answer within {}s; see {}",
                    patience.as_secs(),
                    paths.daemon_log().display()
                ),
            ));
        }
        Err(e) => return Err(broken(&e)),
    }
    let response: Response = serde_json::from_str(&answer).map_err(|e| broken(&e))?;
    match response.outcome {
        Outcome::Result(value) => serde_json::from_value(value).map_err(|e| broken(&e)),
        Outcome::Error(error) => {
            let exit = Exit::from_code(error.code).unwrap_or(Exit::Precondition);
            Err(Failure::new(exit, error.message))
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
    start_daemon(paths).map_err(unreachable)?;
    UnixStream::connect(paths.socket()).map_err(|e| unreachable(format!("{e}; retry")))
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
