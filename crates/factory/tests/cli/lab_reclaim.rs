//! A device whose lease runs nothing is taken back for a waiter (#57): only once its lease has
//! idled `deviceIdleMin`, only while someone waits for that kind of device, never from a lease
//! whose command runs; the lease's next `exec` says why its device is gone.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::lab::{devlab, lab, write_machine_config};
use crate::support::world::World;
use crate::support::{Run, Sandbox, run, spawn};

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

/// The lab with one phone and a device timer of 0.05 minutes: 3 s.
fn one_phone() -> (Sandbox, World) {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""deviceIdleMin": 0.05"#);
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
}

#[test]
fn a_phone_whose_lease_runs_an_exec_is_not_taken_however_long_it_runs() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let mut exec = devlab(&sandbox, &wt_a, &["exec", "--", "sleep", "9"]);
    exec.env("DEVLAB_OWNER_PID", a.0.id().to_string());
    let exec = spawn(exec);

    let refused = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "6"]);
    let ran = crate::support::finish(exec);

    assert_eq!(refused.code, 75, "{refused:?}");
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn an_idle_phone_stays_with_its_lease_once_nobody_waits_for_it() {
    let (sandbox, world) = one_phone();
    let (a, b) = (Owner::new(), Owner::new());
    let (wt_a, wt_b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    let held = owned_by(&sandbox, &wt_a, &a, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let gave_up = owned_by(&sandbox, &wt_b, &b, &["acquire", "--phone", "--wait", "0"]);
    assert_eq!(gave_up.code, 75, "{gave_up:?}");

    std::thread::sleep(Duration::from_secs(5));
    let exec = owned_by(&sandbox, &wt_a, &a, &["exec", "--", "true"]);

    assert_eq!(exec.code, 0, "{exec:?}");
}
