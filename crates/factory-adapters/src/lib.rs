//! Each external tool the factory drives (adb, docker, systemd, git, gh, herdr, agy, codex,
//! claude) sits behind a trait here, and each trait has a fake beside it.
//!
//! The binary reaches external tools only through these traits. Every tool runs through
//! [`process::run`]: in its own process group, with a timeout that kills the group whole.

pub mod gh;
pub mod git;
pub mod process;
pub mod reviewer;
