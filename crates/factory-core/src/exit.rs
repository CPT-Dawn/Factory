//! The exit codes of every `factory` command: the one table (spec #1, "Output contract").
//!
//! The `devlab` name keeps devlab's own codes and text; `run` passes its child's code through.
//!
//! One deliberate exception: `factory bar --json` exits 0 with a valid feed holding a single
//! critical "factory daemon down" chip when the daemon is unreachable, so the bar says what is
//! wrong instead of going blank (ADR 0001). `factory bar` without `--json` exits 3 as usual.

/// What a command's exit code says; the discriminant is the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    Ok = 0,
    Usage = 2,
    /// A precondition or config is missing, or the daemon is unreachable.
    Precondition = 3,
    NoLease = 4,
    NeedsAttention = 10,
    /// Busy or out of quota; the line names the ETA.
    Busy = 75,
    DeviceLost = 76,
    Emulator = 77,
    Stack = 78,
    Timeout = 124,
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
        self as u8
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
