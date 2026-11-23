//! `factory cutover` and `factory cutover --rollback` (#20): factory lab takes the `devlab`
//! name on PATH from today's TypeScript devlab, and gives it back.
//!
//! Today's devlab is a fake here: a script in a stow package under the sandbox, linked into
//! ~/.local/bin the way stow links it, with its watcher unit linked into systemd's user dir. Like
//! the real one once DotFiles' cutover refusal is in, it refuses every call but `help` while the
//! cutover's marker is in its state dir.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::thread;

use rusqlite::Connection;
use serde_json::Value;

use crate::devlab_golden::transcript;
use crate::support::golden;
use crate::support::world::World;
use crate::support::{Run, Sandbox, run};

/// What today's devlab says to every call once it was cut over.
const REFUSAL: &str = "devlab: this devlab was cut over to factory lab; run devlab from ~/.local/bin, or roll back with factory cutover --rollback\n";
/// ~/.local/bin/devlab as stow links it: relative, into the devlab package.
const OLD: &str = "../../../dots/devlab/.local/bin/devlab";
const WATCHER: &str = "devlab-watch.service";
const WATCHER_LINK: &str = "../../../dots/devlab/.config/systemd/user/devlab-watch.service";
/// `devlab status --json` with nothing held.
const FREE: &str = r#"{"devices":[],"slots":[],"owners":[],"waiting":[],"watcher":"active","memAvailableMB":1000}"#;
/// `devlab status --json` with a lease on a phone and a slot, and a request waiting.
const LEASED: &str = r#"{
  "devices": [
    {"serial": "FAKEPHONE01", "kind": "phone", "model": "Pixel_8", "state": "ready", "holder": "lease 7 demo-a (claude)"},
    {"serial": "emulator-5580", "kind": "emulator", "model": "", "state": "ready", "holder": "idle 3m"}
  ],
  "slots": [{"project": "demo", "slot": 0, "stack": "down", "holder": "lease 7 demo-a", "last": "demo-a"}],
  "owners": [{"project": "demo", "owner": "claude", "held": 1, "cap": 1, "maxSlots": 2}],
  "waiting": ["claude @ demo-b for 2m (owner at its slot cap)"],
  "watcher": "active",
  "memAvailableMB": 1000
}"#;

/// The user's systemd, as far as the watcher needs: a unit is enabled while its `[Install]`
/// link is in `default.target.wants` and active from `enable --now` until `stop`. Like real
/// systemd, `disable` deletes a unit file that is a link (stow's, here) along with its
/// `[Install]` link, and `enable` fails for a unit whose file is gone. A call whose verb is the
/// one in `systemctl-fails` fails; any other call works.
const SYSTEMCTL: &str = r#"units='@UNITS@'; active='@ACTIVE@'
case "$2" in
is-active) if [ -e "$active/$3" ]; then echo active; exit 0; fi; echo inactive; exit 3 ;;
is-enabled)
  if [ ! -e "$units/$3" ]; then echo not-found; exit 4; fi
  if [ -L "$units/default.target.wants/$3" ]; then echo enabled; exit 0; fi
  echo disabled; exit 1 ;;
esac
if [ "$2" = "$(cat '@FAILS@' 2>/dev/null)" ]; then echo "Failed to $2 unit: Access denied" >&2; exit 1; fi
case "$2" in
disable)
  rm -f "$units/default.target.wants/$3"
  if [ -L "$units/$3" ]; then rm -f "$units/$3"; fi ;;
stop) shift 2; for unit in "$@"; do rm -f "$active/$unit"; done ;;
enable)
  shift 2; now=; if [ "$1" = --now ]; then now=1; shift; fi
  if [ ! -e "$units/$1" ]; then echo "Failed to enable unit: Unit file $1 does not exist." >&2; exit 1; fi
  mkdir -p "$units/default.target.wants" "$active"
  ln -sfn "$units/$1" "$units/default.target.wants/$1"
  if [ -n "$now" ]; then touch "$active/$1"; fi ;;
esac
exit 0"#;

/// A machine before the cutover: factory installed, today's devlab on PATH with its watcher
/// unit and an empty ledger. `honours_marker` is false for a devlab from before DotFiles'
/// cutover refusal.
fn machine_with(honours_marker: bool) -> (Sandbox, World) {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    let fake = SYSTEMCTL
        .replace(
            "@FAILS@",
            &sandbox.path("systemctl-fails").display().to_string(),
        )
        .replace(
            "@UNITS@",
            &sandbox.path("config/systemd/user").display().to_string(),
        )
        .replace(
            "@ACTIVE@",
            &sandbox.path("systemd-active").display().to_string(),
        );
    sandbox.fake("systemctl", &fake);

    let units = sandbox.path("dots/factory/.config/systemd/user");
    fs::create_dir_all(&units).unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/units");
    for unit in ["factory.socket", "factory.service"] {
        fs::copy(fixtures.join(unit), units.join(unit)).unwrap();
    }
    let install = sandbox.factory(&["install", "--units", units.to_str().unwrap()]);
    assert_eq!(install.code, 0, "{install:?}");

    let script = sandbox.path("dots/devlab/.local/bin/devlab");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    let refuse = if honours_marker {
        format!(
            "if [ -e \"$XDG_STATE_HOME/devlab/cutover.json\" ] && [ \"$1\" != help ]; then\n  \
             printf '%s' '{REFUSAL}' >&2\n  exit 3\nfi\n"
        )
    } else {
        String::new()
    };
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n{refuse}case \"$1\" in\n\
             status) cat '{status}' ;;\n\
             *) echo \"devlab: today's devlab ran $1\" >&2 ;;\n\
             esac\n",
            status = sandbox.path("old-status.json").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(sandbox.path("old-status.json"), FREE).unwrap();
    symlink(OLD, sandbox.path("home/.local/bin/devlab")).unwrap();

    let unit = sandbox.path("dots/devlab/.config/systemd/user/devlab-watch.service");
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "[Service]\nExecStart=%h/.local/bin/devlab watch\n").unwrap();
    symlink(WATCHER_LINK, watcher_unit(&sandbox)).unwrap();
    // Enabled and active, as the fake systemd keeps them.
    let wants = sandbox.path("config/systemd/user/default.target.wants");
    fs::create_dir_all(&wants).unwrap();
    symlink(watcher_unit(&sandbox), wants.join(WATCHER)).unwrap();
    fs::create_dir_all(sandbox.path("systemd-active")).unwrap();
    fs::write(sandbox.path("systemd-active").join(WATCHER), "").unwrap();

    ledger(&sandbox);
    (sandbox, world)
}

fn machine() -> (Sandbox, World) {
    machine_with(true)
}

/// Today's devlab's ledger, with its tables as it creates them and nothing in them.
fn ledger(sandbox: &Sandbox) -> Connection {
    fs::create_dir_all(sandbox.path("state/devlab")).unwrap();
    let db = Connection::open(sandbox.path("state/devlab/devlab.db")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS leases (
           id INTEGER PRIMARY KEY, project TEXT, worktree TEXT NOT NULL,
           owner_pid INTEGER NOT NULL, owner_start TEXT NOT NULL, owner_label TEXT NOT NULL,
           slot INTEGER, created INTEGER NOT NULL, touched INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS waiters (
           id INTEGER PRIMARY KEY, pid INTEGER NOT NULL, start TEXT NOT NULL, created INTEGER NOT NULL,
           label TEXT NOT NULL DEFAULT '', owner TEXT NOT NULL DEFAULT '',
           owner_label TEXT NOT NULL DEFAULT '', project TEXT, slot_cap INTEGER,
           max_slots INTEGER NOT NULL DEFAULT 0);",
    )
    .unwrap();
    db
}

/// The watcher's unit in systemd's user unit dir, a link into the devlab package.
fn watcher_unit(sandbox: &Sandbox) -> PathBuf {
    sandbox.path("config/systemd/user").join(WATCHER)
}

/// What the user's systemd says of the watcher: `is-enabled`, then `is-active`.
fn watcher_state(sandbox: &Sandbox) -> (String, String) {
    let ask = |verb: &str| {
        run(sandbox.command("systemctl", &["--user", verb, WATCHER]))
            .stdout
            .trim()
            .to_owned()
    };
    (ask("is-enabled"), ask("is-active"))
}

fn link(sandbox: &Sandbox) -> PathBuf {
    sandbox.path("home/.local/bin/devlab")
}

/// Today's devlab, called by its own path, as a caller with a stale PATH would.
fn old_devlab(sandbox: &Sandbox, args: &[&str]) -> Run {
    run(sandbox.command(sandbox.path("dots/devlab/.local/bin/devlab"), args))
}

/// The `devlab` on the sandbox's PATH, in `dir`, with the test process as its owner.
fn devlab_on_path(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Run {
    let mut cmd = sandbox.command(link(sandbox), args);
    cmd.current_dir(dir)
        .env("DEVLAB_OWNER_PID", std::process::id().to_string());
    run(cmd)
}

/// What systemctl was asked about the watcher, one call a line.
fn watcher_calls(sandbox: &Sandbox) -> Vec<String> {
    sandbox
        .calls("systemctl")
        .iter()
        .map(|call| call.join(" "))
        .filter(|call| call.contains("devlab-watch"))
        .collect()
}

/// Today's devlab still has the name, still answers, and its watcher was never touched.
fn assert_not_cut_over(sandbox: &Sandbox) {
    assert_eq!(fs::read_link(link(sandbox)).unwrap(), PathBuf::from(OLD));
    let answers = old_devlab(sandbox, &["gc"]);
    assert_eq!(answers.code, 0, "{answers:?}");
    assert_eq!(watcher_calls(sandbox), Vec::<String>::new());
}

fn cut_over(sandbox: &Sandbox) {
    let out = sandbox.factory(&["cutover"]);
    assert_eq!(out.code, 0, "{out:?}");
}

#[test]
fn the_cutover_refuses_in_one_line_while_devlab_status_shows_a_lease() {
    let (sandbox, _world) = machine();
    fs::write(sandbox.path("old-status.json"), LEASED).unwrap();

    let out = sandbox.factory(&["cutover"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            75,
            "",
            "factory: devlab status shows lease 7 demo-a (claude), waiting claude @ demo-b for 2m (owner at its slot cap); cut over once devlab status shows no lease\n"
        )
    );
    assert_not_cut_over(&sandbox);
}

#[test]
fn a_lease_in_the_ledger_that_status_did_not_show_still_refuses_the_cutover() {
    let (sandbox, _world) = machine();
    ledger(&sandbox)
        .execute(
            "INSERT INTO leases (id, project, worktree, owner_pid, owner_start, owner_label, slot, created, touched)
             VALUES (9, NULL, '/wt/plain-c', 1, 'x', 'codex', NULL, 0, 0)",
            [],
        )
        .unwrap();

    let out = sandbox.factory(&["cutover"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            75,
            "",
            "factory: devlab.db holds lease 9 plain-c (codex), which devlab status did not show; cut over once devlab status shows no lease\n"
        )
    );
    assert_not_cut_over(&sandbox);
}

#[test]
fn the_cutover_hands_devlab_to_factory_and_retires_the_watcher() {
    let (sandbox, _world) = machine();

    let out = sandbox.factory(&["cutover"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    let devlab = link(&sandbox);
    assert_eq!(
        out.stdout_lines(),
        vec![format!(
            "cut over: {} runs factory lab now; devlab-watch.service is stopped and disabled; roll back with factory cutover --rollback",
            devlab.display()
        )]
    );
    assert_eq!(fs::read_link(&devlab).unwrap(), PathBuf::from("factory"));
    assert_eq!(
        fs::canonicalize(&devlab).unwrap(),
        fs::canonicalize(sandbox.path("home/.local/bin/factory")).unwrap()
    );
    assert_eq!(
        watcher_calls(&sandbox),
        vec![
            "--user disable devlab-watch.service",
            "--user stop devlab-watch.service"
        ]
    );
    // Today's devlab, by any path, leases nothing now.
    for args in [&["status"][..], &["acquire", "--no-device"], &["watch"]] {
        let old = old_devlab(&sandbox, args);
        assert_eq!((old.code, old.stderr.as_str()), (3, REFUSAL), "{args:?}");
    }

    let again = sandbox.factory(&["cutover"]);
    assert_eq!(
        (again.code, again.stdout.as_str(), again.stderr.as_str()),
        (
            0,
            format!(
                "already cut over: {} runs factory lab; roll back with factory cutover --rollback\n",
                devlab.display()
            )
            .as_str(),
            ""
        )
    );
    assert_eq!(watcher_calls(&sandbox).len(), 2);
}

#[test]
fn the_cutover_says_what_it_did_in_json() {
    let (sandbox, _world) = machine();

    let out = sandbox.factory(&["cutover", "--json"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    let json: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "devlab": link(&sandbox).to_string_lossy(),
            "factory": sandbox.path("home/.local/bin/factory").to_string_lossy(),
            "was": OLD,
            "watcher": "stopped and disabled",
        })
    );
}

#[test]
fn after_the_cutover_devlab_on_path_passes_every_golden_transcript() {
    let (sandbox, _world) = machine();
    cut_over(&sandbox);
    let devlab = link(&sandbox);

    let failures: Vec<String> = thread::scope(|scope| {
        let replays: Vec<_> = transcript::ALL
            .iter()
            .map(|file| {
                let devlab = &devlab;
                scope.spawn(move || {
                    golden::replay(&golden::fixtures().join(file), devlab, false).err()
                })
            })
            .collect();
        replays
            .into_iter()
            .filter_map(|replay| replay.join().unwrap())
            .collect()
    });

    assert_eq!(transcript::ALL.len(), 14);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rollback_gives_devlab_back_and_starts_its_watcher_again() {
    let (sandbox, _world) = machine();
    cut_over(&sandbox);

    let out = sandbox.factory(&["cutover", "--rollback"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    let devlab = link(&sandbox);
    assert_eq!(
        out.stdout_lines(),
        vec![format!(
            "rolled back: {} runs today's devlab again and devlab-watch.service is enabled; cut over again with factory cutover",
            devlab.display()
        )]
    );
    assert_eq!(fs::read_link(&devlab).unwrap(), PathBuf::from(OLD));
    assert_eq!(
        watcher_calls(&sandbox).last().map(String::as_str),
        Some("--user enable --now devlab-watch.service")
    );
    let answers = old_devlab(&sandbox, &["gc"]);
    assert_eq!(answers.code, 0, "{answers:?}");

    let again = sandbox.factory(&["cutover", "--rollback"]);
    assert_eq!(
        (again.code, again.stdout.as_str(), again.stderr.as_str()),
        (
            0,
            format!(
                "not cut over: {} is today's devlab; nothing to roll back\n",
                devlab.display()
            )
            .as_str(),
            ""
        )
    );
}

#[test]
fn rollback_refuses_while_factory_lab_holds_a_lease_unless_forced() {
    let (sandbox, world) = machine();
    cut_over(&sandbox);
    let wt = world.worktree("demo-cut");
    let held = devlab_on_path(&sandbox, &wt, &["acquire", "--no-device"]);
    assert_eq!(held.code, 0, "{held:?}");

    let out = sandbox.factory(&["cutover", "--rollback"]);

    assert_eq!((out.code, out.stdout.as_str()), (75, ""), "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: factory lab holds lease 1 demo-cut (")
            && out
                .stderr
                .ends_with("); roll back once devlab status shows no lease\n"),
        "{out:?}"
    );
    assert_eq!(
        fs::read_link(link(&sandbox)).unwrap(),
        PathBuf::from("factory")
    );
    let old = old_devlab(&sandbox, &["status"]);
    assert_eq!((old.code, old.stderr.as_str()), (3, REFUSAL));

    let forced = sandbox.factory(&["cutover", "--rollback", "--force"]);
    assert_eq!(forced.code, 0, "{forced:?}");
    assert_eq!(fs::read_link(link(&sandbox)).unwrap(), PathBuf::from(OLD));
}

#[test]
fn a_devlab_that_ignores_the_cutover_is_not_cut_over() {
    let (sandbox, _world) = machine_with(false);

    let out = sandbox.factory(&["cutover"]);

    let script = fs::canonicalize(sandbox.path("dots/devlab/.local/bin/devlab")).unwrap();
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr),
        (
            3,
            "",
            format!(
                "factory: today's devlab at {} still answers once cut over; merge DotFiles' cutover refusal into the checkout it runs from, then run factory cutover again\n",
                script.display()
            )
        )
    );
    assert_not_cut_over(&sandbox);
}

#[test]
fn the_cutover_needs_factory_installed() {
    let (sandbox, _world) = machine();
    fs::remove_file(sandbox.path("home/.local/bin/factory")).unwrap();

    let out = sandbox.factory(&["cutover"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr),
        (
            3,
            "",
            format!(
                "factory: no factory at {} to hand devlab to; run factory install first\n",
                sandbox.path("home/.local/bin/factory").display()
            )
        )
    );
    assert_not_cut_over(&sandbox);
}

#[test]
fn a_watcher_systemd_will_not_stop_leaves_todays_devlab_in_place() {
    let (sandbox, _world) = machine();
    fs::write(sandbox.path("systemctl-fails"), "stop").unwrap();

    let out = sandbox.factory(&["cutover"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            3,
            "",
            "factory: systemctl --user stop devlab-watch.service failed: Failed to stop unit: Access denied; fix it and run factory cutover again\n"
        )
    );
    assert_eq!(fs::read_link(link(&sandbox)).unwrap(), PathBuf::from(OLD));
    assert_eq!(old_devlab(&sandbox, &["gc"]).code, 0);
    assert_eq!(
        watcher_calls(&sandbox),
        vec![
            "--user disable devlab-watch.service",
            "--user stop devlab-watch.service",
            "--user enable --now devlab-watch.service"
        ]
    );
}

#[test]
fn a_linked_watcher_unit_outlives_the_cutover_so_rollback_starts_it_again() {
    let (sandbox, _world) = machine();
    assert_eq!(watcher_state(&sandbox), ("enabled".into(), "active".into()));

    cut_over(&sandbox);

    assert_eq!(
        fs::read_link(watcher_unit(&sandbox)).unwrap(),
        PathBuf::from(WATCHER_LINK)
    );
    assert_eq!(
        watcher_state(&sandbox),
        ("disabled".into(), "inactive".into())
    );

    let out = sandbox.factory(&["cutover", "--rollback"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(watcher_state(&sandbox), ("enabled".into(), "active".into()));
}

#[test]
fn rollback_links_a_missing_watcher_unit_again_from_the_marker() {
    let (sandbox, _world) = machine();
    cut_over(&sandbox);
    // As a cutover from before #30 left it: systemd's disable took the link.
    fs::remove_file(watcher_unit(&sandbox)).unwrap();
    assert_eq!(watcher_state(&sandbox).0, "not-found");

    let out = sandbox.factory(&["cutover", "--rollback"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(
        fs::read_link(watcher_unit(&sandbox)).unwrap(),
        PathBuf::from(WATCHER_LINK)
    );
    assert_eq!(watcher_state(&sandbox), ("enabled".into(), "active".into()));
}

/// Rewrites the marker as a cutover from before #30 wrote it: without the watcher's unit.
fn marker_without_watcher(sandbox: &Sandbox) {
    let path = sandbox.path("state/devlab/cutover.json");
    let mut marker: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    marker.as_object_mut().unwrap().remove("watcher").unwrap();
    fs::write(&path, marker.to_string()).unwrap();
}

#[test]
fn a_marker_from_before_the_unit_was_recorded_still_rolls_back() {
    let (sandbox, _world) = machine();
    cut_over(&sandbox);
    marker_without_watcher(&sandbox);

    let out = sandbox.factory(&["cutover", "--rollback"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(fs::read_link(link(&sandbox)).unwrap(), PathBuf::from(OLD));
    assert_eq!(watcher_state(&sandbox), ("enabled".into(), "active".into()));
}

#[test]
fn rollback_says_how_to_link_a_missing_unit_the_marker_does_not_name() {
    let (sandbox, _world) = machine();
    cut_over(&sandbox);
    marker_without_watcher(&sandbox);
    fs::remove_file(watcher_unit(&sandbox)).unwrap();

    let out = sandbox.factory(&["cutover", "--rollback"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr),
        (
            3,
            "",
            format!(
                "factory: {} is gone and {} names no link to put back; link it again (dots link), then run factory cutover --rollback again\n",
                watcher_unit(&sandbox).display(),
                sandbox.path("state/devlab/cutover.json").display()
            )
        )
    );
    // Nothing changed: factory lab keeps the name, and today's devlab keeps refusing.
    assert_eq!(
        fs::read_link(link(&sandbox)).unwrap(),
        PathBuf::from("factory")
    );
    let old = old_devlab(&sandbox, &["status"]);
    assert_eq!((old.code, old.stderr.as_str()), (3, REFUSAL));
}
