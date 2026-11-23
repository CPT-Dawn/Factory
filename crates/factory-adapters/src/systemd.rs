//! The user's systemd manager, through `systemctl --user`.

use std::fmt;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::process;

/// How long one systemctl call may take; stopping a unit waits for it to stop.
const TIMEOUT: Duration = Duration::from_secs(90);

/// Why a systemctl call failed: the call and what systemctl said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdError(pub String);

impl fmt::Display for SystemdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SystemdError {}

/// The user's service manager: what installing and removing the factory's units asks of it.
pub trait UserManager {
    /// Rereads the unit files.
    fn reload(&self) -> Result<(), SystemdError>;
    /// Enables `unit` and starts it now.
    fn enable_now(&self, unit: &str) -> Result<(), SystemdError>;
    /// Disables `unit`, so it no longer starts with the session.
    fn disable(&self, unit: &str) -> Result<(), SystemdError>;
    /// Stops `units`, waiting until they have stopped.
    fn stop(&self, units: &[&str]) -> Result<(), SystemdError>;
}

/// The manager `systemctl --user` on PATH reaches.
pub struct Systemctl;

impl Systemctl {
    fn run(args: &[&str]) -> Result<(), SystemdError> {
        let call = format!("systemctl --user {}", args.join(" "));
        let mut cmd = Command::new("systemctl");
        cmd.arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let output =
            process::run(cmd, TIMEOUT).map_err(|e| SystemdError(format!("{call}: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let said = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("it failed and said nothing");
        Err(SystemdError(format!("{call} failed: {said}")))
    }
}

impl UserManager for Systemctl {
    fn reload(&self) -> Result<(), SystemdError> {
        Systemctl::run(&["daemon-reload"])
    }

    fn enable_now(&self, unit: &str) -> Result<(), SystemdError> {
        Systemctl::run(&["enable", "--now", unit])
    }

    fn disable(&self, unit: &str) -> Result<(), SystemdError> {
        Systemctl::run(&["disable", unit])
    }

    fn stop(&self, units: &[&str]) -> Result<(), SystemdError> {
        let mut args = vec!["stop"];
        args.extend_from_slice(units);
        Systemctl::run(&args)
    }
}

/// A manager that records what it was asked, for tests of code that installs units. Each call
/// is kept as the words `systemctl --user` would get; a call starting with `failing` fails.
#[derive(Debug, Default)]
pub struct FakeManager {
    pub calls: Mutex<Vec<String>>,
    pub failing: Option<String>,
}

impl FakeManager {
    fn record(&self, call: String) -> Result<(), SystemdError> {
        let refused = self
            .failing
            .as_deref()
            .is_some_and(|failing| call.starts_with(failing));
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call.clone());
        if refused {
            return Err(SystemdError(format!("systemctl --user {call} failed")));
        }
        Ok(())
    }
}

impl UserManager for FakeManager {
    fn reload(&self) -> Result<(), SystemdError> {
        self.record("daemon-reload".to_owned())
    }

    fn enable_now(&self, unit: &str) -> Result<(), SystemdError> {
        self.record(format!("enable --now {unit}"))
    }

    fn disable(&self, unit: &str) -> Result<(), SystemdError> {
        self.record(format!("disable {unit}"))
    }

    fn stop(&self, units: &[&str]) -> Result<(), SystemdError> {
        self.record(format!("stop {}", units.join(" ")))
    }
}
