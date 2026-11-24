//! One kind of waiter never blocks another, and one owner can't hold every device (#58): a
//! waiter short of a phone lets a waiter for no device go first, an owner holds at most
//! `maxDevicesPerOwner` devices (all phones but one by default), and a request for more than
//! that is refused at once.

use std::fs;
use std::time::{Duration, Instant};

use super::lab::{Agent, for_owner, lab, waiting, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Sandbox, finish, run, spawn};

/// The lab with phones FAKEPHONE01 to FAKEPHONE0`n` plugged in, each ready.
fn phones(n: u32) -> (Sandbox, World) {
    let (sandbox, world) = lab();
    for i in 1..=n {
        world
            .apply(&format!(
                "plug FAKEPHONE0{i} device usb:1-{i} model:Pixel_8"
            ))
            .unwrap();
    }
    (sandbox, world)
}

#[test]
fn one_owner_gets_two_of_three_phones_waits_naming_the_cap_and_another_owner_gets_the_third() {
    let (sandbox, world) = phones(3);
    let (lead, other) = (Agent::start(), Agent::start());
    let [a1, a2, a3, b] =
        ["plain-a1", "plain-a2", "plain-a3", "plain-b"].map(|n| world.worktree(n));
    for wt in [&a1, &a2] {
        let held = run(for_owner(
            &sandbox,
            wt,
            lead.pid(),
            &["acquire", "--phone", "--quiet"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let third = spawn(for_owner(
        &sandbox,
        &a3,
        lead.pid(),
        &["acquire", "--phone", "--wait", "8"],
    ));
    assert!(poll_until(|| waiting(&sandbox, &a1) == ["plain-a3"]));

    // Queued behind the capped request, and served all the same.
    let served = run(for_owner(
        &sandbox,
        &b,
        other.pid(),
        &["acquire", "--phone", "--wait", "30"],
    ));
    let third = finish(third);

    assert_eq!(served.code, 0, "{served:?}");
    assert!(
        served.stdout.contains("ANDROID_SERIAL=FAKEPHONE03"),
        "{served:?}"
    );
    assert_eq!(third.code, 75, "{third:?}");
    assert!(
        third.stderr.contains(
            "already holds 2 device(s); 1 more would pass its cap of 2 (maxDevicesPerOwner; waits for one of its own leases to end)"
        ),
        "{third:?}"
    );
}

#[test]
fn a_request_for_more_devices_than_the_cap_exits_75_at_once() {
    let (sandbox, world) = phones(3);
    let lead = Agent::start();
    let wt = world.worktree("plain-many");
    let started = Instant::now();

    let refused = run(for_owner(
        &sandbox,
        &wt,
        lead.pid(),
        &["acquire", "--devices", "3", "--wait", "60"],
    ));

    assert_eq!(refused.code, 75, "{refused:?}");
    assert!(
        refused
            .stderr
            .contains("asks for 3 devices; one owner may hold at most 2 (maxDevicesPerOwner)"),
        "{refused:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(20), "{refused:?}");
}

#[test]
fn devlab_json_sets_the_device_cap_over_the_machine_config() {
    let (sandbox, world) = phones(3);
    write_machine_config(&sandbox, r#""maxDevicesPerOwner": 1"#);
    let lead = Agent::start();
    let wt = world.worktree("plain-cap");
    fs::write(
        wt.join("devlab.json"),
        r#"{"name":"cap","maxDevicesPerOwner":3}"#,
    )
    .expect("write devlab.json");

    let held = run(for_owner(
        &sandbox,
        &wt,
        lead.pid(),
        &["acquire", "--devices", "3", "--phone", "--quiet"],
    ));

    assert_eq!(held.code, 0, "{held:?}");
}

#[test]
fn a_waiter_short_of_a_phone_lets_a_request_for_no_device_have_its_slot() {
    let (sandbox, world) = phones(1);
    let (holder, phone_waiter, slot_waiter) = (Agent::start(), Agent::start(), Agent::start());
    let [a, b, c] = ["plain-a", "plain-b", "demo-c"].map(|n| world.worktree(n));
    let held = run(for_owner(
        &sandbox,
        &a,
        holder.pid(),
        &["acquire", "--phone", "--quiet"],
    ));
    assert_eq!(held.code, 0, "{held:?}");
    let short = spawn(for_owner(
        &sandbox,
        &b,
        phone_waiter.pid(),
        &["acquire", "--phone", "--wait", "8"],
    ));
    assert!(poll_until(|| waiting(&sandbox, &a) == ["plain-b"]));

    let slot = run(for_owner(
        &sandbox,
        &c,
        slot_waiter.pid(),
        &["acquire", "--no-device", "--wait", "5"],
    ));
    let short = finish(short);

    assert_eq!(slot.code, 0, "{slot:?}");
    assert!(slot.stdout.contains("DEVLAB_SLOT=0"), "{slot:?}");
    assert_eq!(short.code, 75, "{short:?}");
    assert!(short.stderr.contains("no free phone"), "{short:?}");
}
