//! User units, through `systemctl --user`.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process;

/// How long `systemctl` may take.
const TIMEOUT: Duration = Duration::from_secs(30);

pub trait Units {
    /// What `systemctl --user is-active <unit>` says (`active`, `inactive`, ...); empty when
    /// it can't be asked.
    fn is_active(&self, unit: &str) -> String;
}

/// The user's systemd, through the `systemctl` on PATH.
pub struct Systemctl;

impl Units for Systemctl {
    fn is_active(&self, unit: &str) -> String {
        let mut cmd = Command::new("systemctl");
        cmd.args(["--user", "is-active", unit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        process::run(cmd, TIMEOUT)
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default()
    }
}

/// Units in the states the map gives; any other is `inactive`.
#[derive(Debug, Default, Clone)]
pub struct FakeUnits {
    pub states: BTreeMap<String, String>,
}

impl Units for FakeUnits {
    fn is_active(&self, unit: &str) -> String {
        self.states
            .get(unit)
            .cloned()
            .unwrap_or_else(|| "inactive".to_owned())
    }
}
