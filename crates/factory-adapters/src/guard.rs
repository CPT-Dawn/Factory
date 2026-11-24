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
//! are hidden too. The runtime dir (`$XDG_RUNTIME_DIR`, with the factory's, herdr's, systemd's
//! and other sockets) is an empty tmpfs, Docker's socket is masked, and `/dev` is a minimal
//! private one with its own `/dev/shm`. agy's subscription login reads its token from the
//! Secret Service keyring over the session bus, so for it the bus is given back through
//! `xdg-dbus-proxy`, filtered to the keyring alone: systemd's user manager, which would run
//! anything outside the guard, isn't reachable through it. A fresh `/proc` lets agy's own
//! sandbox start its user namespace inside the guard.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};

/// The program that puts the guard up.
pub const BWRAP: &str = "bwrap";
/// The program that filters the session bus for the guard.
pub const PROXY: &str = "xdg-dbus-proxy";
/// The one bus name a guarded program may talk to: the Secret Service, agy's keyring.
const KEYRING: &str = "org.freedesktop.secrets";
/// How long the proxy may take to say it is listening.
const PROXY_READY: Duration = Duration::from_secs(10);

/// Sockets outside the runtime dir that are masked with `/dev/null`: Docker's writes anything.
const MASKED: [&str; 1] = ["/run/docker.sock"];

/// Whether a guarded program reaches the session bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    /// Through the proxy, to the keyring alone: agy's subscription login.
    Keyring,
    /// Not at all.
    Closed,
}

/// A guard that is up: programs run under it with [`Guard::command`] while it lives, and the
/// bus proxy it started, if any, stops when it drops.
pub struct Guard {
    args: Vec<OsString>,
    _proxy: Option<Proxy>,
}

impl Guard {
    /// The guard for a program that may write only to `writable` and agy's own state, and
    /// reaches the bus as `bus` says. `Err` says, in one line, why it can't start.
    pub fn start(writable: &[&Path], bus: Bus) -> Result<Guard, String> {
        let env = Env::current();
        let proxy = match (bus, env.session_bus()) {
            (Bus::Keyring, Some((address, target))) => {
                Some((Proxy::start(&env.socket_dir(), &address)?, target))
            }
            _ => None,
        };
        let args = args(
            &env,
            writable,
            proxy
                .as_ref()
                .map(|(proxy, target)| (proxy.socket.as_path(), target.as_path())),
        );
        Ok(Guard {
            args,
            _proxy: proxy.map(|(proxy, _)| proxy),
        })
    }

    /// `program` under the guard.
    pub fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(BWRAP);
        cmd.args(&self.args).arg(program);
        cmd
    }
}

/// A running `xdg-dbus-proxy`, listening on `socket`, in a process group of its own. It stops
/// when the pipe it was given as `--fd` closes, so it never outlives the factory, and its group
/// is killed when this drops.
struct Proxy {
    child: Child,
    ready: Option<ChildStdout>,
    socket: PathBuf,
    /// Whether its leader is reaped: from then on its ID may name someone else's group.
    reaped: bool,
}

impl Proxy {
    /// A proxy in `dir` for the bus at `address`, once it is listening.
    fn start(dir: &Path, address: &str) -> Result<Proxy, String> {
        static STARTED: AtomicU32 = AtomicU32::new(0);
        let socket = dir.join(format!(
            "factory-guard-{}-{}.bus",
            std::process::id(),
            STARTED.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&socket);
        let mut child = Command::new(PROXY)
            .arg(address)
            .arg(&socket)
            .arg("--filter")
            .arg(format!("--talk={KEYRING}"))
            // It writes a byte to its stdout once it listens, and stops when that closes.
            .arg("--fd=1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => format!("{PROXY} is not installed"),
                _ => format!("{PROXY}: {e}"),
            })?;
        let Some(mut ready) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{PROXY} has no stdout to say it listens on"));
        };
        let (said, heard) = mpsc::channel();
        thread::spawn(move || {
            let mut byte = [0; 1];
            let read = ready.read(&mut byte);
            let _ = said.send((read, ready));
        });
        let mut proxy = Proxy {
            child,
            ready: None,
            socket,
            reaped: false,
        };
        match heard.recv_timeout(PROXY_READY) {
            Ok((Ok(1), ready)) => {
                proxy.ready = Some(ready);
                Ok(proxy)
            }
            Ok(_) => Err(proxy.why_it_stopped()),
            Err(_) => Err(format!(
                "{PROXY} was not listening after {}s",
                PROXY_READY.as_secs()
            )),
        }
    }

    /// Kills the proxy's process group and reaps it, once: a second call only reads the status
    /// back. Its leader is unreaped until the first, so the group's ID is still its own then;
    /// once killed, the wait is short.
    fn stop(&mut self) -> io::Result<std::process::ExitStatus> {
        if !self.reaped
            && let Some(group) = i32::try_from(self.child.id()).ok().and_then(Pid::from_raw)
        {
            let _ = kill_process_group(group, Signal::KILL);
        }
        // Even a wait that fails leaves the ID unsafe to signal: it fails only once the leader
        // is no child of ours to wait for.
        self.reaped = true;
        self.child.wait()
    }

    /// Why a proxy that ended without listening ended, in its own words. It is stopped first,
    /// so neither the wait nor the read of its stderr can hang on it.
    fn why_it_stopped(&mut self) -> String {
        let status = self.stop();
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        match (stderr.lines().find(|line| !line.trim().is_empty()), status) {
            (Some(said), _) => format!("{PROXY}: {}", said.trim()),
            (None, Ok(status)) => format!("{PROXY} ended without listening ({status})"),
            (None, Err(e)) => format!("{PROXY}: {e}"),
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        drop(self.ready.take());
        let _ = self.stop();
        let _ = fs::remove_file(&self.socket);
    }
}

/// What the guard reads from the environment.
struct Env {
    home: Option<PathBuf>,
    config_home: Option<PathBuf>,
    cache_home: Option<PathBuf>,
    runtime: Option<PathBuf>,
    bus: Option<String>,
}

impl Env {
    fn current() -> Env {
        let var = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
        Env {
            home: var("HOME").map(PathBuf::from),
            config_home: var("XDG_CONFIG_HOME").map(PathBuf::from),
            cache_home: var("XDG_CACHE_HOME").map(PathBuf::from),
            runtime: var("XDG_RUNTIME_DIR").map(PathBuf::from),
            bus: var("DBUS_SESSION_BUS_ADDRESS").map(|address| address.to_string_lossy().into()),
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

    /// The session bus's address, and where its socket goes inside the guard: the path its
    /// address names, else the runtime dir's `bus`. None when there is no bus to reach.
    fn session_bus(&self) -> Option<(String, PathBuf)> {
        let default = self.runtime.as_ref().map(|runtime| runtime.join("bus"));
        let address = self.bus.clone().or_else(|| {
            let socket = default.as_ref().filter(|socket| socket.exists())?;
            Some(format!("unix:path={}", socket.display()))
        })?;
        let target = bus_path(&address).or(default)?;
        Some((address, target))
    }

    /// Where the proxy's socket goes: the runtime dir, private to the user.
    fn socket_dir(&self) -> PathBuf {
        self.runtime.clone().unwrap_or_else(env::temp_dir)
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

/// bwrap's arguments up to and including its `--`, with `bus` the proxy's socket and where it
/// goes inside. Later mounts win, so the read-only root comes first and what is given back
/// after it.
fn args(env: &Env, writable: &[&Path], bus: Option<(&Path, &Path)>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--ro-bind",
        "/",
        "/",
        "--dev",
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
    match bus {
        Some((socket, target)) => {
            let target = real(target);
            args.extend([
                "--ro-bind".into(),
                socket.into(),
                target.clone().into(),
                "--setenv".into(),
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={}", target.display()).into(),
            ]);
        }
        None => args.extend(["--unsetenv".into(), "DBUS_SESSION_BUS_ADDRESS".into()]),
    }
    let mut mount = |flag: &str, path: &Path| {
        let path = real(path);
        args.extend([flag.into(), path.clone().into(), path.into()]);
    };
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
