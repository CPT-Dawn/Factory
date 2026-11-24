//! The golden transcripts of today's devlab (#13): the fake world they run in, the replay that
//! compares any devlab binary with them, and one test per recorded scenario.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::support::golden;
use crate::support::world::{World, poll_until};
use crate::support::{Sandbox, run};

/// A stand-in devlab: a shell script, so the replay itself can be tested without today's devlab.
#[cfg(test)]
fn stand_in(dir: &Path, script: &str) -> PathBuf {
    let path = dir.join("stand-in-devlab");
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write the stand-in");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("make the stand-in executable");
    path
}

#[cfg(test)]
fn fixture(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("scenario.golden");
    fs::write(&path, text).expect("write the transcript");
    path
}

/// Owners were once `sleep` linked as `claude`; where sleep is one multicall binary (Ubuntu 26.04's
/// uutils coreutils) it ran as `claude`, found no such applet and exited, and every lease it held
/// went with it. An owner now needs no program but the shell.
#[test]
fn an_owner_is_a_live_process_named_claude_that_needs_no_program_but_the_shell() {
    let dir = tempfile::tempdir().unwrap();

    let mut owner = golden::start_owner(dir.path()).unwrap();
    let comm = format!("/proc/{}/comm", owner.id());
    // Named claude and asleep in its read: a process that had exited would show as a zombie.
    let waiting = poll_until(|| {
        let stat = fs::read_to_string(format!("/proc/{}/stat", owner.id())).unwrap_or_default();
        let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
        fs::read_to_string(&comm).is_ok_and(|name| name.trim() == "claude")
            && state == Some(Some('S'))
    });
    // Closing its stdin ends it, cleanly.
    drop(owner.stdin.take());
    let mut status = None;
    let ended = poll_until(|| {
        status = owner.try_wait().unwrap();
        status.is_some()
    });
    if !ended {
        let _ = owner.kill();
        let _ = owner.wait();
    }

    assert!(
        waiting,
        "the owner never showed as claude, asleep in its read"
    );
    assert!(ended, "the owner outlived its stdin");
    assert_eq!(status.and_then(|s| s.code()), Some(0));
}

#[test]
fn the_fake_adb_lists_what_is_plugged_in_until_it_is_unplugged() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);

    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    world
        .apply("plug FAKEPHONE02 unauthorized usb:1-2")
        .unwrap();
    let both = run(sandbox.command("adb", &["devices", "-l"]));
    world.apply("unplug FAKEPHONE01").unwrap();
    let one = run(sandbox.command("adb", &["devices", "-l"]));

    assert_eq!(
        both.stdout,
        "List of devices attached\n\
         FAKEPHONE01            device usb:1-1 model:Pixel_8\n\
         FAKEPHONE02            unauthorized usb:1-2\n\n"
    );
    assert_eq!(
        one.stdout,
        "List of devices attached\nFAKEPHONE02            unauthorized usb:1-2\n\n"
    );
}

#[test]
fn the_fake_adb_answers_a_plugged_device_and_refuses_a_missing_one() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    world
        .apply("plug FAKEPHONE01 device model:Pixel_8")
        .unwrap();

    let abi = run(sandbox.command(
        "adb",
        &["-s", "FAKEPHONE01", "shell", "getprop ro.product.cpu.abi"],
    ));
    let split = run(sandbox.command(
        "adb",
        &[
            "-s",
            "FAKEPHONE01",
            "shell",
            "getprop",
            "ro.product.cpu.abi",
        ],
    ));
    let missing = run(sandbox.command("adb", &["-s", "FAKEPHONE09", "shell", "true"]));

    assert_eq!((abi.code, abi.stdout.as_str()), (0, "arm64-v8a\n"));
    assert_eq!((split.code, split.stdout.as_str()), (0, "arm64-v8a\n"));
    assert_eq!(missing.code, 1);
    assert_eq!(missing.stderr, "adb: device 'FAKEPHONE09' not found\n");
}

#[test]
fn the_fake_adb_streams_a_frame_each_time_the_devices_change() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    world
        .apply("plug FAKEPHONE01 device model:Pixel_8")
        .unwrap();
    let track = world.track_devices(&sandbox);

    track.wait_for("0013FAKEPHONE01\tdevice\n");
    world.apply("unplug FAKEPHONE01").unwrap();

    track.wait_for("0013FAKEPHONE01\tdevice\n0000");
}

#[test]
fn the_fake_docker_sees_a_container_the_fake_supabase_started_and_stopped() {
    let sandbox = Sandbox::new();
    let _world = World::install(&sandbox);
    let inspect = ["inspect", "-f", "{{.State.Running}}", "supabase_db_demo"];

    let before = run(sandbox.command("docker", &inspect));
    let mut start = sandbox.command("supabase", &["start"]);
    start.env("SUPABASE_PROJECT_ID", "demo");
    assert_eq!(run(start).code, 0);
    let during = run(sandbox.command("docker", &inspect));
    run(sandbox.command("supabase", &["stop", "--project-id", "demo"]));
    let after = run(sandbox.command("docker", &inspect));

    assert_eq!(
        (before.code, before.stderr.as_str()),
        (1, "Error: No such object: supabase_db_demo\n")
    );
    assert_eq!((during.code, during.stdout.as_str()), (0, "true\n"));
    assert_eq!(after.code, 1);
}

#[test]
fn a_broken_supabase_start_fails_and_runs_nothing() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    world.apply("break supabase start").unwrap();

    let mut start = sandbox.command("supabase", &["start"]);
    start.env("SUPABASE_PROJECT_ID", "demo");
    let failed = run(start);
    world.apply("mend supabase start").unwrap();
    let mut again = sandbox.command("supabase", &["start"]);
    again.env("SUPABASE_PROJECT_ID", "demo");

    assert_eq!(failed.code, 1);
    assert_eq!(
        run(sandbox.command(
            "docker",
            &["inspect", "-f", "{{.State.Running}}", "supabase_db_demo"]
        ))
        .code,
        1
    );
    assert_eq!(run(again).code, 0);
}

#[test]
fn an_emulator_unit_systemd_run_starts_is_active_and_attached() {
    let sandbox = Sandbox::new();
    let _world = World::install(&sandbox);
    let active = ["--user", "is-active", "--quiet", "devlab-emu-0"];

    let before = run(sandbox.command("systemctl", &active));
    let started = run(sandbox.command(
        "systemd-run",
        &[
            "--user",
            "--quiet",
            "--unit=devlab-emu-0",
            "emulator",
            "-avd",
            "devlab-0",
            "-port",
            "5580",
        ],
    ));
    let during = run(sandbox.command("systemctl", &active));
    let listed = run(sandbox.command("adb", &["devices", "-l"]));
    run(sandbox.command("systemctl", &["--user", "stop", "devlab-emu-0"]));
    let after = run(sandbox.command("systemctl", &["--user", "is-active", "devlab-emu-0"]));

    assert_eq!((before.code, started.code, during.code), (3, 0, 0));
    assert!(
        listed.stdout.contains("\nemulator-5580          device "),
        "{listed:?}"
    );
    assert_eq!((after.code, after.stdout.as_str()), (3, "inactive\n"));
}

#[test]
fn a_broken_systemd_run_starts_no_emulator() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    world.apply("break systemd-run").unwrap();

    let started = run(sandbox.command(
        "systemd-run",
        &["--user", "--unit=devlab-emu-0", "emulator", "-port", "5580"],
    ));

    assert_eq!(started.code, 1);
    assert!(
        started
            .stderr
            .starts_with("Failed to start transient service unit"),
        "{started:?}"
    );
    assert_eq!(
        run(sandbox.command("adb", &["devices", "-l"])).stdout,
        "List of devices attached\n\n"
    );
}

#[test]
fn a_world_line_it_does_not_know_is_refused() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);

    assert_eq!(
        world.apply("explode now"),
        Err("unknown world step: explode now".to_owned())
    );
}

#[test]
fn the_replay_names_the_step_the_stream_and_the_first_byte_that_differ() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(dir.path(), "echo 'hello there'; echo 'devlab: said it' >&2");
    let scenario = fixture(
        dir.path(),
        "# Owner a asks for a greeting.\n$ a@plain-a devlab say\n1> hello world\n2> devlab: said it\n? 0\n",
    );

    let report = golden::replay(&scenario, &devlab, false).unwrap_err();

    assert_eq!(
        report,
        "scenario.golden:2 `$ a@plain-a devlab say`: stdout differs at byte 6\n\
         \x20 expected:\n\
         \x20   1> hello world\n\
         \x20 actual:\n\
         \x20   1> hello there\n"
    );
}

#[test]
fn the_replay_reports_a_different_exit_code_and_a_missing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(dir.path(), "printf 'no newline'; exit 4");
    let scenario = fixture(dir.path(), "$ a@plain-a devlab env\n1> no newline\n? 0\n");

    let report = golden::replay(&scenario, &devlab, false).unwrap_err();

    assert!(report.contains("stdout differs at byte 10\n"), "{report}");
    assert!(report.contains("    1~ no newline\n"), "{report}");
    assert!(
        report.contains("scenario.golden:1 `$ a@plain-a devlab env`: exit 4, expected 0\n"),
        "{report}"
    );
}

#[test]
fn the_replay_hides_the_sandbox_path_owner_pids_free_memory_and_signal_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(
        dir.path(),
        "pwd; echo \"lease 1 (claude:$DEVLAB_OWNER_PID)\"; echo 'memory 12.3 GB free'; echo '  \"memAvailableMB\": 12617'; echo 'devlab: sh killed by SIGKILL after 12 s'; echo 'gave up after 3 s'",
    );
    let scenario = fixture(
        dir.path(),
        "$ b@demo-b devlab status\n\
         1> <sandbox>/wt/demo-b\n\
         1> lease 1 (claude:<b>)\n\
         1> memory <mem> GB free\n\
         1>   \"memAvailableMB\": <mem>\n\
         1> devlab: sh killed by SIGKILL after <n> s\n\
         1> gave up after 3 s\n\
         ? 0\n",
    );

    assert_eq!(golden::replay(&scenario, &devlab, false), Ok(()));
}

#[test]
fn the_replay_waits_for_a_background_call_while_the_world_changes() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(
        dir.path(),
        "echo 'devlab: waiting: no phone' >&2\n\
         until adb devices -l | grep -q FAKEPHONE01; do sleep 0.02; done\n\
         echo 'got FAKEPHONE01'",
    );
    let scenario = fixture(
        dir.path(),
        "& q a@plain-a devlab acquire\n\
         until q devlab: waiting:\n\
         plug FAKEPHONE01 device model:Pixel_8\n\
         wait q\n\
         1> got FAKEPHONE01\n\
         2> devlab: waiting: no phone\n\
         ? 0\n",
    );

    assert_eq!(golden::replay(&scenario, &devlab, false), Ok(()));
}

#[test]
fn a_killed_owner_is_dead_to_the_calls_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(
        dir.path(),
        "kill -0 $DEVLAB_OWNER_PID 2>/dev/null && echo alive || echo dead",
    );
    let scenario = fixture(
        dir.path(),
        "$ a@plain-a devlab gc\n1> alive\n? 0\nkill a\n$ a@plain-a devlab gc\n1> dead\n? 0\n",
    );

    assert_eq!(golden::replay(&scenario, &devlab, false), Ok(()));
}

#[test]
fn recording_fills_in_what_each_call_showed_and_keeps_the_steps() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(
        dir.path(),
        "echo \"lease for $(basename \"$PWD\")\"; echo 'devlab: note' >&2; exit 3",
    );
    let scenario = fixture(
        dir.path(),
        "# Two calls.\n$ a@plain-a devlab acquire\n1> stale\n? 0\n\n$ a@demo-a devlab acquire\n",
    );

    assert_eq!(golden::replay(&scenario, &devlab, true), Ok(()));

    assert_eq!(
        fs::read_to_string(&scenario).unwrap(),
        "# Two calls.\n\
         $ a@plain-a devlab acquire\n1> lease for plain-a\n2> devlab: note\n? 3\n\n\
         $ a@demo-a devlab acquire\n1> lease for demo-a\n2> devlab: note\n? 3\n"
    );
    assert_eq!(golden::replay(&scenario, &devlab, false), Ok(()));
}

#[test]
fn a_step_the_replay_does_not_know_is_refused_with_its_line() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(dir.path(), "true");
    let scenario = fixture(dir.path(), "# fine\nfrobnicate now\n");

    assert_eq!(
        golden::replay(&scenario, &devlab, false),
        Err("scenario.golden:2: unknown world step: frobnicate now".to_owned())
    );
}

#[test]
fn a_background_tag_still_running_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let devlab = stand_in(dir.path(), "sleep 5");
    let scenario = fixture(
        dir.path(),
        "& q a@plain-a devlab acquire\n& q a@plain-a devlab acquire\n",
    );

    assert_eq!(
        golden::replay(&scenario, &devlab, false),
        Err("scenario.golden:2: background call q is still running".to_owned())
    );
}

#[test]
fn a_devlab_that_is_not_there_is_named_not_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let scenario = fixture(dir.path(), "$ a@plain-a devlab env\n? 0\n");
    let missing = dir.path().join("no-devlab");

    assert_eq!(
        golden::replay(&scenario, &missing, false),
        Err(format!(
            "scenario.golden: no devlab at {}",
            missing.display()
        ))
    );
}

#[test]
fn a_relative_devlab_path_is_read_from_the_workspace_root() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

    assert_eq!(
        golden::resolve_devlab("crates/factory/Cargo.toml".as_ref())
            .canonicalize()
            .unwrap(),
        manifest.join("Cargo.toml").canonicalize().unwrap()
    );
    assert_eq!(
        golden::resolve_devlab("/bin/sh".as_ref()),
        PathBuf::from("/bin/sh")
    );
}

/// One test per transcript, replayed against [`golden::devlab_under_test`]: the built `factory`
/// unless `GOLDEN_DEVLAB` names another; skipped when there is none. Run one group with `cargo test transcript::slots_` (or `devices_`, `cli_`).
pub(crate) mod transcript {
    use crate::support::golden;

    macro_rules! transcripts {
        ($($test:ident => $file:literal,)*) => {
            $(
                #[test]
                fn $test() {
                    replay($file);
                }
            )*
            pub const ALL: &[&str] = &[$($file),*];
        };
    }

    transcripts! {
        cli_usage => "cli-usage.golden",
        devices_emulator_failure => "devices-emulator-failure.golden",
        devices_emulator_grant => "devices-emulator-grant.golden",
        devices_lost => "devices-lost.golden",
        devices_phone_grant => "devices-phone-grant.golden",
        devices_run_lease_end => "devices-run-lease-end.golden",
        slots_exec_stopped => "slots-exec-stopped.golden",
        slots_free => "slots-free.golden",
        slots_owner_cap => "slots-owner-cap.golden",
        slots_queue => "slots-queue.golden",
        slots_release => "slots-release.golden",
        slots_run_exit => "slots-run-exit.golden",
        slots_run_killed => "slots-run-killed.golden",
        slots_run_stopped_queued => "slots-run-stopped-queued.golden",
        slots_run_stopped_running => "slots-run-stopped-running.golden",
        slots_stack_failure => "slots-stack-failure.golden",
        slots_wait_default => "slots-wait-default.golden",
    }

    fn replay(file: &str) {
        let Some(devlab) = golden::devlab_under_test(file) else {
            eprintln!(
                "skipped {file}: GOLDEN_DEVLAB names no devlab, and factory lab has none of its group yet"
            );
            return;
        };
        let fixture = golden::fixtures().join(file);
        if let Err(report) = golden::replay(&fixture, &devlab, golden::recording()) {
            panic!("{} differs from {file}:\n{report}", devlab.display());
        }
    }
}

#[test]
fn every_transcript_has_a_test() {
    let mut found: Vec<String> = fs::read_dir(golden::fixtures())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".golden"))
        .collect();
    found.sort();

    assert_eq!(found, transcript::ALL);
}
