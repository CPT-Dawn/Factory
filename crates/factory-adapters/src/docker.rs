//! Containers, through `docker`: only whether one runs.

use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process;

/// How long `docker inspect` may take.
const TIMEOUT: Duration = Duration::from_secs(10);

pub trait Containers {
    /// Whether the container `name` exists and runs; false when docker can't say.
    fn running(&self, name: &str) -> bool;
}

/// The `docker` on PATH.
pub struct Docker;

impl Containers for Docker {
    fn running(&self, name: &str) -> bool {
        let mut cmd = Command::new("docker");
        cmd.args(["inspect", "-f", "{{.State.Running}}", name])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        process::run(cmd, TIMEOUT).is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
        })
    }
}

/// Containers that run while their name is in the set.
#[derive(Debug, Default, Clone)]
pub struct FakeContainers {
    pub running: BTreeSet<String>,
}

impl Containers for FakeContainers {
    fn running(&self, name: &str) -> bool {
        self.running.contains(name)
    }
}
