//! Phones readied on plug-in, watched, announced and checked (#55): the daemon preps a phone
//! when adb's stream first shows it and after every replug, probes each attached phone every
//! `readinessCheckSec`, posts one desktop notification per change of its readiness, and
//! `devlab check` says per phone whether it can run unattended.

use std::path::{Path, PathBuf};

use super::lab::{devlab, lab, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Run, Sandbox, run};

const PIXEL: &str = "plug FAKEPHONE01 device usb:1-1 model:Pixel_8";

/// The golden world with notifications on and phones probed ten times a second, its daemon
/// running and following adb before any phone is plugged in.
fn watching() -> (Sandbox, World, PathBuf) {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""notify": true, "readinessCheckSec": 0.1"#);
    let wt = world.worktree("plain-ready");
    let status = run(devlab(&sandbox, &wt, &["status"]));
    assert_eq!(status.code, 0, "{status:?}");
    (sandbox, world, wt)
}

/// Each notification posted: notify-send's arguments.
fn posts(sandbox: &Sandbox) -> Vec<Vec<String>> {
    sandbox.calls("notify-send")
}

/// How many times the phones' screens were probed.
fn probes(sandbox: &Sandbox) -> usize {
    sandbox
        .calls("adb")
        .iter()
        .filter(|c| c.last().is_some_and(|cmd| cmd.contains("dumpsys power")))
        .count()
}

fn check(sandbox: &Sandbox, wt: &Path, args: &[&str]) -> Run {
    run(devlab(sandbox, wt, &[&["check"], args].concat()))
}

#[test]
fn a_phone_plugged_locked_asks_once_to_be_unlocked_and_says_ready_once_it_is() {
    let (sandbox, world, _wt) = watching();
    world.apply("lock FAKEPHONE01").unwrap();
    world.apply(PIXEL).unwrap();

    assert!(poll_until(|| !posts(&sandbox).is_empty()), "nothing posted");
    assert_eq!(
        posts(&sandbox),
        [[
            "-a",
            "devlab",
            "-u",
            "critical",
            "Pixel 8 needs unlocking",
            "Unlock it once: it stays awake and unlocked while plugged in."
        ]]
    );
    world.apply("unlock FAKEPHONE01").unwrap();
    assert!(
        poll_until(|| posts(&sandbox).len() == 2),
        "{:?}",
        posts(&sandbox)
    );
    assert_eq!(posts(&sandbox)[1][3..5], ["low", "Pixel 8 ready"]);

    let then = probes(&sandbox);
    assert!(
        poll_until(|| probes(&sandbox) >= then + 10),
        "probed {} times",
        probes(&sandbox) - then
    );
    assert_eq!(posts(&sandbox).len(), 2, "{:?}", posts(&sandbox));
    // From a systemd user service, a notification reaches the desktop through the session bus.
    let bus = format!("unix:path={}/bus\n", sandbox.path("run").display());
    assert_eq!(world.read("notify-bus"), bus.repeat(2));
    // Nothing the lab sends turns a screen off.
    let sent: Vec<String> = sandbox
        .calls("adb")
        .into_iter()
        .map(|c| c.join(" "))
        .collect();
    assert!(
        !sent
            .iter()
            .any(|c| c.contains("KEYCODE_POWER") || c.contains("KEYCODE_SLEEP")),
        "{sent:?}"
    );
}

#[test]
fn check_exits_10_naming_a_locked_phone_and_what_to_do_and_0_once_it_is_unlocked() {
    let (sandbox, world, wt) = watching();
    world.apply("lock FAKEPHONE01").unwrap();
    world.apply(PIXEL).unwrap();

    // Once adb's stream has shown it to the daemon.
    poll_until(|| check(&sandbox, &wt, &[]).stdout.contains("FAKEPHONE01"));
    let locked = check(&sandbox, &wt, &[]);
    assert_eq!(
        (locked.code, locked.stdout.as_str()),
        (
            10,
            "FAKEPHONE01 (Pixel 8): locked: unlock it once; it then stays awake and unlocked while plugged in\n"
        ),
        "{locked:?}"
    );
    world.apply("unlock FAKEPHONE01").unwrap();
    assert!(
        poll_until(|| check(&sandbox, &wt, &["FAKEPHONE01"]).code == 0),
        "{:?}",
        check(&sandbox, &wt, &["FAKEPHONE01"])
    );
    let ready = check(&sandbox, &wt, &["FAKEPHONE01"]);
    assert_eq!(ready.stdout, "FAKEPHONE01 (Pixel 8): ready\n", "{ready:?}");

    let json = check(&sandbox, &wt, &["--json"]);
    assert_eq!(json.code, 0, "{json:?}");
    let json: serde_json::Value =
        serde_json::from_str(&json.stdout).unwrap_or_else(|e| panic!("{e}: {json:?}"));
    assert_eq!(json["ready"], true, "{json}");
    assert_eq!(json["phones"][0]["serial"], "FAKEPHONE01", "{json}");
    assert_eq!(json["phones"][0]["readiness"], "ready", "{json}");

    let missing = check(&sandbox, &wt, &["FAKEPHONE01", "FAKEPHONE09"]);
    assert_eq!(
        (missing.code, missing.stdout.as_str()),
        (
            10,
            "FAKEPHONE01 (Pixel 8): ready\nFAKEPHONE09: not attached: plug it in and allow USB debugging\n"
        ),
        "{missing:?}"
    );
}

#[test]
fn a_replug_runs_the_prep_again() {
    let (sandbox, world, wt) = watching();
    world.apply(PIXEL).unwrap();
    let stay_on = "FAKEPHONE01 stay_on_while_plugged_in 7";
    // Ready once its prep is done.
    assert!(
        poll_until(|| check(&sandbox, &wt, &[]).code == 0),
        "{:?}",
        check(&sandbox, &wt, &[])
    );
    assert!(
        world.read("settings").contains(stay_on),
        "{}",
        world.read("settings")
    );
    // Something on the phone reset it: a probe sees it, and a replug sets it again.
    world
        .apply("setting FAKEPHONE01 stay_on_while_plugged_in 0")
        .unwrap();
    assert!(
        poll_until(|| check(&sandbox, &wt, &[]).stdout.contains("stay-awake off")),
        "{:?}",
        check(&sandbox, &wt, &[])
    );
    world.apply("unplug FAKEPHONE01").unwrap();
    assert!(
        poll_until(|| !check(&sandbox, &wt, &[]).stdout.contains("FAKEPHONE01")),
        "{:?}",
        check(&sandbox, &wt, &[])
    );
    world.apply(PIXEL).unwrap();
    assert!(
        poll_until(|| world.read("settings").contains(stay_on)),
        "{}",
        world.read("settings")
    );
    assert!(
        poll_until(|| check(&sandbox, &wt, &[]).code == 0),
        "{:?}",
        check(&sandbox, &wt, &[])
    );
}

#[test]
fn an_unplug_is_critical_while_a_lease_holds_the_phone_and_low_while_it_is_free() {
    let (sandbox, world, wt) = watching();
    world.apply(PIXEL).unwrap();
    assert!(
        poll_until(|| posts(&sandbox).len() == 1),
        "{:?}",
        posts(&sandbox)
    );
    let acquired = run(devlab(
        &sandbox,
        &wt,
        &["acquire", "--phone", "--wait", "0"],
    ));
    assert_eq!(acquired.code, 0, "{acquired:?}");

    world.apply("unplug FAKEPHONE01").unwrap();
    assert!(
        poll_until(|| posts(&sandbox).len() == 2),
        "{:?}",
        posts(&sandbox)
    );
    let leased = &posts(&sandbox)[1];
    assert_eq!(
        leased[3..5],
        ["critical", "Pixel 8 unplugged"],
        "{leased:?}"
    );
    assert!(leased[5].contains("(plain-ready)"), "{leased:?}");

    let released = run(devlab(&sandbox, &wt, &["release"]));
    assert_eq!(released.code, 0, "{released:?}");
    world.apply(PIXEL).unwrap();
    assert!(
        poll_until(|| posts(&sandbox).len() == 3),
        "{:?}",
        posts(&sandbox)
    );
    world.apply("unplug FAKEPHONE01").unwrap();
    assert!(
        poll_until(|| posts(&sandbox).len() == 4),
        "{:?}",
        posts(&sandbox)
    );
    assert_eq!(posts(&sandbox)[3][3..5], ["low", "Pixel 8 unplugged"]);
}

/// The daemon's log.
fn daemon_log(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(sandbox.path("run/factory/daemon.log")).unwrap_or_default()
}

/// A directory of links to every program in /usr/bin and /bin but `missing`: a PATH where it
/// is not found, though the host has it.
fn system_without(sandbox: &Sandbox, missing: &str) -> PathBuf {
    let dir = sandbox.path("system-bin");
    std::fs::create_dir_all(&dir).unwrap();
    for sys in ["/usr/bin", "/bin"] {
        for entry in std::fs::read_dir(sys).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if name != missing && !dir.join(&name).exists() {
                let _ = std::os::unix::fs::symlink(entry.path(), dir.join(&name));
            }
        }
    }
    dir
}

#[test]
fn a_daemon_with_no_notify_send_says_so_in_its_log_once_and_goes_on_watching() {
    let (sandbox, world) = lab();
    let system = system_without(&sandbox, "notify-send");
    let path = format!("{}:{}", sandbox.path("bin").display(), system.display());
    let sandbox = sandbox.env("PATH", path);
    std::fs::remove_file(sandbox.path("bin/notify-send")).unwrap();
    write_machine_config(&sandbox, r#""notify": true, "readinessCheckSec": 0.1"#);
    let wt = world.worktree("plain-ready");
    assert_eq!(run(devlab(&sandbox, &wt, &["status"])).code, 0);

    world.apply("lock FAKEPHONE01").unwrap();
    world.apply(PIXEL).unwrap();
    assert!(
        poll_until(|| daemon_log(&sandbox).contains("Pixel 8 needs unlocking")),
        "{}",
        daemon_log(&sandbox)
    );
    world.apply("unlock FAKEPHONE01").unwrap();
    assert!(
        poll_until(|| daemon_log(&sandbox).contains("Pixel 8 ready")),
        "{}",
        daemon_log(&sandbox)
    );
    let log = daemon_log(&sandbox);
    let said =
        "factory: lab: notify-send not found: no desktop notifications until it is installed";
    assert_eq!(log.lines().filter(|l| *l == said).count(), 1, "{log}");
    assert!(posts(&sandbox).is_empty(), "{:?}", posts(&sandbox));
    assert_eq!(check(&sandbox, &wt, &[]).code, 0, "{log}");
}

#[test]
fn factory_lab_check_keeps_the_output_contract() {
    let (sandbox, world, wt) = watching();
    world.apply("lock FAKEPHONE01").unwrap();
    world.apply(PIXEL).unwrap();
    let native = |args: &[&str]| {
        let mut cmd = sandbox.factory_command(&[&["lab", "check"], args].concat());
        cmd.current_dir(&wt);
        run(cmd)
    };
    assert!(
        poll_until(|| native(&[]).stdout.contains("locked")),
        "{:?}",
        native(&[])
    );
    let one = native(&[]);
    assert_eq!(
        (one.code, one.stdout.as_str()),
        (
            10,
            "FAKEPHONE01 (Pixel 8): locked: unlock it once; it then stays awake and unlocked while plugged in\n"
        ),
        "{one:?}"
    );
    let two = native(&["FAKEPHONE01", "FAKEPHONE09", "--json"]);
    assert_eq!(two.code, 10, "{two:?}");
    let json: serde_json::Value =
        serde_json::from_str(&two.stdout).unwrap_or_else(|e| panic!("{e}: {two:?}"));
    assert_eq!(json["ready"], false, "{json}");
    assert_eq!(
        json["phones"][1]["readiness"],
        serde_json::Value::Null,
        "{json}"
    );
    let listed = std::fs::read_to_string(json["checkFile"].as_str().unwrap()).unwrap();
    assert!(listed.contains("FAKEPHONE09: not attached"), "{listed}");
    let summary = native(&["FAKEPHONE01", "FAKEPHONE09"]);
    assert!(
        summary.stdout.starts_with("0 of 2 phones ready; each in "),
        "{summary:?}"
    );
}

#[test]
fn a_daemon_on_a_machine_with_no_devlab_config_readies_phones_and_notifies_by_default() {
    let (sandbox, world) = lab();
    std::fs::remove_file(sandbox.path("config/devlab/config.json")).unwrap();
    let wt = world.worktree("plain-ready");
    // A call that needs no device list starts the daemon.
    run(devlab(&sandbox, &wt, &["release"]));
    world.apply(PIXEL).unwrap();

    assert!(poll_until(|| !posts(&sandbox).is_empty()), "nothing posted");
    assert_eq!(posts(&sandbox)[0][3..5], ["low", "Pixel 8 ready"]);
    assert!(
        world
            .read("settings")
            .contains("FAKEPHONE01 stay_on_while_plugged_in 7"),
        "{}",
        world.read("settings")
    );
}
