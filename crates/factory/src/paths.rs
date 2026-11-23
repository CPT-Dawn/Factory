//! Where the factory keeps its files.
//!
//! State resolves as factory-stats resolves it (`$FACTORY_STATE`, else
//! `${XDG_STATE_HOME:-$HOME/.local/state}/factory`), so both read and write one ledger. The
//! daemon's socket, locks and pid live in `$XDG_RUNTIME_DIR/factory`, and the config in
//! `${XDG_CONFIG_HOME:-$HOME/.config}/factory`.

use std::env;
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::output::Failure;

#[derive(Clone, Debug)]
pub struct Paths {
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub config: PathBuf,
    /// herdr's socket: `$HERDR_SOCKET_PATH`, as herdr sets it in its panes, else herdr's
    /// default `${XDG_CONFIG_HOME:-$HOME/.config}/herdr/herdr.sock`.
    pub herdr: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Paths, Failure> {
        // An empty variable counts as unset, as in factory-stats.
        let var = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
        let home = || var("HOME").unwrap_or_else(|| OsString::from("/"));
        let state = match (var("FACTORY_STATE"), var("XDG_STATE_HOME")) {
            (Some(dir), _) => PathBuf::from(dir),
            (None, Some(dir)) => Path::new(&dir).join("factory"),
            (None, None) => Path::new(&home()).join(".local/state/factory"),
        };
        let herdr = match (var("HERDR_SOCKET_PATH"), var("XDG_CONFIG_HOME")) {
            (Some(socket), _) => PathBuf::from(socket),
            (None, Some(dir)) => Path::new(&dir).join("herdr/herdr.sock"),
            (None, None) => Path::new(&home()).join(".config/herdr/herdr.sock"),
        };
        let config = var("XDG_CONFIG_HOME")
            .map_or_else(|| Path::new(&home()).join(".config"), PathBuf::from)
            .join("factory");
        let runtime = var("XDG_RUNTIME_DIR").ok_or_else(|| {
            Failure::precondition("XDG_RUNTIME_DIR is not set; run factory inside a user session")
        })?;
        Ok(Paths {
            state,
            runtime: Path::new(&runtime).join("factory"),
            config,
            herdr,
        })
    }

    /// The event ledger factory-stats reads.
    pub fn ledger(&self) -> PathBuf {
        self.state.join("events.jsonl")
    }

    /// The lab's lease store, which the daemon alone writes.
    pub fn lab_db(&self) -> PathBuf {
        self.state.join("lab.db")
    }

    /// The daemon's database: reviewer quotas, and later leases and waits.
    pub fn database(&self) -> PathBuf {
        self.state.join("factory.db")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// The final report of the lead `slug` working in `repo`, as lead.sh names it.
    pub fn lead_report(&self, repo: &str, slug: &str) -> PathBuf {
        self.state
            .join("leads")
            .join(repo)
            .join(format!("{slug}.md"))
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("factory.sock")
    }

    /// Held by the running daemon for its whole life, so only one runs.
    pub fn daemon_lock(&self) -> PathBuf {
        self.runtime.join("daemon.lock")
    }

    pub fn daemon_pid(&self) -> PathBuf {
        self.runtime.join("daemon.pid")
    }

    /// The stderr of a daemon the CLI started.
    pub fn daemon_log(&self) -> PathBuf {
        self.runtime.join("daemon.log")
    }

    /// Held by a CLI while it starts the daemon, so only one starts it.
    pub fn start_lock(&self) -> PathBuf {
        self.runtime.join("start.lock")
    }

    /// Creates the runtime dir, readable by the user alone.
    pub fn create_runtime(&self) -> Result<(), Failure> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.runtime)
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot create the runtime dir {}: {e}; check XDG_RUNTIME_DIR",
                    self.runtime.display()
                ))
            })
    }
}
