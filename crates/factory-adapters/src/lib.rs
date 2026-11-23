//! Each external tool the factory drives (adb, docker, systemd, gh, herdr, agy, codex, claude)
//! sits behind a trait here, and each trait has a fake beside it.
//!
//! The binary reaches external tools only through these traits. No adapter exists yet: the
//! first arrives with the first command that runs an external tool, and with it the binary's
//! dependency on this crate.
