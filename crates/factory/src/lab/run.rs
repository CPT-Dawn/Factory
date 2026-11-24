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
//! closes without a word. A daemon that stops leaves the run to the next one (#37): devlab
//! connects again and asks it to follow the run (`lab.run.resume`), and exits only once its end
//! is recorded. A run whose devlab is gone before any daemon picks it up is recorded
//! `daemon-stopped`.

use std::fs::File;
use std::io::{BufRead, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
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
use super::lease::{self, apply_reverses, lease_env, me, wake};
use super::wire::{
    ACQUIRE, AcquireParams, Granted, Lease, RECEIVED, RUN_END, RUN_END_ID, RUN_RESUME, RUN_START,
    RUN_STOPPING, RunAsk, RunEnd, RunResume,
};
use crate::client::{self, Session};
use crate::output::Failure;
use crate::paths::Paths;
use crate::protocol::{Request, Response};

/// Exit code when the command can't be run or waited for, as a shell says it.
const CANNOT_RUN: u8 = 127;
/// How long a run stopped while queued waits for the daemon to take it out of the queue before
/// it exits anyway.
const LEAVE_GRACE: Duration = Duration::from_millis(500);
/// How long a run that ended waits for a daemon to record its end: one that went away gets
/// the time a resume takes, and a last answer.
const SETTLE_TIMEOUT: Duration = client::RESUME_TIMEOUT.saturating_add(client::ANSWER_TIMEOUT);

/// `devlab run`: [`run_to`] with the caller's stdio, the lease's summary said unless `quiet`.
pub fn run(
    ctx: &Ctx,
    opts: &Opts,
    quiet: bool,
    keep: bool,
    argv: &[String],
) -> Result<u8, Failure> {
    run_to(ctx, opts, keep, argv, &Out::Caller { quiet }).map(|ran| ran.code)
}

/// Where a run's command's output, and what the run says, go.
pub enum Out<'a> {
    /// The caller's stdout and stderr, and devlab's stderr; `quiet` leaves out the lease's
    /// summary.
    Caller { quiet: bool },
    /// The file this makes for the lease granted (`factory lab run`); nothing is said before.
    File(&'a dyn Fn(&Lease) -> Result<PathBuf, Failure>),
}

/// How a run ended: its exit code, and the lease it ran in and the file its output went to
/// once it was granted one.
pub struct Ran {
    pub code: u8,
    pub lease: Option<i64>,
    pub output: Option<PathBuf>,
}

/// A run: queues for the lease, takes it, runs `argv` in it (its devices woken and the
/// project's ports reversed onto them) and returns its exit code
/// (128 + the signal that ended it, or that stopped devlab). A lease the run made ends with it,
/// however it ends, unless `keep`; one it found (an acquire's, an earlier `--keep`) stays. The
/// daemon follows it as running, so its devices are never reclaimed from under it.
pub fn run_to(
    ctx: &Ctx,
    opts: &Opts,
    keep: bool,
    argv: &[String],
    out: &Out<'_>,
) -> Result<Ran, Failure> {
    let Some(program) = argv.first() else {
        return Err(Failure::usage("devlab run [opts] -- <cmd...>"));
    };
    let mut ran = Ran {
        code: 0,
        lease: None,
        output: None,
    };
    let say = Say::new(None, matches!(out, Out::Caller { .. }));
    let ask = RunAsk {
        command: argv.join(" "),
        session: std::env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty()),
        keep,
    };
    let (params, listen) = lease::ask(ctx, opts, Some(ask))?;
    let paths = lease::paths()?;
    let watch = Watch::install(program, None, true, say.clone())?;
    let ask_at = |resume| AcquireParams {
        resume,
        ..params.clone()
    };
    // A daemon that comes back says why it waits again: say it once.
    let mut said = String::new();
    let asked = client::call_queued_on::<_, Granted>(
        &paths,
        ACQUIRE,
        RECEIVED,
        ask_at,
        listen,
        &mut |why| {
            if let Some(reason) = why.strip_prefix("waiting: ") {
                watch.lock().reason = reason.to_owned();
            }
            if why != said {
                say.line(why);
                why.clone_into(&mut said);
            }
        },
        &mut |session| watch.lock().link = session.notifier().ok(),
    );
    let taken = asked.and_then(|(granted, session)| {
        watch.lock().reason = format!("lease {} granted, not started yet", granted.lease.id);
        watch.follow(session, paths);
        if let Out::File(make) = out {
            let path = make(&granted.lease)?;
            let _ = say.file.set(create_log(&path)?);
            ran.output = Some(path);
        }
        ran.lease = Some(granted.lease.id);
        lease::take(ctx, opts, granted, &mut |why| say.line(why))
    });
    let lease = match taken {
        Ok(lease) => lease,
        Err(failure) => {
            if let Some(signal) = watch.lock().stopped {
                watch.settle();
                ran.code = signal_code(signal);
                return Ok(ran);
            }
            watch.end(End::Exit(failure.exit.code()), Phase::Queued);
            return Err(failure);
        }
    };
    if let Out::Caller { quiet: false } = out {
        note(&lease::summary(&lease, ctx.project.as_ref())?);
    }
    wake(&lease);
    apply_reverses(&lease, ctx.project.as_ref(), &mut |why| say.line(why))?;
    let env = lease_env(&lease, ctx.project.as_ref())?;
    let child = {
        let mut state = watch.lock();
        if let Some(signal) = state.stopped {
            drop(state);
            watch.settle();
            ran.code = signal_code(signal);
            return Ok(ran);
        }
        // Said before the spawn: a devlab killed the moment its command starts is a lost run
        // that was running, not one still queued.
        notify(state.link.as_ref(), 0, RUN_START, &());
        state.started = true;
        match spawn(argv, &env, say.file.get()) {
            Ok(child) => {
                state.child = Some(Running {
                    pidfd: child_pidfd(&child),
                    since: Instant::now(),
                });
                child
            }
            Err(e) => {
                drop(state);
                say.line(&format!("cannot run {program}: {e}"));
                watch.end(End::Exit(CANNOT_RUN), Phase::Queued);
                ran.code = CANNOT_RUN;
                return Ok(ran);
            }
        }
    };
    let waited = wait_for(child);
    let mut state = watch.lock();
    if let Some(signal) = state.stopped {
        state.child = None;
        drop(state);
        // Only once the command is gone does the run end, and its lease with it (#53).
        watch.end(End::Stopped(signal), Phase::Running);
        ran.code = signal_code(signal);
        return Ok(ran);
    }
    let since = state.child.take().map_or_else(Instant::now, |c| c.since);
    drop(state);
    let (end, code) = match waited {
        Ok(status) => match status.signal() {
            Some(signal) => {
                say.line(&killed(program, signal, since.elapsed().as_secs()));
                (End::Signal(signal), exit_code(&status))
            }
            None => (End::Exit(exit_code(&status)), exit_code(&status)),
        },
        Err(e) => {
            say.line(&format!("cannot wait for {program}: {e}"));
            (End::Exit(CANNOT_RUN), CANNOT_RUN)
        }
    };
    // The end is in the ledger before devlab exits, whichever daemon follows the run by then.
    watch.end(end, Phase::Running);
    ran.code = code;
    Ok(ran)
}

/// The file `factory lab` gives a command's output, made empty.
fn create_log(path: &Path) -> Result<File, Failure> {
    File::create(path).map_err(|e| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check XDG_RUNTIME_DIR",
            path.display()
        ))
    })
}

/// Runs `argv` in the lease, its devices woken and the project's ports reversed onto them, and
/// returns its exit code (128 + the signal that ended it, or that stopped this process);
/// [`CANNOT_RUN`] when it can't be run. Its stdout and stderr, and what it says, go to `output`
/// when given, else they are the caller's. `devlab exec` and `factory lab`'s run and exec,
/// which are not followed. Like `devlab run`, it names a signal that stops it or kills its
/// command (#37).
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
    let log = output.map(create_log).transpose()?;
    let say = Say::new(log.as_ref().and_then(|file| file.try_clone().ok()), true);
    wake(lease);
    apply_reverses(lease, ctx.project.as_ref(), &mut |why| say.line(why))?;
    let watch = Watch::install(program, None, false, say.clone())?;
    let child = {
        let mut state = watch.lock();
        if let Some(signal) = state.stopped {
            say.line(&stopped_running(program, signal, 0));
            return Ok(signal_code(signal));
        }
        match spawn(argv, &env, log.as_ref()) {
            Ok(child) => {
                state.child = Some(Running {
                    pidfd: child_pidfd(&child),
                    since: Instant::now(),
                });
                child
            }
            Err(e) => {
                say.line(&format!("cannot run {program}: {e}"));
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
    drop(state);
    match waited {
        Ok(status) => {
            if let Some(signal) = status.signal() {
                say.line(&killed(program, signal, since.elapsed().as_secs()));
            }
            Ok(exit_code(&status))
        }
        Err(e) => {
            say.line(&format!("cannot wait for {program}: {e}"));
            Ok(CANNOT_RUN)
        }
    }
}

/// Where a run or exec says what happened: the file `factory lab` gave its command's output,
/// once there is one; before, devlab's stderr if `stderr`, else nowhere.
#[derive(Clone)]
struct Say {
    file: Arc<OnceLock<File>>,
    stderr: bool,
}

impl Say {
    fn new(file: Option<File>, stderr: bool) -> Say {
        let once = OnceLock::new();
        if let Some(file) = file {
            let _ = once.set(file);
        }
        Say {
            file: Arc::new(once),
            stderr,
        }
    }

    fn line(&self, why: &str) {
        match self.file.get() {
            Some(file) => {
                let mut file: &File = file;
                let _ = writeln!(file, "{why}");
            }
            None if self.stderr => note(why),
            None => {}
        }
    }
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

/// What the signal thread, and the thread that keeps the run's daemon, need to know of the run.
struct State {
    /// A `devlab run`, which a signal before its command starts takes out of the queue; an
    /// exec's command starts at once, so its caller sees the stop then.
    queued: bool,
    /// Why the run waits, as the daemon last said it.
    reason: String,
    /// The daemon connection, to send the run's end on.
    link: Option<UnixStream>,
    child: Option<Running>,
    /// The first signal that stopped devlab.
    stopped: Option<i32>,
    /// The run's command was started, as the daemon was told.
    started: bool,
    /// How the run ended, as the daemon was told; told again to a daemon that picks it up.
    end: Option<(End, Phase)>,
    /// A daemon follows the run, and answers its end.
    followed: bool,
    /// The daemon recorded the end, or none will.
    settled: bool,
}

/// The run as the signal thread sees it.
struct Watch {
    state: Arc<(Mutex<State>, Condvar)>,
}

impl Watch {
    /// Takes SIGTERM, SIGINT and SIGHUP from their default (dying) and answers them on a
    /// thread of their own.
    fn install(
        program: &str,
        link: Option<UnixStream>,
        queued: bool,
        say: Say,
    ) -> Result<Watch, Failure> {
        let state = Arc::new((
            Mutex::new(State {
                queued,
                reason: "no answer from the daemon yet".to_owned(),
                link,
                child: None,
                stopped: None,
                started: false,
                end: None,
                followed: false,
                settled: false,
            }),
            Condvar::new(),
        ));
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])
            .map_err(|e| Failure::precondition(format!("cannot watch for signals: {e}; retry")))?;
        let watched = Arc::clone(&state);
        let program = program.to_owned();
        let started = Instant::now();
        thread::spawn(move || {
            for signal in signals.forever() {
                stopped(&watched, &program, started, signal, &say);
            }
        });
        Ok(Watch { state })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state.0)
    }

    /// From the grant on, reads what the daemon says on `session` on a thread of its own: the
    /// answer to the run's end, or the daemon going away. A run whose daemon went away asks the
    /// next to follow it ([`RUN_RESUME`]), telling it again of its start and end.
    fn follow(&self, session: Session, paths: Paths) {
        let state = Arc::clone(&self.state);
        lock(&state.0).followed = true;
        thread::spawn(move || keep(&state, session, &paths));
    }

    /// Tells the daemon how the run ended, and waits until it is recorded.
    fn end(&self, end: End, phase: Phase) {
        let mut state = self.lock();
        send_end(state.link.as_ref(), end, phase);
        state.end = Some((end, phase));
        drop(state);
        self.settle();
    }

    /// Waits until the daemon following the run recorded its end, or none will: one that went
    /// away gets [`client::RESUME_TIMEOUT`] to come back.
    fn settle(&self) {
        let (mutex, settled) = &*self.state;
        let state = lock(mutex);
        if !state.followed {
            return;
        }
        let _ = settled.wait_timeout_while(state, SETTLE_TIMEOUT, |s| !s.settled);
    }
}

/// Reads the run's daemon connection until the daemon answers its end; a connection that
/// breaks first is made again, if a daemon takes the run up.
fn keep(state: &(Mutex<State>, Condvar), session: Session, paths: &Paths) {
    let (mutex, settled) = state;
    let mut reader = session.into_reader();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok_and(|n| n > 0) {
            let answer = serde_json::from_str::<Response>(&line);
            if answer.is_ok_and(|a| a.id == Some(RUN_END_ID)) {
                break;
            }
            continue;
        }
        let resumed = client::reattach(paths, RUN_RESUME, &RunResume { client: me() });
        let Ok(session) = resumed else {
            break;
        };
        let mut s = lock(mutex);
        s.link = session.notifier().ok();
        if s.started {
            notify(s.link.as_ref(), 0, RUN_START, &());
        }
        if let Some((end, phase)) = s.end {
            send_end(s.link.as_ref(), end, phase);
        } else if let (Some(signal), Some(_)) = (s.stopped, &s.child) {
            send_stopping(s.link.as_ref(), signal);
        }
        drop(s);
        reader = session.into_reader();
    }
    lock(mutex).settled = true;
    settled.notify_all();
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// devlab got `signal`: it says so before anything else. While the command runs, the command
/// gets the signal too, the daemon hears the run is stopping, and the main thread ends the run
/// and exits once the command is gone (the lease is held until then); while queued, the run leaves
/// the queue, and exits when the daemon has taken it out, or after [`LEAVE_GRACE`]. Once the
/// command is over, it only cuts short the wait for the daemon to record the run's end.
fn stopped(
    state: &(Mutex<State>, Condvar),
    program: &str,
    started: Instant,
    signal: i32,
    say: &Say,
) {
    let (state, settled) = state;
    let mut s = lock(state);
    if s.started && s.child.is_none() {
        // The command is over: devlab only waits for its end to be recorded, and stops
        // waiting, with the command's code.
        s.settled = true;
        settled.notify_all();
        return;
    }
    let first = s.stopped.is_none();
    if first {
        s.stopped = Some(signal);
    }
    if let Some(child) = &s.child {
        if first {
            say.line(&stopped_running(
                program,
                signal,
                child.since.elapsed().as_secs(),
            ));
        }
        if let (Some(pidfd), Some(sig)) = (&child.pidfd, Signal::from_named_raw(signal)) {
            let _ = pidfd_send_signal(pidfd, sig);
        }
        if first {
            send_stopping(s.link.as_ref(), signal);
        }
        return;
    }
    if !first || !s.queued {
        return;
    }
    say.line(&stopped_queued(
        signal,
        started.elapsed().as_secs(),
        &s.reason,
    ));
    send_end(s.link.as_ref(), End::Stopped(signal), Phase::Queued);
    s.end = Some((End::Stopped(signal), Phase::Queued));
    drop(s);
    thread::sleep(LEAVE_GRACE);
    std::process::exit(i32::from(signal_code(signal)));
}

/// Tells the daemon a signal stopped devlab while its command runs: the run's end, should
/// devlab be killed before the command is gone.
fn send_stopping(link: Option<&UnixStream>, signal: i32) {
    notify(
        link,
        0,
        RUN_STOPPING,
        &RunEnd {
            outcome: End::Stopped(signal).outcome(),
            phase: Phase::Running.word().to_owned(),
        },
    );
}

fn send_end(link: Option<&UnixStream>, end: End, phase: Phase) {
    notify(
        link,
        RUN_END_ID,
        RUN_END,
        &RunEnd {
            outcome: end.outcome(),
            phase: phase.word().to_owned(),
        },
    );
}

/// Sends a request on the run's connection, as `id` (0 for a notification), without waiting
/// for an answer; a daemon that is gone loses it.
fn notify<P: Serialize>(link: Option<&UnixStream>, id: u64, method: &str, params: &P) {
    let Some(mut link) = link else { return };
    let Ok(params) = serde_json::to_value(params) else {
        return;
    };
    let request = Request {
        id,
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
