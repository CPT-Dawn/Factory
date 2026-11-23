//! Running an external process: in its own process group, under a timeout that kills the
//! whole group.

use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};

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
    let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(child.wait_with_output()));
    match rx.recv_timeout(timeout) {
        Ok(output) => output.map_err(RunError::Wait),
        Err(_) => {
            if let Some(group) = group {
                let _ = kill_process_group(group, Signal::KILL);
            }
            // The waiting thread reaps the leader once the group is dead.
            let _ = rx.recv_timeout(REAP_TIMEOUT);
            Err(RunError::TimedOut(timeout))
        }
    }
}

/// Runs `cmd` as [`run`] does, then kills what its process group still holds once it has
/// exited: a program that stops on a timeout of its own can leave its children running. Its
/// output is read only once it has exited, so its stdout and stderr should go to files.
pub fn run_swept(mut cmd: Command, timeout: Duration) -> Result<Output, RunError> {
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let leader = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let (tx, rx) = mpsc::channel();
    // The leader stays unreaped once it exits, so no other group can take its ID while this
    // one is killed.
    thread::spawn(move || tx.send(leader.map_or(Ok(()), exited)));
    let sweep = || {
        if let Some(group) = leader {
            // An empty group has nothing to signal; that is the usual case.
            let _ = kill_process_group(group, Signal::KILL);
        }
    };
    let ended = rx.recv_timeout(timeout);
    let timed_out = ended.is_err();
    let ended = ended.or_else(|_| {
        sweep();
        rx.recv_timeout(REAP_TIMEOUT)
    });
    sweep();
    match ended {
        Ok(Ok(())) if !timed_out => child.wait_with_output().map_err(RunError::Wait),
        Ok(Err(e)) => {
            let _ = child.wait();
            Err(RunError::Wait(e))
        }
        Ok(Ok(())) => {
            let _ = child.wait();
            Err(RunError::TimedOut(timeout))
        }
        Err(_) => Err(RunError::TimedOut(timeout)),
    }
}

/// Waits for `leader` to exit, leaving it to be reaped.
fn exited(leader: Pid) -> io::Result<()> {
    loop {
        match waitid(
            WaitId::Pid(leader),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => return Ok(()),
        }
    }
}
