//! Each external tool the factory drives (adb, docker, systemd, gh, herdr, agy, codex, claude)
//! sits behind an adapter here, and the binary reaches external tools only through these.
//!
//! herdr is a server, not a command, so its fake is a server too: the binary-seam tests run a fake
//! herdr on a socket (`crates/factory/tests/cli/support/herdr.rs`).

pub mod herdr;
