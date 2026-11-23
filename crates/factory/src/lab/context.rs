//! Who is asking and from where: the machine config, the worktree, its project and the owner.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use factory_adapters::process;
use factory_core::exit::Exit;
use factory_core::lab::config::{self, MachineConfig, ProjectConfig};

use super::owner::find_owner;
use super::wire::{FindParams, Owner};
use crate::output::Failure;

pub struct Ctx {
    pub cfg: MachineConfig,
    pub worktree: String,
    pub project: Option<ProjectConfig>,
    pub owner: Owner,
}

impl Ctx {
    /// What finds this caller's lease.
    pub fn find(&self) -> FindParams {
        FindParams {
            worktree: self.worktree.clone(),
            owner: self.owner.clone(),
            lease: forced_lease(),
        }
    }
}

pub fn context() -> Result<Ctx, Failure> {
    let cfg = machine_config()?;
    let cwd = std::env::current_dir()
        .map_err(|e| Failure::precondition(format!("cannot read the working directory: {e}")))?;
    let worktree = toplevel(&cwd).unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    let project = project_config(&worktree)?;
    let owner = find_owner(&cfg.owner_names);
    Ok(Ctx {
        cfg,
        worktree,
        project,
        owner,
    })
}

/// The git worktree's top directory, when `cwd` is in one.
fn toplevel(cwd: &Path) -> Option<String> {
    let mut git = Command::new("git");
    git.args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = process::run(git, Duration::from_secs(5)).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn var(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

fn home() -> PathBuf {
    PathBuf::from(var("HOME").unwrap_or_else(|| OsString::from("/")))
}

/// devlab's own state: stack workdirs and logs, `${XDG_STATE_HOME:-~/.local/state}/devlab`.
pub fn devlab_state() -> PathBuf {
    var("XDG_STATE_HOME")
        .map_or_else(|| home().join(".local/state"), PathBuf::from)
        .join("devlab")
}

/// `DEVLAB_LEASE`, when it names a lease.
pub fn forced_lease() -> Option<i64> {
    std::env::var("DEVLAB_LEASE")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|id| *id != 0)
}

pub fn machine_config() -> Result<MachineConfig, Failure> {
    let file = var("XDG_CONFIG_HOME")
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("devlab/config.json");
    match std::fs::read_to_string(&file) {
        Ok(text) => config::machine_config(&file.to_string_lossy(), &text)
            .map_err(|e| Failure::new(Exit::Precondition, e.0)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MachineConfig::default()),
        Err(e) => Err(Failure::precondition(format!("{}: {e}", file.display()))),
    }
}

/// The project whose `devlab.json` is at the root of `worktree`, if any.
pub fn project_config(worktree: &str) -> Result<Option<ProjectConfig>, Failure> {
    let file = Path::new(worktree).join("devlab.json");
    match std::fs::read_to_string(&file) {
        Ok(text) => config::project_config(&file.to_string_lossy(), &text)
            .map(Some)
            .map_err(|e| Failure::new(Exit::Precondition, e.0)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Failure::precondition(format!("{}: {e}", file.display()))),
    }
}
