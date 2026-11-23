//! Running a command inside a lease: its env on top of the caller's, its stdio the caller's
//! (or, for `factory lab`, its output in a file).
//!
//! The command is a child of this process, which stays alive until it ends: target-repo scripts
//! find the `devlab run` above them by walking their ancestors. It gets no time limit; it ends
//! when it ends.
//!
//! `devlab run` says what stopped it (#24). Claude Code stops a call with SIGTERM to its whole
//! tree and SIGKILL about a second later, so on SIGTERM, SIGINT or SIGHUP the run names the
//! signal at once: while queued it leaves the queue and exits; while its command runs it hands
//! the command the signal and exits once the command is gone. The daemon follows the run on the
//! acquire's connection and records how it ended in the ledger, `lost` when the connection
//! closes without a word.

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use factory_core::lab::args::Opts;
use factory_core::lab::run::{End, Phase, killed, stopped_queued, stopped_running};
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde::Serialize;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use super::cli::note;
use super::context::Ctx;
use super::lease::{self, lease_env};
use super::wire::{ACQUIRE, Granted, Lease, RECEIVED, RUN_END, RUN_START, RunAsk, RunEnd};
use crate::client::Session;
use crate::output::Failure;
use crate::protocol::Request;

/// Exit code when the command can't be run or waited for, as a shell says it.
const CANNOT_RUN: u8 = 127;
/// How long a run stopped while queued waits for the daemon to take it out of the queue before
/// it exits anyway.
const LEAVE_GRACE: Duration = Duration::from_millis(500);

/// `devlab run`: queues for the lease, takes it, runs `argv` in it and returns its exit code
/// (128 + the signal that ended it, or that stopped devlab).
pub fn run(ctx: &Ctx, opts: &Opts, quiet: bool, argv: &[String]) -> Result<u8, Failure> {
    let Some(program) = argv.first() else {
        return Err(Failure::usage("devlab run [opts] -- <cmd...>"));
    };
    let ask = RunAsk {
        command: argv.join(" "),
        session: std::env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty()),
    };
    let (params, listen) = lease::ask(ctx, opts, Some(ask))?;
    let mut session = Session::open(&lease::paths()?)?;
    let watch = Watch::install(program, session.notifier().ok())?;
    let asked = session.call_streaming::<_, Granted>(ACQUIRE, &params, listen, &mut |said| {
        note(said);
        if let Some(why) = said.strip_prefix("waiting: ") {
            watch.lock().reason = why.to_owned();
        }
    });
    let taken = asked.and_then(|granted| {
        let mut state = watch.lock();
        state.reason = format!("lease {} granted, not started yet", granted.lease.id);
        // The grant reached this run: the daemon may let its place go.
        notify(state.link.as_ref(), RECEIVED, &());
        drop(state);
        lease::take(ctx, opts, granted, &mut note)
    });
    let lease = match taken {
        Ok(lease) => lease,
        Err(failure) => {
            if let Some(signal) = watch.lock().stopped {
                return Ok(signal_code(signal));
            }
            watch.end(End::Exit(failure.exit.code()), Phase::Queued);
            return Err(failure);
        }
    };
    if !quiet {
        note(&lease::summary(&lease, ctx.project.as_ref())?);
    }
    let env = lease_env(&lease, ctx.project.as_ref())?;
    let child = {
        let mut state = watch.lock();
        if let Some(signal) = state.stopped {
            return Ok(signal_code(signal));
        }
        // Said before the spawn: a devlab killed the moment its command starts is a lost run
        // that was running, not one still queued.
        notify(state.link.as_ref(), RUN_START, &());
        match spawn(argv, &env, None) {
            Ok(child) => {
                state.child = Some(Running {
                    pidfd: child_pidfd(&child),
                    since: Instant::now(),
                });
                child
            }
            Err(e) => {
                drop(state);
                note(&format!("cannot run {program}: {e}"));
                watch.end(End::Exit(CANNOT_RUN), Phase::Queued);
                return Ok(CANNOT_RUN);
            }
        }
    };
    let waited = wait_for(child);
    let mut state = watch.lock();
    if let Some(signal) = state.stopped {
        return Ok(signal_code(signal));
    }
    let since = state.child.take().map_or_else(Instant::now, |c| c.since);
    let (end, code) = match waited {
        Ok(status) => match status.signal() {
            Some(signal) => {
                note(&killed(program, signal, since.elapsed().as_secs()));
                (End::Signal(signal), exit_code(&status))
            }
            None => (End::Exit(exit_code(&status)), exit_code(&status)),
        },
        Err(e) => {
            note(&format!("cannot wait for {program}: {e}"));
            (End::Exit(CANNOT_RUN), CANNOT_RUN)
        }
    };
    send_end(state.link.as_ref(), end, Phase::Running);
    drop(state);
    // The session stays open until the end is sent: its closing without one is a lost run.
    drop(session);
    Ok(code)
}

/// Runs `argv` in the lease and returns its exit code (128 + the signal that ended it);
/// [`CANNOT_RUN`] when it can't be run. Its stdout and stderr, and why it couldn't run, go to
/// `output` when given, else they are the caller's. `devlab exec` and `factory lab`'s run
/// and exec, which are not followed.
pub fn exec_in(
    ctx: &Ctx,
    lease: &Lease,
    argv: &[String],
    output: Option<&Path>,
) -> Result<u8, Failure> {
    let env = lease_env(lease, ctx.project.as_ref())?;
    let Some(program) = argv.first() else {
        return Err(Failure::usage("devlab exec -- <cmd...>"));
    };
    let mut log = match output {
        Some(path) => Some(File::create(path).map_err(|e| {
            Failure::precondition(format!(
                "cannot write {}: {e}; check XDG_RUNTIME_DIR",
                path.display()
            ))
        })?),
        None => None,
    };
    let ended = match spawn(argv, &env, log.as_ref()) {
        Ok(child) => wait_for(child)
            .map(|status| exit_code(&status))
            .map_err(|e| format!("cannot wait for {program}: {e}")),
        Err(e) => Err(format!("cannot run {program}: {e}")),
    };
    let why = match ended {
        Ok(code) => return Ok(code),
        Err(why) => why,
    };
    match log.as_mut() {
        Some(file) => {
            let _ = writeln!(file, "{why}");
        }
        None => note(&why),
    }
    Ok(CANNOT_RUN)
}

/// Starts `argv` with `env` on top of this process's, its stdout and stderr in `log` if given.
fn spawn(argv: &[String], env: &[(String, String)], log: Option<&File>) -> std::io::Result<Child> {
    let Some((program, args)) = argv.split_first() else {
        return Err(std::io::ErrorKind::InvalidInput.into());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    if let Some(file) = log {
        command.stdout(Stdio::from(file.try_clone()?));
        command.stderr(Stdio::from(file.try_clone()?));
    }
    command.spawn()
}

/// A handle that signals the child itself even once its pid is reused; the child is not
/// reaped yet, so its pid is still its own here.
fn child_pidfd(child: &Child) -> Option<OwnedFd> {
    let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw)?;
    pidfd_open(pid, PidfdFlags::empty()).ok()
}

/// The command this run is running.
struct Running {
    pidfd: Option<OwnedFd>,
    since: Instant,
}

/// What the signal thread needs to know of the run.
struct State {
    /// Why the run waits, as the daemon last said it.
    reason: String,
    /// The daemon connection, to send the run's end on.
    link: Option<UnixStream>,
    child: Option<Running>,
    /// The first signal that stopped devlab.
    stopped: Option<i32>,
}

/// The run as the signal thread sees it.
struct Watch {
    state: Arc<Mutex<State>>,
}

impl Watch {
    /// Takes SIGTERM, SIGINT and SIGHUP from their default (dying) and answers them on a
    /// thread of their own.
    fn install(program: &str, link: Option<UnixStream>) -> Result<Watch, Failure> {
        let state = Arc::new(Mutex::new(State {
            reason: "no answer from the daemon yet".to_owned(),
            link,
            child: None,
            stopped: None,
        }));
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])
            .map_err(|e| Failure::precondition(format!("cannot watch for signals: {e}; retry")))?;
        let watched = Arc::clone(&state);
        let program = program.to_owned();
        let started = Instant::now();
        thread::spawn(move || {
            for signal in signals.forever() {
                stopped(&watched, &program, started, signal);
            }
        });
        Ok(Watch { state })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Tells the daemon how the run ended.
    fn end(&self, end: End, phase: Phase) {
        send_end(self.lock().link.as_ref(), end, phase);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// devlab got `signal`: it says so before anything else. While the command runs, the command
/// gets the signal too, and the main thread exits once it is gone; while queued, the run leaves
/// the queue, and exits when the daemon has taken it out, or after [`LEAVE_GRACE`].
fn stopped(state: &Mutex<State>, program: &str, started: Instant, signal: i32) {
    let mut s = lock(state);
    let first = s.stopped.is_none();
    if first {
        s.stopped = Some(signal);
    }
    if let Some(child) = &s.child {
        if first {
            note(&stopped_running(
                program,
                signal,
                child.since.elapsed().as_secs(),
            ));
        }
        if let (Some(pidfd), Some(sig)) = (&child.pidfd, Signal::from_named_raw(signal)) {
            let _ = pidfd_send_signal(pidfd, sig);
        }
        if first {
            send_end(s.link.as_ref(), End::Stopped(signal), Phase::Running);
        }
        return;
    }
    if !first {
        return;
    }
    note(&stopped_queued(
        signal,
        started.elapsed().as_secs(),
        &s.reason,
    ));
    send_end(s.link.as_ref(), End::Stopped(signal), Phase::Queued);
    drop(s);
    thread::sleep(LEAVE_GRACE);
    std::process::exit(i32::from(signal_code(signal)));
}

fn send_end(link: Option<&UnixStream>, end: End, phase: Phase) {
    notify(
        link,
        RUN_END,
        &RunEnd {
            outcome: end.outcome(),
            phase: phase.word().to_owned(),
        },
    );
}

/// Sends a notification on the run's connection; a daemon that is gone loses it.
fn notify<P: Serialize>(link: Option<&UnixStream>, method: &str, params: &P) {
    let Some(mut link) = link else { return };
    let Ok(params) = serde_json::to_value(params) else {
        return;
    };
    let request = Request {
        id: 0,
        method: method.to_owned(),
        params,
    };
    if let Ok(mut line) = serde_json::to_string(&request) {
        line.push('\n');
        let _ = link.write_all(line.as_bytes());
    }
}

/// Waits until the child has ended. The one place this process idles while the command runs.
fn wait_for(mut child: Child) -> std::io::Result<ExitStatus> {
    loop {
        match child.wait() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            done => return done,
        }
    }
}

/// 128 + `signal`, as a shell reports a process the signal ended.
fn signal_code(signal: i32) -> u8 {
    u8::try_from((128 + signal) & 0xff).unwrap_or(1)
}

/// What the shell would call the status: its code, or 128 + the signal that ended it.
fn exit_code(status: &ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
        (None, Some(signal)) => signal_code(signal),
        (None, None) => 1,
    }
}
