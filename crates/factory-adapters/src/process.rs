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
pub fn run(mut cmd: Command, timeout: Duration) -> Result<Output, RunError> {
    cmd.process_group(0);
    let child = cmd.spawn().map_err(RunError::Spawn)?;
    let leader = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(child.wait_with_output()));
    match rx.recv_timeout(timeout) {
        Ok(output) => output.map_err(RunError::Wait),
        Err(_) => {
            if let Some(group) = i32::try_from(leader).ok().and_then(Pid::from_raw) {
                let _ = kill_process_group(group, Signal::KILL);
            }
            // The waiting thread reaps the leader once the group is dead.
            let _ = rx.recv_timeout(REAP_TIMEOUT);
            Err(RunError::TimedOut(timeout))
        }
    }
}
