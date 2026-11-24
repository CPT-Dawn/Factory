//! The write guard an agy review runs under (#49).
//!
//! agy's own terminal sandbox mounts its workspace read-only but leaves the rest of the
//! filesystem writable (docs/agy-sandbox.md), so a review is put inside bubblewrap (`bwrap`)
//! first: the whole filesystem read-only, and write access given back only to the paths the
//! review names (its throwaway checkout and its log) and to agy's own state. Everything stays
//! readable, so git, the bundle, the spec, the evidence and the previous report all read as
//! before.
//!
//! A socket on a read-only mount still connects, so the sockets that could write for a review
//! are hidden too: the runtime dir (`$XDG_RUNTIME_DIR`, with the factory's, herdr's and other
//! sockets) is an empty tmpfs but for the session bus, which agy's login needs for the
//! keyring, and Docker's socket is masked. `/dev/shm` is a private tmpfs. A fresh `/proc`
//! lets agy's own sandbox start its user namespace inside the guard.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The program that puts the guard up.
pub const BWRAP: &str = "bwrap";

/// Sockets outside the runtime dir that are masked with `/dev/null`: Docker's writes anything.
const MASKED: [&str; 1] = ["/run/docker.sock"];

/// `program` under the guard, able to write only to `writable` and agy's own state.
pub fn command(program: &str, writable: &[&Path]) -> Command {
    let mut cmd = Command::new(BWRAP);
    cmd.args(args(&Env::current(), writable)).arg(program);
    cmd
}

/// What the guard reads from the environment.
struct Env {
    home: Option<PathBuf>,
    config_home: Option<PathBuf>,
    cache_home: Option<PathBuf>,
    runtime: Option<PathBuf>,
    bus: Option<PathBuf>,
}

impl Env {
    fn current() -> Env {
        let var = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
        Env {
            home: var("HOME").map(PathBuf::from),
            config_home: var("XDG_CONFIG_HOME").map(PathBuf::from),
            cache_home: var("XDG_CACHE_HOME").map(PathBuf::from),
            runtime: var("XDG_RUNTIME_DIR").map(PathBuf::from),
            bus: var("DBUS_SESSION_BUS_ADDRESS")
                .and_then(|address| bus_path(&address.to_string_lossy())),
        }
    }

    /// Where agy keeps its state: `~/.gemini` (its login, conversations, logs and settings)
    /// and the `antigravity` dirs under the XDG config and cache homes.
    fn agy_state(&self) -> Vec<PathBuf> {
        let Some(home) = &self.home else {
            return Vec::new();
        };
        let config = self
            .config_home
            .clone()
            .unwrap_or_else(|| home.join(".config"));
        let cache = self
            .cache_home
            .clone()
            .unwrap_or_else(|| home.join(".cache"));
        vec![
            home.join(".gemini"),
            config.join("antigravity"),
            cache.join("antigravity"),
        ]
    }

    /// The session bus's socket: the one its address names, else the runtime dir's `bus`.
    fn bus(&self) -> Option<PathBuf> {
        self.bus
            .clone()
            .or_else(|| self.runtime.as_ref().map(|runtime| runtime.join("bus")))
    }
}

/// The socket a `unix:path=...` bus address names.
fn bus_path(address: &str) -> Option<PathBuf> {
    address
        .split(';')
        .find_map(|one| one.strip_prefix("unix:"))
        .and_then(|keys| keys.split(',').find_map(|key| key.strip_prefix("path=")))
        .map(PathBuf::from)
}

/// Where `path` lives once its symlinks are followed, since bwrap can't mount on a symlink: the
/// link reads through the read-only root to the writable mount. A path that isn't there stays
/// as it is, for a `-try` mount to skip.
fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// bwrap's arguments up to and including its `--`. Later mounts win, so the read-only root
/// comes first and what is given back after it.
fn args(env: &Env, writable: &[&Path]) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--ro-bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
        "--tmpfs",
        "/dev/shm",
        "--proc",
        "/proc",
        "--die-with-parent",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut mount = |flag: &str, source: &Path, target: &Path| {
        args.extend([flag.into(), source.into(), target.into()]);
    };
    for socket in MASKED {
        mount("--ro-bind-try", Path::new("/dev/null"), Path::new(socket));
    }
    if let Some(runtime) = &env.runtime {
        args.extend(["--tmpfs".into(), real(runtime).into()]);
    }
    let mut mount = |flag: &str, path: &Path| {
        let path = real(path);
        args.extend([flag.into(), path.clone().into(), path.into()]);
    };
    if let Some(bus) = env.bus() {
        mount("--ro-bind-try", &bus);
    }
    for dir in env.agy_state() {
        mount("--bind-try", &dir);
    }
    for path in writable {
        mount("--bind", path);
    }
    args.push("--".into());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bus_socket_is_the_path_its_address_names() {
        assert_eq!(
            bus_path("unix:path=/run/user/1000/bus,guid=abc"),
            Some(PathBuf::from("/run/user/1000/bus"))
        );
        assert_eq!(
            bus_path("tcp:host=localhost;unix:guid=x,path=/s/bus"),
            Some(PathBuf::from("/s/bus"))
        );
        assert_eq!(bus_path("unix:abstract=/tmp/dbus-x"), None);
    }
}
