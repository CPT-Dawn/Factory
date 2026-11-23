//! The exit codes of every `factory` command: the one table (spec #1, "Output contract").
//!
//! The `devlab` name keeps devlab's own codes and text; `run` passes its child's code through.

/// What a command's exit code says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Ok,
    Usage,
    /// A precondition or config is missing, or the daemon is unreachable.
    Precondition,
    NoLease,
    NeedsAttention,
    /// Busy or out of quota; the line names the ETA.
    Busy,
    DeviceLost,
    Emulator,
    Stack,
    Timeout,
}

impl Exit {
    pub const ALL: [Exit; 10] = [
        Exit::Ok,
        Exit::Usage,
        Exit::Precondition,
        Exit::NoLease,
        Exit::NeedsAttention,
        Exit::Busy,
        Exit::DeviceLost,
        Exit::Emulator,
        Exit::Stack,
        Exit::Timeout,
    ];

    pub const fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Usage => 2,
            Exit::Precondition => 3,
            Exit::NoLease => 4,
            Exit::NeedsAttention => 10,
            Exit::Busy => 75,
            Exit::DeviceLost => 76,
            Exit::Emulator => 77,
            Exit::Stack => 78,
            Exit::Timeout => 124,
        }
    }

    /// A few words for `--help`.
    pub const fn meaning(self) -> &'static str {
        match self {
            Exit::Ok => "ok",
            Exit::Usage => "usage",
            Exit::Precondition => "precondition, config or daemon unreachable",
            Exit::NoLease => "no lease",
            Exit::NeedsAttention => "needs attention",
            Exit::Busy => "busy or out of quota",
            Exit::DeviceLost => "device lost",
            Exit::Emulator => "emulator",
            Exit::Stack => "stack",
            Exit::Timeout => "timeout",
        }
    }

    pub fn from_code(code: u8) -> Option<Exit> {
        Exit::ALL.into_iter().find(|exit| exit.code() == code)
    }
}
