//! The sandbox's fakes behave as later tests rely on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::support::world::World;
use crate::support::{DEADLINE, Sandbox, SetOnDrop, run};

#[test]
fn an_unscripted_fake_refuses_and_records_its_call() {
    let sandbox = Sandbox::new();

    let out = run(sandbox.command("gh", &["pr", "checks", "12"]));

    assert_eq!(out.code, 127);
    assert_eq!(out.stderr, "fake gh: no script for: pr checks 12\n");
    assert_eq!(sandbox.calls("gh"), vec![vec!["pr", "checks", "12"]]);
}

#[test]
fn a_scripted_fake_plays_back_its_script_and_records_each_call() {
    let sandbox = Sandbox::new();
    sandbox.fake("adb", "echo \"devices: $#\"; exit 3");

    let first = run(sandbox.command("adb", &["devices", "-l"]));
    let second = run(sandbox.command("adb", &["shell", "echo two words"]));

    assert_eq!((first.code, first.stdout.as_str()), (3, "devices: 2\n"));
    assert_eq!((second.code, second.stdout.as_str()), (3, "devices: 2\n"));
    assert_eq!(
        sandbox.calls("adb"),
        vec![vec!["devices", "-l"], vec!["shell", "echo two words"]]
    );
}

/// A world step is one change: a fake that reads the world meanwhile sees it from before or
/// after, never half-written. A phone plugged in beside another once made the fake adb find
/// no devices at all, so the daemon's prep of the first phone failed (docs/flakes.md).
#[test]
fn a_world_step_never_shows_the_fakes_a_half_written_world() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    let done = AtomicBool::new(false);

    let failed = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
                world
                    .apply("plug FAKEPHONE02 device usb:1-2 model:Pixel_8")
                    .unwrap();
                world
                    .apply("setting FAKEPHONE02 stay_on_while_plugged_in 7")
                    .unwrap();
            }
        });
        let _stop = SetOnDrop(&done);
        let failed: Vec<_> = (0..150)
            .map(|_| {
                run(sandbox.command(
                    "adb",
                    &[
                        "-s",
                        "FAKEPHONE01",
                        "shell",
                        "settings get global stay_on_while_plugged_in",
                    ],
                ))
            })
            .filter(|out| out.code != 0)
            .collect();
        failed
    });

    assert!(
        failed.is_empty(),
        "{} of 150 calls failed: {:?}",
        failed.len(),
        failed.first()
    );
}

/// An emulator killed is gone from adb by the time its unit is: the daemon that stops one
/// waits for its unit to end, then boots a new one on the same serial, and a kill that dropped
/// the unit first took the new emulator's line out of the world with it (docs/flakes.md).
#[test]
fn a_killed_emulator_is_gone_from_adb_before_its_unit_is() {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    let start = || {
        let started = run(sandbox.command(
            "systemd-run",
            &["--user", "--unit=devlab-emu-0", "emulator", "-port", "5580"],
        ));
        assert_eq!(started.code, 0, "{started:?}");
    };
    let mut lost = 0;

    for _ in 0..40 {
        start();
        std::thread::scope(|scope| {
            let kill = scope
                .spawn(|| run(sandbox.command("adb", &["-s", "emulator-5580", "emu", "kill"])));
            // The daemon's own order: its unit ended, it boots the next emulator on the serial.
            let given_up = Instant::now() + DEADLINE;
            while world.read("units").contains("devlab-emu-0") {
                assert!(Instant::now() < given_up, "the kill never ended the unit");
            }
            start();
            assert_eq!(kill.join().unwrap().code, 0);
        });
        if world.read("devices").matches("emulator-5580").count() != 1 {
            lost += 1;
        }
        let killed = run(sandbox.command("adb", &["-s", "emulator-5580", "emu", "kill"]));
        assert_eq!(killed.code, 0, "{killed:?}");
    }

    assert_eq!(
        lost, 0,
        "the next emulator's line was taken out with the killed one's"
    );
}
