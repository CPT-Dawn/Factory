//! `factory install` and `factory uninstall`: the binary on PATH, DotFiles' socket-activated
//! units linked into systemd's user unit dir, and systemd told.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use rustix::process::Signal;
use serde_json::Value;

use crate::support::socket_unit::SocketUnit;
use crate::support::{Sandbox, kill_group, run, world};

/// The units as DotFiles keeps them, in a stow package under the sandbox.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn dotfiles_units(sandbox: &Sandbox) -> PathBuf {
    let dir = sandbox.path("dots/factory/.config/systemd/user");
    fs::create_dir_all(&dir).unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/units");
    for unit in ["factory.socket", "factory.service"] {
        fs::copy(fixtures.join(unit), dir.join(unit)).unwrap();
    }
    dir
}

/// A sandbox with factory installed from DotFiles' units.
#[cfg(test)]
fn installed() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);
    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);
    assert_eq!(out.code, 0, "{out:?}");
    sandbox
}

/// What the fake systemctl was asked, one call a line.
#[cfg(test)]
fn calls(sandbox: &Sandbox) -> Vec<String> {
    sandbox
        .calls("systemctl")
        .iter()
        .map(|call| call.join(" "))
        .collect()
}

#[test]
fn install_puts_factory_on_path_links_the_units_and_starts_the_socket() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stderr, "");
    let binary = sandbox.path("home/.local/bin/factory");
    assert_eq!(
        out.stdout_lines(),
        vec![format!(
            "installed factory at {}; factory.socket listens on {}",
            binary.display(),
            sandbox.path("run/factory/factory.sock").display()
        )]
    );
    let installed = run(sandbox.command(&binary, &["--version"]));
    assert_eq!(installed.stdout, sandbox.factory(&["--version"]).stdout);
    for unit in ["factory.socket", "factory.service"] {
        let link = sandbox.path("config/systemd/user").join(unit);
        // Relative, as stow links, so `dots link` takes the link for its own.
        let target = fs::read_link(&link).unwrap();
        assert!(target.is_relative(), "{unit} links to {target:?}");
        assert_eq!(
            fs::canonicalize(&link).unwrap(),
            fs::canonicalize(units.join(unit)).unwrap()
        );
    }
    assert_eq!(
        calls(&sandbox),
        vec![
            "--user daemon-reload",
            "--user stop factory.service",
            "--user enable --now factory.socket"
        ]
    );
}

#[test]
fn install_leaves_the_devlab_name_alone() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);

    assert_eq!(out.code, 0, "{out:?}");
    let bin: Vec<_> = fs::read_dir(sandbox.path("home/.local/bin"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(bin, vec!["factory"]);
}

#[test]
fn with_the_units_installed_the_first_log_starts_the_daemon_through_the_socket() {
    let sandbox = installed();
    let socket = SocketUnit::from_installed(&sandbox);
    assert_eq!(socket.activations(), 0);

    let out = sandbox.factory(&["log", "review", "repo=X"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(sandbox.ledger_lines().len(), 1);
    assert_eq!(socket.activations(), 1);
}

#[test]
fn restarting_the_daemon_mid_run_loses_no_ledger_line() {
    let sandbox = installed();
    let socket = SocketUnit::from_installed(&sandbox);
    let restarted = AtomicBool::new(false);

    // Eight loggers log one event after another while the daemon is stopped twenty times, and
    // log two more each once it has been.
    let logged: Vec<(usize, usize)> = thread::scope(|scope| {
        let loggers: Vec<_> = (0..8)
            .map(|w| {
                let (sandbox, restarted) = (&sandbox, &restarted);
                scope.spawn(move || {
                    let mut after = 0;
                    let mut n = 0;
                    while after < 2 {
                        if restarted.load(Ordering::SeqCst) {
                            after += 1;
                        }
                        let out = sandbox.factory(&[
                            "log",
                            "review",
                            &format!("w={w}"),
                            &format!("n={n}"),
                        ]);
                        assert_eq!(out.code, 0, "logger {w}, event {n}: {out:?}");
                        n += 1;
                    }
                    (0..n).map(move |n| (w, n))
                })
            })
            .collect();
        for _ in 0..20 {
            socket.stop_daemon();
        }
        restarted.store(true, Ordering::SeqCst);
        loggers
            .into_iter()
            .flat_map(|logger| logger.join().unwrap())
            .collect()
    });

    let mut landed: Vec<(usize, usize)> = sandbox
        .ledger_lines()
        .iter()
        .map(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            let field = |key: &str| event[key].as_str().unwrap().parse::<usize>().unwrap();
            (field("w"), field("n"))
        })
        .collect();
    landed.sort_unstable();
    assert_eq!(landed, logged);
    assert!(
        socket.activations() >= 21,
        "{} activations",
        socket.activations()
    );
}

#[test]
fn install_without_units_uses_the_ones_dots_link_put_in_place() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);
    let unit_dir = sandbox.path("config/systemd/user");
    fs::create_dir_all(&unit_dir).unwrap();
    for unit in ["factory.socket", "factory.service"] {
        let stowed = PathBuf::from("../../../dots/factory/.config/systemd/user").join(unit);
        std::os::unix::fs::symlink(stowed, unit_dir.join(unit)).unwrap();
    }

    let out = sandbox.factory(&["install"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        fs::read_to_string(unit_dir.join("factory.socket")).unwrap(),
        fs::read_to_string(units.join("factory.socket")).unwrap()
    );
    assert_eq!(calls(&sandbox).len(), 3, "{:?}", calls(&sandbox));
}

#[test]
fn install_refuses_in_one_line_when_it_finds_no_units() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let empty = sandbox.path("empty");
    fs::create_dir(&empty).unwrap();

    for args in [
        &["install"][..],
        &["install", "--units", empty.to_str().unwrap()],
    ] {
        let out = sandbox.factory(args);

        assert_eq!(out.code, 3, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr_lines().len(), 1, "{args:?}: {out:?}");
        assert!(out.stderr.contains("factory.socket"), "{out:?}");
        assert!(out.stderr.contains("--units"), "{out:?}");
    }
    assert!(!sandbox.path("home/.local/bin/factory").exists());
    assert!(calls(&sandbox).is_empty(), "{:?}", calls(&sandbox));
}

#[test]
fn install_over_an_older_factory_upgrades_it() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);
    let binary = sandbox.path("home/.local/bin/factory");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "#!/bin/sh\necho factory 0.0.0\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stdout
            .starts_with(&format!("upgraded factory at {};", binary.display())),
        "{out:?}"
    );
    let installed = run(sandbox.command(&binary, &["--version"]));
    assert_eq!(installed.stdout, sandbox.factory(&["--version"]).stdout);
    // The running daemon stops, so the socket starts the new binary on the next call.
    assert!(calls(&sandbox).contains(&"--user stop factory.service".to_owned()));
}

#[test]
fn install_says_which_systemctl_call_failed() {
    let sandbox = Sandbox::new();
    sandbox.fake(
        "systemctl",
        "[ \"$2\" = daemon-reload ] && { echo 'Failed to connect to bus: No medium found' >&2; exit 1; }\nexit 0",
    );
    let units = dotfiles_units(&sandbox);

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "factory: systemctl --user daemon-reload failed: Failed to connect to bus: No medium found; \
         fix it and run factory install again\n"
    );
}

#[test]
fn install_stops_a_daemon_the_cli_started_so_the_socket_starts_the_next() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);
    // Before the units, the first call starts a daemon of its own.
    assert_eq!(sandbox.factory(&["log", "review", "n=1"]).code, 0);

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);

    assert_eq!(out.code, 0, "{out:?}");
    let socket = SocketUnit::from_installed(&sandbox);
    let out = sandbox.factory(&["log", "review", "n=2"]);
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(socket.activations(), 1);
    assert_eq!(sandbox.ledger_lines().len(), 2);
}

#[test]
fn uninstall_removes_the_units_and_the_binary_and_leaves_the_state_alone() {
    let sandbox = installed();
    assert_eq!(sandbox.factory(&["log", "review", "n=1"]).code, 0);
    let before = calls(&sandbox).len();

    let out = sandbox.factory(&["uninstall"]);

    assert_eq!(out.code, 0, "{out:?}");
    let binary = sandbox.path("home/.local/bin/factory");
    assert_eq!(
        out.stdout_lines(),
        vec![format!(
            "uninstalled factory: removed {} and its units; left {} alone",
            binary.display(),
            sandbox.path("state/factory").display()
        )]
    );
    assert!(fs::symlink_metadata(&binary).is_err());
    for unit in ["factory.socket", "factory.service"] {
        let link = sandbox.path("config/systemd/user").join(unit);
        assert!(
            fs::symlink_metadata(&link).is_err(),
            "{unit} is still there"
        );
    }
    assert_eq!(
        calls(&sandbox)[before..],
        [
            "--user stop factory.socket factory.service",
            "--user disable factory.socket",
            "--user daemon-reload"
        ]
    );
    assert_eq!(sandbox.ledger_lines().len(), 1);
}

#[test]
fn uninstall_with_nothing_installed_asks_systemd_nothing() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["uninstall"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(calls(&sandbox).is_empty(), "{:?}", calls(&sandbox));
}

#[test]
fn uninstall_stops_a_daemon_the_cli_started() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.factory(&["log", "review", "n=1"]).code, 0);

    let out = sandbox.factory(&["uninstall"]);

    assert_eq!(out.code, 0, "{out:?}");
    // A daemon holds its lock as long as it runs.
    let lock = fs::File::options()
        .write(true)
        .open(sandbox.path("run/factory/daemon.lock"))
        .unwrap();
    assert!(lock.try_lock().is_ok(), "the daemon still runs");
    assert_eq!(sandbox.ledger_lines().len(), 1);
}

#[test]
fn an_upgrade_while_the_socket_listens_keeps_the_socket() {
    let sandbox = installed();
    let socket = SocketUnit::from_installed(&sandbox);
    // A call that came in as the old daemon stopped: the socket started a daemon already.
    assert_eq!(sandbox.factory(&["log", "review", "n=1"]).code, 0);
    let units = sandbox.path("dots/factory/.config/systemd/user");

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.starts_with("upgraded factory at "), "{out:?}");
    assert_eq!(sandbox.factory(&["log", "review", "n=2"]).code, 0);
    assert_eq!(
        socket.activations(),
        2,
        "the next call did not come through the socket"
    );
    assert_eq!(sandbox.ledger_lines().len(), 2);
}

#[test]
fn install_and_uninstall_answer_in_json() {
    let sandbox = Sandbox::new();
    sandbox.fake("systemctl", "exit 0");
    let units = dotfiles_units(&sandbox);
    let binary = sandbox.path("home/.local/bin/factory");

    let out = sandbox.factory(&["install", "--units", units.to_str().unwrap(), "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = out.stdout_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    let done: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(done["binary"], binary.to_str().unwrap());
    assert_eq!(done["upgraded"], false);
    assert_eq!(
        done["socket"],
        sandbox.path("run/factory/factory.sock").to_str().unwrap()
    );
    assert_eq!(done["units"].as_array().map(Vec::len), Some(2));

    let out = sandbox.factory(&["uninstall", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = out.stdout_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    let done: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(done["binary"], binary.to_str().unwrap());
    assert_eq!(done["units"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        done["state"],
        sandbox.path("state/factory").to_str().unwrap()
    );
}

#[test]
fn a_call_while_a_daemon_the_cli_started_stops_waits_and_starts_the_next() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.factory(&["log", "review", "n=1"]).code, 0);
    // A caller that connected and has not asked yet keeps the stopping daemon draining.
    let socket = sandbox.path("run/factory/factory.sock");
    let quiet = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).unwrap();
    kill_group(pid.trim().parse().unwrap(), Signal::TERM);
    assert!(
        world::poll_until(|| std::os::unix::net::UnixStream::connect(&socket).is_err()),
        "the daemon never stopped listening"
    );

    let out = sandbox.factory(&["log", "review", "n=2"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(sandbox.ledger_lines().len(), 2);
    drop(quiet);
}
