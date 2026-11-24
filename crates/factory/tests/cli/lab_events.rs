//! The lab's events in the stats ledger (#62): the daemon writes each lease, wait, reclaim and
//! readiness change as it happens, one line each, and a ledger it can't write changes nothing
//! any lab command does.

use std::path::Path;

use serde_json::Value;

use super::lab::{devlab, lab, owned, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Run, Sandbox, run};

const PIXEL: &str = "plug FAKEPHONE01 device usb:1-1 model:Pixel_8";

/// The ledger's events of type `kind`, oldest first.
fn events(sandbox: &Sandbox, kind: &str) -> Vec<Value> {
    sandbox
        .ledger_lines()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == kind)
        .collect()
}

/// Waits until the ledger holds `n` events of type `kind` that `pick` keeps, and returns them.
fn await_events(
    sandbox: &Sandbox,
    kind: &str,
    n: usize,
    pick: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let picked =
        |s: &Sandbox| -> Vec<Value> { events(s, kind).into_iter().filter(&pick).collect() };
    poll_until(|| picked(sandbox).len() >= n);
    picked(sandbox)
}

/// No event of #62 names the fake phone's serial or the sandbox's paths.
fn nothing_private(sandbox: &Sandbox) {
    let root = sandbox.path("").display().to_string();
    let ours = ["lab-lease", "lab-wait", "lab-reclaim", "lab-readiness"];
    for line in sandbox.ledger_lines().iter().filter(|l| {
        ours.iter()
            .any(|kind| l.contains(&format!("\"type\":\"{kind}\"")))
    }) {
        assert!(!line.contains("FAKEPHONE01"), "a serial in {line}");
        assert!(!line.contains(&root), "a sandbox path in {line}");
    }
}

/// The lab with the Pixel plugged in.
fn one_phone(extra: Option<&str>) -> (Sandbox, World) {
    let (sandbox, world) = lab();
    if let Some(extra) = extra {
        write_machine_config(&sandbox, extra);
    }
    world.apply(PIXEL).unwrap();
    (sandbox, world)
}

/// `devlab` run in `dir` for the owner `pid`.
fn owned_by(sandbox: &Sandbox, dir: &Path, pid: u32, args: &[&str]) -> Run {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", pid.to_string());
    run(cmd)
}

#[test]
fn a_devlab_run_writes_one_lease_start_and_one_run_end_with_its_seconds_and_idle_seconds() {
    let (sandbox, world) = one_phone(None);
    let wt = world.worktree("plain-events");

    let ran = owned(&sandbox, &wt, &["run", "--phone", "--quiet", "--", "true"]);
    assert_eq!(ran.code, 0, "{ran:?}");

    let ends = await_events(&sandbox, "lab-lease", 1, |e| e["action"] == "end");
    let starts: Vec<Value> = events(&sandbox, "lab-lease")
        .into_iter()
        .filter(|e| e["action"] == "start")
        .collect();
    assert_eq!((starts.len(), ends.len()), (1, 1), "{starts:?} {ends:?}");
    let (start, end) = (&starts[0], &ends[0]);
    assert_eq!(start["lease"], end["lease"]);
    assert_eq!(start["worktree"], "plain-events");
    assert_eq!(start["devices"], "phone:Pixel 8");
    assert_eq!(start["count"], 1);
    assert_eq!(end["reason"], "run-end");
    assert!(end["seconds"].is_u64(), "{end}");
    assert!(end["idle_seconds"].is_u64(), "{end}");
    assert!(
        end["idle_seconds"].as_u64() <= end["seconds"].as_u64(),
        "{end}"
    );
    let granted = await_events(&sandbox, "lab-wait", 1, |e| e["action"] == "granted");
    assert_eq!(granted[0]["asked"], "phone", "{granted:?}");
    nothing_private(&sandbox);
}

#[test]
fn a_release_ends_the_lease_with_reason_release() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-release");
    assert_eq!(owned(&sandbox, &wt, &["acquire", "--no-device"]).code, 0);
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);

    let ends = await_events(&sandbox, "lab-lease", 1, |e| e["action"] == "end");
    assert_eq!(ends[0]["reason"], "release", "{ends:?}");
    assert_eq!(ends[0]["count"], 0, "{ends:?}");
}

#[test]
fn a_leases_idle_seconds_leave_out_the_time_an_exec_ran_on_it() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-idle");
    assert_eq!(owned(&sandbox, &wt, &["acquire", "--no-device"]).code, 0);
    assert_eq!(owned(&sandbox, &wt, &["exec", "--", "sleep", "2"]).code, 0);
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);

    let ends = await_events(&sandbox, "lab-lease", 1, |e| e["action"] == "end");
    let (seconds, idle) = (
        ends[0]["seconds"].as_u64().unwrap_or_default(),
        ends[0]["idle_seconds"].as_u64().unwrap_or_default(),
    );
    assert!(seconds >= 2, "{ends:?}");
    assert!(idle <= seconds - 2, "{ends:?}");
}

#[test]
fn a_wait_that_times_out_writes_a_gave_up_wait_with_its_seconds_and_reason() {
    let (sandbox, world) = one_phone(None);
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let holder = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn an owner");
    let held = owned_by(
        &sandbox,
        &wt_a,
        holder.id(),
        &["acquire", "--phone", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");

    let refused = owned(&sandbox, &wt_b, &["acquire", "--phone", "--wait", "1"]);
    assert_eq!(refused.code, 75, "{refused:?}");

    let gave_up = await_events(&sandbox, "lab-wait", 1, |e| e["action"] == "gave-up");
    let wait = &gave_up[0];
    assert_eq!(wait["asked"], "phone", "{wait}");
    assert_eq!(wait["count"], 1, "{wait}");
    assert!(wait["seconds"].is_u64(), "{wait}");
    assert_eq!(wait["wait"], "phones", "{wait}");
    assert_eq!(wait["busy"], 1, "{wait}");
    let mut holder = holder;
    let _ = holder.kill();
    let _ = holder.wait();
    nothing_private(&sandbox);
}

#[test]
fn a_reclaim_writes_the_reclaim_and_the_waiters_grant() {
    let (sandbox, world) = one_phone(Some(r#""deviceIdleMin": 0"#));
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let mut holder = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn an owner");
    let held = owned_by(
        &sandbox,
        &wt_a,
        holder.id(),
        &["acquire", "--phone", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");

    let taken = owned(&sandbox, &wt_b, &["acquire", "--phone", "--wait", "60"]);
    assert_eq!(taken.code, 0, "{taken:?}");

    let reclaims = await_events(&sandbox, "lab-reclaim", 1, |_| true);
    let reclaim = &reclaims[0];
    assert_eq!(reclaim["lease"], 1, "{reclaim}");
    assert_eq!(reclaim["kind"], "phone", "{reclaim}");
    assert_eq!(reclaim["model"], "Pixel 8", "{reclaim}");
    assert!(reclaim["idle_minutes"].is_u64(), "{reclaim}");
    let granted = await_events(&sandbox, "lab-wait", 2, |e| e["action"] == "granted");
    assert_eq!(granted.len(), 2, "{granted:?}");
    // The reclaim names the waiter whose grant took the phone.
    assert!(reclaim["waiter"].is_i64(), "{reclaim}");
    assert_eq!(granted[1]["waiter"], reclaim["waiter"], "{granted:?}");
    let _ = holder.kill();
    let _ = holder.wait();
    nothing_private(&sandbox);
}

#[test]
fn a_locked_then_unlocked_phone_writes_two_readiness_events() {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""notify": true, "readinessCheckSec": 0.1"#);
    let wt = world.worktree("plain-ready");
    assert_eq!(run(devlab(&sandbox, &wt, &["status"])).code, 0);
    world.apply("lock FAKEPHONE01").unwrap();
    world.apply(PIXEL).unwrap();
    await_events(&sandbox, "lab-readiness", 1, |_| true);
    world.apply("unlock FAKEPHONE01").unwrap();

    let changes = await_events(&sandbox, "lab-readiness", 2, |_| true);
    let seen: Vec<(Value, Value, Value)> = changes
        .iter()
        .map(|e| (e["from"].clone(), e["to"].clone(), e["notified"].clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("none".into(), "locked".into(), "true".into()),
            ("locked".into(), "ready".into(), "true".into()),
        ]
    );
    assert_eq!(changes[0]["model"], "Pixel 8");
    let id = changes[0]["phone"].as_str().unwrap_or_default();
    assert_eq!(id.len(), 8, "{changes:?}");
    assert_eq!(changes[1]["phone"], changes[0]["phone"]);
    nothing_private(&sandbox);
}

/// The lab commands the ledger test runs, as each one ended: exit code, stdout and stderr, with
/// the sandbox's own path taken out.
fn a_day_in_the_lab(sandbox: &Sandbox, world: &World) -> Vec<(i32, String, String)> {
    let wt = world.worktree("demo-day");
    let root = sandbox.path("").display().to_string();
    let steps: [&[&str]; 6] = [
        &["acquire", "--no-device"],
        &["env"],
        &["release"],
        &["run", "--no-device", "--", "true"],
        &["run", "--phone", "--", "true"],
        &["acquire", "--emulator", "--wait", "1"],
    ];
    steps
        .iter()
        .map(|args| {
            let out = owned(sandbox, &wt, args);
            (
                out.code,
                out.stdout.replace(&root, "<sandbox>/"),
                out.stderr.replace(&root, "<sandbox>/"),
            )
        })
        .collect()
}

/// The lab day of [`a_day_in_the_lab`] in a sandbox whose ledger `spoil` made unwritable.
fn a_day_with_a_spoilt_ledger(spoil: impl Fn(&Path)) -> Vec<(i32, String, String)> {
    let (sandbox, world) = one_phone(Some(r#""maxEmulators": 0"#));
    let ledger = sandbox.ledger();
    std::fs::create_dir_all(ledger.parent().expect("a state dir")).expect("make the state dir");
    spoil(&ledger);
    a_day_in_the_lab(&sandbox, &world)
}

#[test]
fn a_ledger_that_cannot_be_written_leaves_every_lab_commands_exit_code_and_output_unchanged() {
    let (writable, world) = one_phone(Some(r#""maxEmulators": 0"#));
    let expected = a_day_in_the_lab(&writable, &world);

    // Every write to it fails, as on a full disk or a filesystem remounted read-only.
    let full = a_day_with_a_spoilt_ledger(|ledger| {
        std::os::unix::fs::symlink("/dev/full", ledger).expect("link the ledger to /dev/full");
    });
    // It can't even be opened, as in a read-only dir: the daemon starts all the same.
    let unopenable = a_day_with_a_spoilt_ledger(|ledger| {
        std::fs::create_dir(ledger).expect("make the ledger a dir");
    });

    assert_eq!(full, expected);
    assert_eq!(unopenable, expected);
    assert!(
        expected.iter().any(|(code, _, _)| *code == 75),
        "{expected:?}"
    );
    assert!(
        events(&writable, "lab-lease").len() >= 4,
        "{:?}",
        writable.ledger_lines()
    );
}
