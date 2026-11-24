//! Only ready phones are granted (#56): a phone that is locked, asleep, offline or unauthorized
//! is skipped like a missing one, a `--phone` request waits and says how many of each, and a
//! locked phone unlocked during the wait goes to the waiter.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::lab::{devlab, lab};
use crate::support::world::{World, poll_until};
use crate::support::{Run, Sandbox, run};

const PIXEL_1: &str = "plug FAKEPHONE01 device usb:1-1 product:shiba model:Pixel_8 transport_id:1";
const PIXEL_3: &str =
    "plug FAKEPHONE03 device usb:1-3 product:panther model:Pixel_7 transport_id:3";

/// `devlab <args>` in `dir`, the test process its owner.
fn owned(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Run {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    run(cmd)
}

/// A lab whose daemon has seen `plugs`: each applied to adb, then waited for in `devlab check`.
/// The phones `locked` name are locked before they are plugged in.
fn plugged(locked: &[&str], plugs: &[&str]) -> (Sandbox, World, PathBuf) {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-phones");
    for serial in locked {
        world.apply(&format!("lock {serial}")).unwrap();
    }
    for plug in plugs {
        world.apply(plug).unwrap();
    }
    let serials: Vec<&str> = plugs.iter().filter_map(|p| p.split(' ').nth(1)).collect();
    assert!(
        poll_until(|| {
            let seen = run(devlab(&sandbox, &wt, &["check"])).stdout;
            serials.iter().all(|s| seen.contains(s))
        }),
        "the daemon never listed {serials:?}"
    );
    (sandbox, world, wt)
}

fn serial_of(out: &Run) -> Option<&str> {
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("export ANDROID_SERIAL="))
}

#[test]
fn a_locked_phone_is_passed_over_for_an_unlocked_one_whatever_their_serials() {
    let (sandbox, _world, wt) = plugged(&["FAKEPHONE01"], &[PIXEL_1, PIXEL_3]);

    let got = owned(&sandbox, &wt, &["acquire", "--phone", "--wait", "0"]);

    assert_eq!(got.code, 0, "{got:?}");
    assert_eq!(serial_of(&got), Some("FAKEPHONE03"), "{got:?}");
    assert!(!got.stderr.contains("lock screen"), "{got:?}");
}

#[test]
fn the_only_phone_locked_makes_a_phone_request_wait_naming_it_until_it_is_unlocked() {
    let (sandbox, world, wt) = plugged(&["FAKEPHONE01"], &[PIXEL_1]);

    let refused = owned(&sandbox, &wt, &["acquire", "--phone", "--wait", "2"]);
    assert_eq!(refused.code, 75, "{refused:?}");
    assert!(
        refused.stderr.contains(
            "gave up after 2 s: no free phone: 0 phone(s) busy, 1 locked (unlock it once)"
        ),
        "{refused:?}"
    );

    let mut cmd = devlab(&sandbox, &wt, &["acquire", "--phone", "--wait", "30"]);
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let waiter = cmd.spawn().expect("start the waiting acquire");
    assert!(
        poll_until(|| {
            let status = run(devlab(&sandbox, &wt, &["status", "--json"])).stdout;
            serde_json::from_str::<serde_json::Value>(&status)
                .is_ok_and(|s| s["waiting"].as_array().is_some_and(|w| !w.is_empty()))
        }),
        "the acquire never queued"
    );
    world.apply("unlock FAKEPHONE01").unwrap();
    let out = waiter.wait_with_output().expect("the waiter ends");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout.contains("export ANDROID_SERIAL=FAKEPHONE01"),
        "{stdout}"
    );
}

#[test]
fn offline_and_unauthorized_phones_are_counted_in_the_reason() {
    let (sandbox, _world, wt) = plugged(
        &[],
        &[
            "plug FAKEPHONE01 offline usb:1-1 transport_id:1",
            "plug FAKEPHONE02 unauthorized usb:1-2 transport_id:2",
        ],
    );

    let got = owned(&sandbox, &wt, &["acquire", "--phone", "--wait", "0"]);

    assert_eq!(got.code, 75, "{got:?}");
    assert!(
        got.stderr.contains(
            "gave up after 0 s: no free phone: 0 phone(s) busy, 1 offline, 1 unauthorized (tap Allow on the phone)"
        ),
        "{got:?}"
    );
}
