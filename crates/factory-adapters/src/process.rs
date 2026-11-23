//! Running an external process: in its own process group, under a timeout that kills the
//! whole group.

use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};

/// How long a killed group may take to be reaped before the run gives up on it.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum RunError {
    /// The program could not be started.
    Spawn(io::Error),
    /// Waiting for it failed.
    Wait(io::Error),
    /// It outlived its timeout, so its process group was killed.
    TimedOut(Duration),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "cannot start it: {e}"),
            RunError::Wait(e) => write!(f, "lost it: {e}"),
            RunError::TimedOut(timeout) => write!(
                f,
                "it outlived {}s and was killed with its process group",
                timeout.as_secs()
            ),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs `cmd` to its end in a process group of its own, with the stdio its caller set. When
/// it outlives `timeout`, the whole group is killed, so nothing it started lives on.
pub fn run(cmd: Command, timeout: Duration) -> Result<Output, RunError> {
    run_in_group(cmd, timeout).1
}

/// Runs `cmd` as [`run`] does, then kills what its process group still holds once it has
/// exited: a program that stops on a timeout of its own can leave its children running.
pub fn run_swept(cmd: Command, timeout: Duration) -> Result<Output, RunError> {
    let (group, result) = run_in_group(cmd, timeout);
    if let Some(group) = group {
        // An empty group has nothing to signal; that is the usual case.
        let _ = kill_process_group(group, Signal::KILL);
    }
    result
}

/// Runs `cmd` in a group of its own, killing the group on timeout: the group, once started,
/// and how the run ended.
fn run_in_group(mut cmd: Command, timeout: Duration) -> (Option<Pid>, Result<Output, RunError>) {
    cmd.process_group(0);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return (None, Err(RunError::Spawn(e))),
    };
    let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(child.wait_with_output()));
    let result = match rx.recv_timeout(timeout) {
        Ok(output) => output.map_err(RunError::Wait),
        Err(_) => {
            if let Some(group) = group {
                let _ = kill_process_group(group, Signal::KILL);
            }
            // The waiting thread reaps the leader once the group is dead.
            let _ = rx.recv_timeout(REAP_TIMEOUT);
            Err(RunError::TimedOut(timeout))
        }
    };
    (group, result)
}
