//! `factory install` and `factory uninstall`: the binary on PATH, the socket-activated user
//! units DotFiles keeps linked into systemd's user unit dir, and systemd told.
//!
//! The units are linked the way stow links them (relative symlinks), so DotFiles' `dots link`
//! and this command agree on what is installed. Once the socket listens, the first factory call
//! starts the daemon, and a restart or upgrade loses nothing: calls wait in the socket.

use std::env;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use factory_adapters::systemd::{Systemctl, UserManager};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::json;

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;

/// The units, socket first.
const UNITS: [&str; 2] = [SOCKET, SERVICE];
const SOCKET: &str = "factory.socket";
const SERVICE: &str = "factory.service";
/// How long a daemon the CLI started may take to answer what it holds and stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

pub fn install(units: Option<&Path>) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    match units {
        Some(dir) => link_units(dir, &paths.units)?,
        None => units_in_place(&paths.units)?,
    }
    let upgraded = paths.bin.join("factory").exists();
    let binary = install_binary(&paths)?;
    let systemd = Systemctl;
    systemd.reload().map_err(told)?;
    // The daemon systemd runs stops; the socket starts the new binary on the next call.
    systemd.stop(&[SERVICE]).map_err(told)?;
    {
        // No CLI starts a daemon of its own between the old daemon going and the socket
        // listening: they queue on the start lock, then find the socket.
        paths.create_runtime()?;
        let _start_lock = client::take_start_lock(&paths).map_err(Failure::precondition)?;
        stop_cli_daemon(&paths, "install")?;
        systemd.enable_now(SOCKET).map_err(told)?;
    }
    let socket = paths.socket();
    Ok(Done::new(
        format!(
            "{} factory at {}; {SOCKET} listens on {}",
            if upgraded { "upgraded" } else { "installed" },
            binary.display(),
            socket.display()
        ),
        json!({
            "binary": binary.to_string_lossy(),
            "upgraded": upgraded,
            "units": UNITS.map(|unit| paths.units.join(unit).to_string_lossy().into_owned()),
            "socket": socket.to_string_lossy(),
        }),
    ))
}

pub fn uninstall() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let present: Vec<PathBuf> = UNITS
        .iter()
        .map(|unit| paths.units.join(unit))
        .filter(|unit| fs::symlink_metadata(unit).is_ok())
        .collect();
    if !present.is_empty() {
        let systemd = Systemctl;
        systemd.stop(&UNITS).map_err(told_uninstall)?;
        systemd.disable(SOCKET).map_err(told_uninstall)?;
        for unit in &present {
            remove(unit)?;
        }
        systemd.reload().map_err(told_uninstall)?;
    }
    stop_cli_daemon(&paths, "uninstall")?;
    let binary = paths.bin.join("factory");
    remove(&binary)?;
    Ok(Done::new(
        format!(
            "uninstalled factory: removed {} and its units; left {} alone",
            binary.display(),
            paths.state.display()
        ),
        json!({
            "binary": binary.to_string_lossy(),
            "units": present.iter().map(|unit| unit.to_string_lossy()).collect::<Vec<_>>(),
            "state": paths.state.to_string_lossy(),
        }),
    ))
}

/// Removes a file or link; one that isn't there is already removed.
fn remove(path: &Path) -> Result<(), Failure> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Failure::precondition(format!(
            "cannot remove {}: {e}; remove it, then run factory uninstall again",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn told_uninstall(e: impl std::fmt::Display) -> Failure {
    Failure::precondition(format!("{e}; fix it and run factory uninstall again"))
}

/// Stops a daemon a CLI started outside systemd: it holds the daemon lock and a socket of its
/// own, so the daemon the socket unit starts could not run beside it.
fn stop_cli_daemon(paths: &Paths, command: &str) -> Result<(), Failure> {
    let lock_path = paths.daemon_lock();
    let again = format!("then run factory {command} again");
    let cannot_lock = |e: &dyn std::fmt::Display| {
        Failure::precondition(format!(
            "cannot lock {}: {e}; check XDG_RUNTIME_DIR, {again}",
            lock_path.display()
        ))
    };
    // No lock file: no daemon has run in this session.
    let Ok(lock) = File::options().write(true).open(&lock_path) else {
        return Ok(());
    };
    match lock.try_lock() {
        Ok(()) => return Ok(()),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(e)) => return Err(cannot_lock(&e)),
    }
    let raw = fs::read_to_string(paths.daemon_pid())
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok());
    let Some((raw, pid)) = raw.and_then(|raw| Some((raw, Pid::from_raw(raw)?))) else {
        return Err(Failure::precondition(format!(
            "a daemon holds {} but {} names no pid; stop it, {again}",
            lock_path.display(),
            paths.daemon_pid().display()
        )));
    };
    kill_process(pid, Signal::TERM).map_err(|e| {
        Failure::precondition(format!(
            "cannot stop the daemon (pid {raw}): {e}; stop it, {again}"
        ))
    })?;
    match client::within(STOP_TIMEOUT, move || lock.lock()) {
        Some(Ok(())) => {}
        Some(Err(e)) => return Err(cannot_lock(&e)),
        None => {
            return Err(Failure::precondition(format!(
                "the daemon (pid {raw}) did not stop within {}s; kill it, {again}",
                STOP_TIMEOUT.as_secs()
            )));
        }
    }
    // Its socket stays behind; systemd's takes the path.
    match fs::remove_file(paths.socket()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Failure::precondition(format!(
            "cannot remove the old daemon's socket {}: {e}; remove it, {again}",
            paths.socket().display()
        ))),
        _ => Ok(()),
    }
}

fn told(e: impl std::fmt::Display) -> Failure {
    Failure::precondition(format!("{e}; fix it and run factory install again"))
}

/// Copies this binary to `~/.local/bin/factory` through a rename, so a daemon running the old
/// one keeps running until it is restarted.
fn install_binary(paths: &Paths) -> Result<PathBuf, Failure> {
    let target = paths.bin.join("factory");
    let exe = env::current_exe()
        .map_err(|e| Failure::precondition(format!("cannot find this factory binary: {e}")))?;
    let cannot = |e: io::Error| {
        Failure::precondition(format!(
            "cannot install {}: {e}; check that {} is writable",
            target.display(),
            paths.bin.display()
        ))
    };
    fs::create_dir_all(&paths.bin).map_err(cannot)?;
    if same_file(&exe, &target) {
        return Ok(target);
    }
    let partial = paths
        .bin
        .join(format!(".factory.{}.partial", std::process::id()));
    fs::copy(&exe, &partial)
        .and_then(|_| fs::rename(&partial, &target))
        .map_err(|e| {
            let _ = fs::remove_file(&partial);
            cannot(e)
        })?;
    Ok(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Checks that each unit is already in `unit_dir`, as `dots link` puts them.
fn units_in_place(unit_dir: &Path) -> Result<(), Failure> {
    match UNITS
        .into_iter()
        .find(|unit| !unit_dir.join(unit).is_file())
    {
        None => Ok(()),
        Some(unit) => Err(Failure::precondition(format!(
            "no {unit} in {}; pass --units with the dir holding DotFiles' factory units, or run dots link",
            unit_dir.display()
        ))),
    }
}

/// Links each unit in `from` into `unit_dir` with a relative symlink, as stow would.
fn link_units(from: &Path, unit_dir: &Path) -> Result<(), Failure> {
    let cannot = |what: &Path, e: io::Error| {
        Failure::precondition(format!(
            "cannot link {}: {e}; check that {} is writable",
            what.display(),
            unit_dir.display()
        ))
    };
    fs::create_dir_all(unit_dir).map_err(|e| cannot(unit_dir, e))?;
    let unit_dir_real = fs::canonicalize(unit_dir).map_err(|e| cannot(unit_dir, e))?;
    for unit in UNITS {
        let source = fs::canonicalize(from.join(unit)).map_err(|e| {
            Failure::precondition(format!(
                "no {unit} in {}: {e}; pass --units the dir holding DotFiles' factory units",
                from.display()
            ))
        })?;
        let link = unit_dir.join(unit);
        let target = relative(&unit_dir_real, &source);
        if fs::read_link(&link).is_ok_and(|existing| existing == target) {
            continue;
        }
        let partial = unit_dir.join(format!(".{unit}.{}.partial", std::process::id()));
        std::os::unix::fs::symlink(&target, &partial)
            .and_then(|()| fs::rename(&partial, &link))
            .map_err(|e| {
                let _ = fs::remove_file(&partial);
                cannot(&link, e)
            })?;
    }
    Ok(())
}

/// The path from the directory `from` to `to`, both absolute and free of `..`.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let up = std::iter::repeat_n(Component::ParentDir, from.len() - common);
    up.chain(to[common..].iter().copied()).collect()
}
