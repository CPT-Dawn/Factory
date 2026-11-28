//! A device whose lease runs nothing is taken back for a waiter (#57): only once its lease has
//! idled `deviceIdleMin`, only while someone waits for that kind of device, never from a lease
//! whose command runs; the lease's next `exec` says why its device is gone.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use super::lab::{devlab, lab, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Run, Sandbox, finish, run, spawn};

/// A process that stands in for an agent owning leases, until the test ends.
struct Owner(Child);

impl Owner {
    fn new() -> Owner {
        let mut cmd = Command::new("sleep");
        cmd.arg("120").stdin(Stdio::null());
        Owner(spawn(cmd))
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn owned_by(sandbox: &Sandbox, dir: &Path, owner: &Owner, args: &[&str]) -> Run {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", owner.0.id().to_string());
    run(cmd)
}

/// `cmd` run in `dir` for `owner`, left running.
fn spawned_by(dir: &Path, owner: &Owner, mut cmd: Command) -> Child {
    cmd.current_dir(dir)
        .env("DEVLAB_OWNER_PID", owner.0.id().to_string());
    spawn(cmd)
}

/// Waits until `devlab status` shows a lease running `command`.
fn until_running(sandbox: &Sandbox, dir: &Path, owner: &Owner, command: &str) {
    let shown = poll_until(|| {
        owned_by(sandbox, dir, owner, &["status"])
            .stdout
            .contains(&format!("running {command}"))
    });
    assert!(shown, "status never showed {command} running");
}

/// The lab with one phone and a device timer of 0: a lease that runs nothing is idle at once,
/// so only a command running or nobody waiting keeps its phone.
fn one_phone() -> (Sandbox, World) {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""deviceIdleMin": 0"#);
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    (sandbox, world)
}

#[test]
fn an_idle_leases_phone_goes_to_the_agent_waiting_for_one_and_its_next_exec_exits_76() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert!(held.stdout.contains("FAKEPHONE01"), "{held:?}");

    let taken = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "60"]);
    let exec = owned_by(&sandbox, &wt_a, &a, &["exec", "--", "true"]);
    let status = owned_by(&sandbox, &wt_a, &a, &["status"]);

    assert_eq!(taken.code, 0, "{taken:?}");
    assert!(
        taken.stdout.contains("ANDROID_SERIAL=FAKEPHONE01"),
        "{taken:?}"
    );
    assert_eq!(exec.code, 76, "{exec:?}");
    assert!(
        exec.stderr.contains(
            "phone FAKEPHONE01 (Pixel 8) was reclaimed after 0 min idle: run devlab acquire to replace it"
        ),
        "{exec:?}"
    );
    // The lease itself stays, holding what it held but the phone.
    assert!(status.stdout.contains("lease 1 plain-a"), "{status:?}");
}

#[test]
fn a_reclaimed_phone_keeps_its_battery_for_the_lease_it_goes_to_and_a_release_restores_it() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert!(held.stdout.contains("FAKEPHONE01"), "{held:?}");
    let taken = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "60"]);
    assert!(
        taken.stdout.contains("ANDROID_SERIAL=FAKEPHONE01"),
        "{taken:?}"
    );

    // The release puts the battery back before it answers; the reclaim, which may come after
    // the phone's next grant, never does (#84).
    assert_eq!(owned_by(&sandbox, &wt_b, &b, &["release"]).code, 0);

    let restore = "dumpsys battery reset; dumpsys deviceidle unforce";
    let restores = sandbox
        .calls("adb")
        .iter()
        .filter(|c| c.last().is_some_and(|cmd| cmd == restore))
        .count();
    assert_eq!(restores, 1, "{:?}", sandbox.calls("adb"));
}

#[test]
fn a_phone_whose_lease_runs_an_exec_is_not_taken_however_long_it_runs() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let exec = spawned_by(
        &wt_a,
        &a,
        devlab(&sandbox, &wt_a, &["exec", "--", "sleep", "6"]),
    );
    until_running(&sandbox, &wt_a, &a, "sleep 6");

    let refused = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "2"]);
    let ran = finish(exec);

    assert_eq!(refused.code, 75, "{refused:?}");
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn a_phone_whose_lease_runs_a_devlab_run_is_not_taken_and_status_shows_the_run() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let run = spawned_by(
        &wt_a,
        &a,
        devlab(&sandbox, &wt_a, &["run", "--phone", "--", "sleep", "6"]),
    );
    until_running(&sandbox, &wt_a, &a, "sleep 6");

    let refused = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "2"]);
    let ran = finish(run);

    assert_eq!(refused.code, 75, "{refused:?}");
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn a_phone_whose_lease_runs_a_factory_lab_run_is_not_taken() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let run = spawned_by(
        &wt_a,
        &a,
        sandbox.factory_command(&["lab", "run", "--phone", "--", "sleep", "6"]),
    );
    until_running(&sandbox, &wt_a, &a, "sleep 6");

    let refused = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "2"]);
    let ran = finish(run);

    assert_eq!(refused.code, 75, "{refused:?}");
    assert_eq!(ran.code, 0, "{ran:?}");
}

/// The lab with a device timer of 0 and phones probed ten times a second, so a phone locked
/// while a lease holds it is known locked at once; FAKEPHONE01 plugged in.
fn watched_phones() -> (Sandbox, World) {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""deviceIdleMin": 0, "readinessCheckSec": 0.1"#);
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    (sandbox, world)
}

/// Waits until `devlab check` says `serial` is locked.
fn until_locked(sandbox: &Sandbox, dir: &Path, owner: &Owner, serial: &str) {
    let locked = poll_until(|| {
        owned_by(sandbox, dir, owner, &["check", serial])
            .stdout
            .contains(": locked:")
    });
    assert!(locked, "{serial} was never seen locked");
}

#[test]
fn a_free_locked_phone_does_not_keep_an_idle_leases_ready_phone_from_a_waiter() {
    let (sandbox, world) = watched_phones();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert!(held.stdout.contains("FAKEPHONE01"), "{held:?}");
    world.apply("lock FAKEPHONE03").unwrap();
    world
        .apply("plug FAKEPHONE03 device usb:1-3 model:Pixel_7")
        .unwrap();
    until_locked(&sandbox, &wt_b, &b, "FAKEPHONE03");

    let taken = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "10"]);

    assert_eq!(taken.code, 0, "{taken:?}");
    assert!(
        taken.stdout.contains("ANDROID_SERIAL=FAKEPHONE01"),
        "{taken:?}"
    );
}

#[test]
fn an_idle_leases_locked_phone_stays_with_it_while_another_idle_lease_has_a_ready_one() {
    let (sandbox, world) = watched_phones();
    world
        .apply("plug FAKEPHONE03 device usb:1-3 model:Pixel_7")
        .unwrap();
    let (a, b, c) = (Owner::new(), Owner::new(), Owner::new());
    let wt = |name| world.worktree(name);
    let (wt_a, wt_b, wt_c) = (wt("plain-a"), wt("plain-b"), wt("plain-c"));
    let first = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert!(first.stdout.contains("FAKEPHONE01"), "{first:?}");
    let second = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--quiet"]);
    assert!(second.stdout.contains("FAKEPHONE03"), "{second:?}");
    world.apply("lock FAKEPHONE01").unwrap();
    until_locked(&sandbox, &wt_a, &a, "FAKEPHONE01");

    let taken = owned_by(&sandbox, &wt_c, &c, &["acquire", "--phone", "--wait", "10"]);
    let kept = owned_by(&sandbox, &wt_a, &a, &["exec", "--", "true"]);

    assert_eq!(taken.code, 0, "{taken:?}");
    assert!(
        taken.stdout.contains("ANDROID_SERIAL=FAKEPHONE03"),
        "{taken:?}"
    );
    assert_eq!(kept.code, 0, "{kept:?}");
}

#[test]
fn factory_lab_exec_counts_as_running_too() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let exec = spawned_by(
        &wt_a,
        &a,
        sandbox.factory_command(&["lab", "exec", "--", "sleep", "6"]),
    );
    until_running(&sandbox, &wt_a, &a, "sleep 6");

    let refused = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "2"]);
    let ran = finish(exec);

    assert_eq!(refused.code, 75, "{refused:?}");
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn the_agent_waiting_gets_the_phone_as_soon_as_the_exec_holding_it_ends() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let exec = spawned_by(
        &wt_a,
        &a,
        devlab(&sandbox, &wt_a, &["exec", "--", "sleep", "2"]),
    );
    until_running(&sandbox, &wt_a, &a, "sleep 2");

    // Well within the keeper's minute between unprompted looks: the exec's end prompts one.
    let taken = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "30"]);
    let ran = finish(exec);

    assert_eq!(taken.code, 0, "{taken:?}");
    assert!(
        taken.stdout.contains("ANDROID_SERIAL=FAKEPHONE01"),
        "{taken:?}"
    );
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn an_idle_phone_stays_with_its_lease_when_nobody_waits_for_it() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");

    // A caller that won't wait is no waiter.
    let gave_up = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "0"]);
    let exec = owned_by(&sandbox, &wt_a, &a, &["exec", "--", "true"]);

    assert_eq!(gave_up.code, 75, "{gave_up:?}");
    assert_eq!(exec.code, 0, "{exec:?}");
}

#[test]
fn an_offline_phone_is_not_reclaimed_and_its_lease_is_told_it_is_offline() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    world.apply("plug FAKEPHONE01 offline usb:1-1").unwrap();

    let refused = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "2"]);
    let env = owned_by(&sandbox, &wt_a, &a, &["env"]);

    assert_eq!(refused.code, 75, "{refused:?}");
    assert_eq!(env.code, 76, "{env:?}");
    assert!(
        env.stderr
            .contains("phone FAKEPHONE01 (Pixel 8) is offline: run devlab acquire to replace it"),
        "{env:?}"
    );
}

#[test]
fn a_lease_that_lost_one_of_two_phones_is_whole_again_once_it_asks_for_one() {
    let (sandbox, world) = one_phone();
    // Both phones to one owner: over the default cap of all phones but one.
    write_machine_config(&sandbox, r#""deviceIdleMin": 0, "maxDevicesPerOwner": 2"#);
    world
        .apply("plug FAKEPHONE02 device usb:1-2 model:Pixel_7")
        .unwrap();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(
        &sandbox,
        &wt_a,
        &a,
        &["acquire", "--phone", "--pair", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");
    let taken = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "30"]);
    assert_eq!(taken.code, 0, "{taken:?}");
    // One waiter for one phone takes one.
    let status = owned_by(&sandbox, &wt_a, &a, &["status"]);
    let held_by_a = status
        .stdout
        .lines()
        .filter(|l| l.contains(" phone ") && l.contains("lease 1 plain-a"))
        .count();
    assert_eq!(held_by_a, 1, "{status:?}");

    let again = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    let exec = owned_by(&sandbox, &wt_a, &a, &["exec", "--", "true"]);

    assert_eq!(again.code, 0, "{again:?}");
    assert_eq!(exec.code, 0, "{exec:?}");
}

#[test]
fn a_lease_whose_phone_is_reclaimed_keeps_its_stack_slot() {
    let (sandbox, world) = one_phone();
    let (a, b, c) = (Owner::new(), Owner::new(), Owner::new());
    let [wt_a, wt_b, wt_c] = ["demo-a", "plain-b", "demo-c"].map(|n| world.worktree(n));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert!(held.stdout.contains("DEVLAB_SLOT=0"), "{held:?}");

    let taken = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "60"]);
    assert!(
        taken.stdout.contains("ANDROID_SERIAL=FAKEPHONE01"),
        "{taken:?}"
    );
    // Its lease still holds slot 0 of demo: the next lease of demo gets slot 1.
    let next = owned_by(&sandbox, &wt_c, &c, &["acquire", "--no-device", "--quiet"]);
    assert!(next.stdout.contains("DEVLAB_SLOT=1"), "{next:?}");
    let status = owned_by(&sandbox, &wt_a, &a, &["status"]);
    assert!(status.stdout.contains("lease 1 demo-a"), "{status:?}");
}

/// The `ANDROID_SERIAL` an acquire printed.
fn serial_of(out: &Run) -> String {
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("export ANDROID_SERIAL="))
        .unwrap_or_else(|| panic!("no ANDROID_SERIAL in {out:?}"))
        .trim_matches('\'')
        .to_owned()
}

#[test]
fn an_idle_emulator_adb_shows_offline_does_not_stand_in_for_one_an_idle_lease_holds() {
    let (sandbox, world) = lab();
    write_machine_config(
        &sandbox,
        r#""deviceIdleMin": 0, "maxEmulators": 2, "emulatorIdleMin": 60"#,
    );
    let (a, b, c) = (Owner::new(), Owner::new(), Owner::new());
    let [wt_a, wt_b, wt_c] = ["plain-a", "plain-b", "plain-c"].map(|n| world.worktree(n));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--emulator", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let other = owned_by(&sandbox, &wt_c, &c, &["acquire", "--emulator", "--quiet"]);
    assert_eq!(other.code, 0, "{other:?}");
    // The second emulator idles, free, but adb can't reach it.
    let idle = serial_of(&other);
    assert_eq!(owned_by(&sandbox, &wt_c, &c, &["release"]).code, 0);
    world.apply(&format!("plug {idle} offline")).unwrap();

    let taken = owned_by(
        &sandbox,
        &wt_b,
        &b,
        &["acquire", "--emulator", "--wait", "20"],
    );

    assert_eq!(taken.code, 0, "{taken:?}");
    assert_eq!(serial_of(&taken), serial_of(&held), "{taken:?}");
}
