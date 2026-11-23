//! Each external tool the factory drives (adb, docker, systemd, git, gh, herdr, agy, codex,
//! claude) sits behind a trait here, and each trait has a fake beside it, such as
//! [`herdr::HerdrApi`] and [`herdr::fake::FakeHerdr`]. So does the Claude Code session inbox
//! ([`inbox::InboxApi`]) the daemon posts into.
//!
//! The binary reaches external tools only through these traits. Every tool runs through
//! [`process::run`]: in its own process group, with a timeout that kills the group whole.
//! herdr is a server, not a command: it is reached on its socket, and the binary-seam tests run
//! a fake herdr server (`crates/factory/tests/cli/support/herdr.rs`). A session inbox is a socket
//! too, and those tests run a fake of it (`crates/factory/tests/cli/support/inbox.rs`).

pub mod adb;
pub mod devlab;
pub mod docker;
pub mod emulator;
pub mod gh;
pub mod git;
pub mod herdr;
pub mod inbox;
pub mod process;
pub mod reviewer;
pub mod supabase;
pub mod systemd;
