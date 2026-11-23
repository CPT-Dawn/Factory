//! Each external tool the factory drives (adb, docker, systemd, gh, herdr, agy, codex, claude)
//! sits behind an adapter here, and the binary reaches external tools only through these.
//!
//! Each adapter is a trait with a fake beside it, such as [`herdr::HerdrApi`] and
//! [`herdr::fake::FakeHerdr`]. herdr is a server, not a command, so the binary-seam tests run a
//! fake herdr server on a socket too (`crates/factory/tests/cli/support/herdr.rs`).

pub mod herdr;
