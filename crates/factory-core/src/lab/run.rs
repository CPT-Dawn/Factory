//! How a `devlab run` ends, as its last stderr line and its `lab-run` ledger event say it
//! (#24). Claude Code stops a call with SIGTERM to its whole tree and SIGKILL about a second
//! later, and reports 137 either way, so devlab names the signal before it dies, and the daemon
//! names the runs that never got to.

/// Where a run was when it ended: waiting for its lease, or running its command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Queued for the lease, or granted it and not yet started.
    Queued,
    Running,
}

impl Phase {
    pub fn word(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Running => "running",
        }
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// devlab exited with this code: the command's, or devlab's own before the command ran.
    Exit(u8),
    /// A signal killed the command while devlab was not signalled.
    Signal(i32),
    /// devlab was signalled.
    Stopped(i32),
    /// The client's connection closed without saying how it ended: a SIGKILL'd devlab.
    Lost,
    /// A daemon stop cut the daemon off from the run, and the run never came back to say how
    /// it ended: its devlab exited while no daemon followed it.
    DaemonStopped,
}

impl End {
    /// The ledger's `outcome`: `exit 3`, `signal SIGKILL`, `stopped SIGTERM`, `lost` or
    /// `daemon-stopped`.
    pub fn outcome(self) -> String {
        match self {
            End::Exit(code) => format!("exit {code}"),
            End::Signal(signal) => format!("signal {}", signal_name(signal)),
            End::Stopped(signal) => format!("stopped {}", signal_name(signal)),
            End::Lost => "lost".to_owned(),
            End::DaemonStopped => "daemon-stopped".to_owned(),
        }
    }
}

/// `SIGTERM` for 15; a signal without a name here is `SIG<n>`.
pub fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        4 => "ILL",
        5 => "TRAP",
        6 => "ABRT",
        7 => "BUS",
        8 => "FPE",
        9 => "KILL",
        10 => "USR1",
        11 => "SEGV",
        12 => "USR2",
        13 => "PIPE",
        14 => "ALRM",
        15 => "TERM",
        24 => "XCPU",
        25 => "XFSZ",
        _ => return format!("SIG{signal}"),
    };
    format!("SIG{name}")
}

/// `stopped by SIGTERM after 3 s, while queued: <why it waits>`.
pub fn stopped_queued(signal: i32, secs: u64, reason: &str) -> String {
    format!(
        "stopped by {} after {secs} s, while queued: {reason}",
        signal_name(signal)
    )
}

/// `<cmd> stopped by SIGTERM after 3 s`: devlab was signalled while the command ran.
pub fn stopped_running(cmd: &str, signal: i32, secs: u64) -> String {
    format!("{cmd} stopped by {} after {secs} s", signal_name(signal))
}

/// `<cmd> killed by SIGKILL after 3 s`: a signal ended the command, not devlab.
pub fn killed(cmd: &str, signal: i32, secs: u64) -> String {
    format!("{cmd} killed by {} after {secs} s", signal_name(signal))
}
