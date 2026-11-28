//! Where the factory keeps its files.
//!
//! State resolves as factory-stats resolves it (`$FACTORY_STATE`, else
//! `${XDG_STATE_HOME:-$HOME/.local/state}/factory`), so both read and write one ledger. The
//! daemon's socket, locks and pid live in `$XDG_RUNTIME_DIR/factory`, and the config in
//! `${XDG_CONFIG_HOME:-$HOME/.config}/factory`. `factory install` puts the binary in
//! `~/.local/bin` and links the units into systemd's user unit dir.

use std::env;
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use factory_core::ci::Watched;

use crate::output::Failure;

/// The daemon's socket, the one systemd's socket unit may hold.
const SOCKET: &str = "factory.sock";
/// The socket only a running daemon binds: no longer a name than [`SOCKET`]'s, so a runtime dir
/// whose socket path fits the kernel's limit fits both.
const LIVE_SOCKET: &str = "live.sock";
const _: () = assert!(LIVE_SOCKET.len() <= SOCKET.len());

#[derive(Clone, Debug)]
pub struct Paths {
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub config: PathBuf,
    /// herdr's socket: `$HERDR_SOCKET_PATH`, as herdr sets it in its panes, else herdr's
    /// default `${XDG_CONFIG_HOME:-$HOME/.config}/herdr/herdr.sock`.
    pub herdr: PathBuf,
    /// Claude Code's config dir, where it logs its sessions: `$CLAUDE_CONFIG_DIR`, else
    /// `$HOME/.claude`.
    pub claude: PathBuf,
    /// Where `factory install` puts the binary: `$HOME/.local/bin`, where the service unit
    /// runs it from.
    pub bin: PathBuf,
    /// systemd's user unit dir: `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user`.
    pub units: PathBuf,
}

/// The config dir, `${XDG_CONFIG_HOME:-$HOME/.config}/factory`: all a hook needs of the paths, since
/// it never talks to the daemon.
pub fn config_dir() -> PathBuf {
    let var = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
    var("XDG_CONFIG_HOME")
        .map_or_else(
            || Path::new(&var("HOME").unwrap_or_else(|| OsString::from("/"))).join(".config"),
            PathBuf::from,
        )
        .join("factory")
}

/// The runtime dir, `$XDG_RUNTIME_DIR/factory`, when the variable is set.
pub fn runtime_dir() -> Option<PathBuf> {
    let dir = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())?;
    Some(Path::new(&dir).join("factory"))
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
        let config_home = var("XDG_CONFIG_HOME")
            .map_or_else(|| Path::new(&home()).join(".config"), PathBuf::from);
        let herdr = var("HERDR_SOCKET_PATH")
            .map_or_else(|| config_home.join("herdr/herdr.sock"), PathBuf::from);
        let config = config_dir();
        let claude = var("CLAUDE_CONFIG_DIR")
            .map_or_else(|| Path::new(&home()).join(".claude"), PathBuf::from);
        let runtime = runtime_dir().ok_or_else(|| {
            Failure::precondition("XDG_RUNTIME_DIR is not set; run factory inside a user session")
        })?;
        Ok(Paths {
            state,
            runtime,
            config,
            herdr,
            claude,
            bin: Path::new(&home()).join(".local/bin"),
            units: config_home.join("systemd/user"),
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

    /// `factory quota agy`'s listing of every quota group, when it is too long for the line.
    pub fn agy_quota(&self) -> PathBuf {
        self.state.join("agy-quota.txt")
    }

    /// The failed checks of a pull request's or branch's head, with their URLs and log tails:
    /// one file per head, so a late read of an old head never overwrites a newer one's. A
    /// branch's `/` is a `-` there, as a number never is.
    pub fn ci_failures(&self, pr: &Watched, head: &str) -> PathBuf {
        let name = match pr {
            Watched::Pull(pr) => pr.number.to_string(),
            Watched::Branch(branch) => branch.name.replace('/', "-"),
        };
        self.state
            .join("ci")
            .join(pr.host())
            .join(pr.owner())
            .join(pr.repo())
            .join(format!("{name}-{}.md", head.get(..12).unwrap_or(head)))
    }

    /// Where Claude Code keeps one directory of session logs per working directory.
    pub fn claude_projects(&self) -> PathBuf {
        self.claude.join("projects")
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
        self.runtime.join(SOCKET)
    }

    /// The socket the running daemon binds itself, beside the one systemd may hold: a call here
    /// reaches a daemon that runs, or nothing, and never makes systemd start one.
    pub fn live_socket(&self) -> PathBuf {
        self.runtime.join(LIVE_SOCKET)
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
