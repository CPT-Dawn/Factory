//! What devlab refuses or says rather than guesses (#60): a grant is never made from a device
//! list adb stopped updating, a flag it doesn't know is no command, and a slot whose schema came
//! from another worktree is said so whether or not its stack runs.

use std::fs;
use std::path::{Path, PathBuf};

use super::adb_stream::{PHONE_A, adb_playing};
use super::lab::{devlab, lab};
use crate::support::world::{World, poll_until};
use crate::support::{Run, Sandbox, run};

/// `devlab <args>` in `dir`, the test process its owner.
fn owned(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Run {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    run(cmd)
}

fn daemon_log(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.path("run/factory/daemon.log")).unwrap_or_default()
}

/// A sandbox whose adb shows one phone, ends its track stream and waits 6 s to restart it, so
/// the daemon's device list is stale from the moment it logs the restart; and a worktree
/// without a project and one with one.
struct Down {
    sandbox: Sandbox,
    plain: PathBuf,
    project: PathBuf,
}

fn stream_down() -> Down {
    let sandbox = Sandbox::new().env("FACTORY_ADB_BACKOFF_MS", "6000");
    let world = World::install(&sandbox);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_factory"), sandbox.path("bin/devlab"))
        .expect("link devlab");
    let (plain, project) = (world.worktree("plain-a"), world.worktree("demo-a"));
    adb_playing(
        &sandbox,
        &format!(
            r#"if [ "$n" = 1 ]; then frame '{PHONE_A}'; exit 0; fi; frame '{PHONE_A}'; exec sleep 1000"#
        ),
    );
    // The first call starts the daemon, which starts the stream.
    owned(&sandbox, &plain, &["status"]);
    assert!(
        poll_until(|| daemon_log(&sandbox).contains("restarting in")),
        "the stream never ended: {}",
        daemon_log(&sandbox)
    );
    Down {
        sandbox,
        plain,
        project,
    }
}

#[test]
fn a_device_acquire_during_an_adb_restart_waits_and_says_why() {
    let down = stream_down();

    let out = owned(
        &down.sandbox,
        &down.plain,
        &["acquire", "--phone", "--wait", "1"],
    );

    assert_eq!(out.code, 75, "{out:?}");
    assert!(out.stderr.contains("adb is restarting"), "{out:?}");
    assert!(!out.stdout.contains("PHONEA01"), "{out:?}");
}

#[test]
fn a_device_acquire_is_granted_once_the_stream_shows_a_list_again() {
    let down = stream_down();

    let out = owned(
        &down.sandbox,
        &down.plain,
        &["acquire", "--phone", "--wait", "60"],
    );

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.contains("PHONEA01"), "{out:?}");
}

#[test]
fn an_acquire_for_no_device_goes_on_during_an_adb_restart() {
    let down = stream_down();

    let out = owned(&down.sandbox, &down.project, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.contains("DEVLAB_SLOT=0"), "{out:?}");
}

#[test]
fn an_unknown_flag_is_refused_with_exit_2_and_never_run_as_the_command() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-a");
    let marker = sandbox.path("ran");

    let out = owned(
        &sandbox,
        &wt,
        &[
            "run",
            "--abi",
            "x86",
            "--",
            "touch",
            &marker.display().to_string(),
        ],
    );

    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("unknown option --abi"), "{out:?}");
    assert!(!marker.exists(), "the command ran");
    let acquire = owned(&sandbox, &wt, &["acquire", "--no-device", "--stak"]);
    assert_eq!(acquire.code, 2, "{acquire:?}");
    assert!(
        acquire.stderr.contains("unknown option --stak"),
        "{acquire:?}"
    );
}

#[test]
fn a_slot_whose_stack_never_ran_another_worktrees_schema_warns_of_nothing() {
    let (sandbox, world) = lab();
    let (a, b) = (world.worktree("demo-a"), world.worktree("demo-b"));
    assert_eq!(owned(&sandbox, &a, &["acquire", "--no-device"]).code, 0);
    assert_eq!(owned(&sandbox, &a, &["release"]).code, 0);

    let out = owned(&sandbox, &b, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(!out.stderr.contains("schema"), "{out:?}");
}

#[test]
fn a_slot_last_used_by_another_worktree_warns_of_its_schema_with_its_stack_stopped() {
    let (sandbox, world) = lab();
    let (a, b) = (world.worktree("demo-a"), world.worktree("demo-b"));
    assert_eq!(
        owned(&sandbox, &a, &["acquire", "--no-device", "--stack"]).code,
        0
    );
    assert_eq!(owned(&sandbox, &a, &["release", "--stop-stack"]).code, 0);

    let out = owned(&sandbox, &b, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stderr.contains(
            "slot 0's stack last ran demo-a's schema: reset it if your migrations differ"
        ),
        "{out:?}"
    );
}

/// Writes the migrations dir of `worktree`'s supabase project: these files, and a directory.
fn migrations(worktree: &Path, files: &[&str], dirs: &[&str]) -> PathBuf {
    let dir = worktree.join("supabase/migrations");
    fs::create_dir_all(&dir).unwrap();
    for name in files {
        fs::write(dir.join(name), "").unwrap();
    }
    for name in dirs {
        fs::create_dir_all(dir.join(name)).unwrap();
    }
    dir
}

#[test]
fn a_lease_that_ends_records_the_newest_migration_of_its_worktree_with_the_slots_schema() {
    let (sandbox, world) = lab();
    let (a, b) = (world.worktree("demo-a"), world.worktree("demo-b"));
    let dir = migrations(&a, &["20260101_a.sql", "README.md"], &["zzz.sql"]);
    assert_eq!(
        owned(&sandbox, &a, &["acquire", "--no-device", "--stack"]).code,
        0
    );
    // A migration added during the lease is the head its end records.
    fs::write(dir.join("20260102_b.sql"), "").unwrap();
    assert_eq!(owned(&sandbox, &a, &["release"]).code, 0);

    let out = owned(&sandbox, &b, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stderr.contains(
            "slot 0's stack last ran demo-a's schema, up to 20260102_b.sql: reset it if your migrations differ"
        ),
        "{out:?}"
    );
}

#[test]
fn a_stack_stopped_by_its_lease_keeps_the_head_that_lease_ran() {
    let (sandbox, world) = lab();
    let (a, b) = (world.worktree("demo-a"), world.worktree("demo-b"));
    let dir = migrations(&a, &["20260101_a.sql"], &[]);
    assert_eq!(
        owned(&sandbox, &a, &["acquire", "--no-device", "--stack"]).code,
        0
    );
    fs::write(dir.join("20260102_b.sql"), "").unwrap();
    assert_eq!(owned(&sandbox, &a, &["release", "--stop-stack"]).code, 0);

    let out = owned(&sandbox, &b, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stderr
            .contains("slot 0's stack last ran demo-a's schema, up to 20260102_b.sql:"),
        "{out:?}"
    );
}

#[test]
fn a_lease_that_never_ran_the_slots_stack_leaves_the_schema_to_the_worktree_that_did() {
    let (sandbox, world) = lab();
    let (a, b, c) = (
        world.worktree("demo-a"),
        world.worktree("demo-b"),
        world.worktree("demo-c"),
    );
    assert_eq!(
        owned(&sandbox, &a, &["acquire", "--no-device", "--stack"]).code,
        0
    );
    assert_eq!(owned(&sandbox, &a, &["release", "--stop-stack"]).code, 0);
    assert_eq!(owned(&sandbox, &b, &["acquire", "--no-device"]).code, 0);
    assert_eq!(owned(&sandbox, &b, &["release"]).code, 0);

    let out = owned(&sandbox, &c, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stderr
            .contains("slot 0's stack last ran demo-a's schema:"),
        "{out:?}"
    );
}

#[test]
fn a_running_stack_devlab_has_no_schema_record_of_warns_of_the_slots_last_worktree() {
    let (sandbox, world) = lab();
    let (a, b) = (world.worktree("demo-a"), world.worktree("demo-b"));
    // Started outside devlab, or by a daemon from before the slot's schema was recorded.
    fs::write(sandbox.path("world/containers"), "supabase_db_demo\n").unwrap();
    assert_eq!(owned(&sandbox, &a, &["acquire", "--no-device"]).code, 0);
    assert_eq!(owned(&sandbox, &a, &["release"]).code, 0);

    let out = owned(&sandbox, &b, &["acquire", "--no-device"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stderr
            .contains("slot 0's stack last ran demo-a's schema:"),
        "{out:?}"
    );
}
