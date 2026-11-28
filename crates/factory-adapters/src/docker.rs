//! Containers, through `docker`: whether one runs, and keeping a Supabase stack's containers
//! from coming back on their own.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process;

/// How long `docker inspect`, `ps` or `update` may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The label the supabase CLI puts on each container of a stack, valued its project id.
pub const SUPABASE_PROJECT_LABEL: &str = "com.supabase.cli.project";

pub trait Containers {
    /// Whether the container `name` exists and runs; false when docker can't say.
    fn running(&self, name: &str) -> bool;
    /// Sets the restart policy of every container of the Supabase stack `project_id` to `no`,
    /// so a reboot or a docker restart doesn't start it again; whether docker did.
    fn no_restart(&self, project_id: &str) -> bool;
}

/// The `docker` on PATH.
pub struct Docker;

fn docker(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("docker");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    process::run(cmd, TIMEOUT)
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Containers for Docker {
    fn running(&self, name: &str) -> bool {
        docker(&["inspect", "-f", "{{.State.Running}}", name])
            .is_some_and(|out| out.trim() == "true")
    }

    fn no_restart(&self, project_id: &str) -> bool {
        let label = format!("label={SUPABASE_PROJECT_LABEL}={project_id}");
        let Some(listed) = docker(&["ps", "-aq", "--filter", &label]) else {
            return false;
        };
        let ids: Vec<&str> = listed.split_whitespace().collect();
        if ids.is_empty() {
            return true;
        }
        let mut args = vec!["update", "--restart=no"];
        args.extend(ids);
        docker(&args).is_some()
    }
}

/// Containers that run while their name is in the set; each stack `no_restart` was asked of
/// is recorded.
#[derive(Debug, Default, Clone)]
pub struct FakeContainers {
    pub running: BTreeSet<String>,
    pub no_restart: RefCell<Vec<String>>,
}

impl Containers for FakeContainers {
    fn running(&self, name: &str) -> bool {
        self.running.contains(name)
    }

    fn no_restart(&self, project_id: &str) -> bool {
        self.no_restart.borrow_mut().push(project_id.to_owned());
        true
    }
}
