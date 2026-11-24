//! The lab's events in the stats ledger (#62): one line per fact the daemon learns, each field
//! a flat value, the measures numbers, and nothing that names a serial, a home or a user.

use factory_core::event::{Fields, is_numeric_field};
use factory_core::lab::devices::{Mode, PlanInput, Unready, plan_devices};
use factory_core::lab::events::{
    Asked, LeaseEnd, LeaseFacts, lease_end, lease_start, readiness, reclaim, short_id, spans,
    wait_gave_up, wait_granted, wait_reason,
};
use factory_core::lab::ready::Readiness;
use serde_json::{Value, json};

fn object(fields: &Fields) -> Value {
    Value::Object(fields.clone())
}

/// Every number an event holds is in the ledger's numeric list, so `factory log` types it alike.
fn numbers_are_listed(fields: &Fields) {
    for (key, value) in fields {
        match value {
            Value::Number(_) => assert!(is_numeric_field(key), "{key} is no numeric field"),
            Value::String(_) | Value::Bool(_) => {}
            other => panic!("{key} is not a flat value: {other}"),
        }
    }
}

fn facts() -> LeaseFacts {
    LeaseFacts {
        id: 7,
        project: Some("demo".to_owned()),
        owner_label: "claude:4242".to_owned(),
        worktree: "/tmp/sandbox/worktrees/demo-a".to_owned(),
        private: vec!["someone".to_owned()],
        devices: vec![
            ("phone".to_owned(), "Pixel_8".to_owned()),
            ("emulator".to_owned(), String::new()),
        ],
        slot: Some(1),
        reservation: Some(3),
    }
}

#[test]
fn a_lease_start_names_its_project_owner_worktree_basename_devices_slot_and_reservation() {
    let start = lease_start(&facts());
    numbers_are_listed(&start);
    assert_eq!(
        object(&start),
        json!({
            "action": "start",
            "lease": 7,
            "project": "demo",
            "owner": "claude:4242",
            "worktree": "demo-a",
            "devices": "phone:Pixel_8,emulator",
            "count": 2,
            "slot": 1,
            "reservation": 3,
        })
    );
}

#[test]
fn a_lease_with_no_project_device_or_reservation_leaves_those_fields_out() {
    let bare = LeaseFacts {
        project: None,
        devices: Vec::new(),
        slot: None,
        reservation: None,
        ..facts()
    };
    assert_eq!(
        object(&lease_start(&bare)),
        json!({"action": "start", "lease": 7, "owner": "claude:4242", "worktree": "demo-a", "count": 0})
    );
}

#[test]
fn a_lease_end_adds_how_long_it_lasted_how_long_it_ran_nothing_and_why_it_ended() {
    let end = lease_end(&facts(), 90, 60, LeaseEnd::RunEnd);
    numbers_are_listed(&end);
    let end = object(&end);
    assert_eq!(end["action"], "end");
    assert_eq!(end["seconds"], 90);
    assert_eq!(end["idle_seconds"], 60);
    assert_eq!(end["reason"], "run-end");
    let reasons: Vec<&str> = [
        LeaseEnd::RunEnd,
        LeaseEnd::Release,
        LeaseEnd::OwnerExit,
        LeaseEnd::Reclaim,
        LeaseEnd::SettleFailed,
    ]
    .iter()
    .map(|r| r.word())
    .collect();
    assert_eq!(
        reasons,
        [
            "run-end",
            "release",
            "owner-exit",
            "reclaim",
            "settle-failed"
        ]
    );
}

#[test]
fn a_leases_idle_time_is_its_time_minus_the_time_a_command_ran_on_it() {
    // Held 90.4 s, a command ran 30 s of it and another still runs for the last 0.4 s.
    assert_eq!(spans(0, 30_000, Some(90_000), 90_400), (90, 60));
    // A clock that went back never makes a negative span.
    assert_eq!(spans(5_000, 0, None, 4_000), (0, 0));
    // Busy time can't pass the lease's own.
    assert_eq!(spans(0, 50_000, None, 10_000), (10, 0));
}

#[test]
fn a_phone_wait_reason_keeps_the_busy_and_unready_counts() {
    let why = "no free phone: 2 phone(s) busy, 1 locked (unlock it once), 1 asleep (wake it), \
               1 stay-awake off (replug it), 3 offline, 1 unauthorized (tap Allow on the phone)";
    let fields = wait_reason(why);
    numbers_are_listed(&fields);
    assert_eq!(
        object(&fields),
        json!({
            "wait": "phones",
            "busy": 2,
            "locked": 1,
            "asleep": 1,
            "stay_awake_off": 1,
            "offline": 3,
            "unauthorized": 1,
        })
    );
}

#[test]
fn a_wait_on_a_cap_says_which_cap_and_its_size() {
    assert_eq!(
        object(&wait_reason(
            "no free phone (0 phone(s) busy, 1 locked (unlock it once)) and the emulator cap (2) is reached"
        )),
        json!({"wait": "emulator-cap", "busy": 0, "locked": 1, "cap": 2})
    );
    assert_eq!(
        object(&wait_reason(
            "owner claude:1 already holds 2 device(s); 1 more would pass its cap of 2 (maxDevicesPerOwner; waits for one of its own leases to end)"
        )),
        json!({"wait": "device-cap", "cap": 2})
    );
    assert_eq!(
        object(&wait_reason(
            "owner claude:1 already holds 1 of 2 demo stack slots; cap 1 (waits for one of its own leases to end)"
        )),
        json!({"wait": "slot-cap", "cap": 1})
    );
}

#[test]
fn other_wait_reasons_are_named_by_kind_and_keep_no_text() {
    let kinds: Vec<Value> = [
        "queued behind claude:0 @ wt0 (waiting since 0 min)",
        "all 2 stack slots of demo are held",
        "no free phone (1 phone(s) busy) and too little memory for an emulator (100 MB free, need 4096)",
        "adb is restarting",
        "a phone is being probed again",
        "something new",
        "",
    ]
    .iter()
    .map(|why| object(&wait_reason(why))["wait"].clone())
    .collect();
    assert_eq!(
        kinds,
        [
            "queue", "slots", "memory", "adb", "reprobe", "other", "none"
        ]
    );
}

#[test]
fn a_wait_says_what_was_asked_how_long_it_waited_and_its_last_reason() {
    let asked = Asked {
        mode: Some("phone".to_owned()),
        count: 1,
        stack: true,
    };
    let gave_up = wait_gave_up(4, &asked, 12, "no free phone: 1 phone(s) busy");
    numbers_are_listed(&gave_up);
    assert_eq!(
        object(&gave_up),
        json!({"action": "gave-up", "waiter": 4, "asked": "phone", "count": 1, "stack": "yes", "seconds": 12, "wait": "phones", "busy": 1})
    );
    let none = Asked {
        mode: None,
        count: 0,
        stack: false,
    };
    assert_eq!(
        object(&wait_granted(5, &none, 0, None)),
        json!({"action": "granted", "waiter": 5, "asked": "none", "count": 0, "stack": "no", "seconds": 0, "wait": "none"})
    );
}

#[test]
fn a_reclaim_names_the_lease_device_and_idle_minutes() {
    let fields = reclaim(7, "phone", "Pixel_8", 12);
    numbers_are_listed(&fields);
    assert_eq!(
        object(&fields),
        json!({"lease": 7, "kind": "phone", "model": "Pixel_8", "idle_minutes": 12})
    );
}

#[test]
fn a_readiness_change_names_the_phone_by_model_and_a_short_hash_never_its_serial() {
    let fields = readiness(
        "Pixel_8",
        "FAKEPHONE01",
        Some(Readiness::Locked),
        Readiness::StayAwakeOff,
        true,
    );
    numbers_are_listed(&fields);
    let id = short_id("FAKEPHONE01");
    assert_eq!(
        object(&fields),
        json!({"model": "Pixel_8", "phone": id, "from": "locked", "to": "stay-awake-off", "notified": "true"})
    );
    assert!(!object(&fields).to_string().contains("FAKEPHONE01"));
    let first = object(&readiness("", "X", None, Readiness::Ready, false));
    assert_eq!(
        (first["from"].clone(), first["notified"].clone()),
        (json!("none"), json!("false"))
    );
}

#[test]
fn a_short_id_is_the_first_8_hex_of_the_serials_sha256() {
    // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    assert_eq!(short_id("abc"), "ba7816bf");
    assert_eq!(short_id("FAKEPHONE01").len(), 8);
}

#[test]
fn a_worktree_named_as_the_user_or_their_home_dir_is_shown_as_a_tilde() {
    let at = |worktree: &str| {
        let f = LeaseFacts {
            worktree: worktree.to_owned(),
            ..facts()
        };
        object(&lease_start(&f))["worktree"].clone()
    };
    assert_eq!(at("/home/someone"), "~");
    assert_eq!(at("/home/someone/"), "~");
    assert_eq!(at("/mnt/backup/someone"), "~");
    assert_eq!(at("/home/someone/src/app"), "app");
}

#[test]
fn the_counts_are_read_from_the_wait_text_the_planner_writes() {
    let input = PlanInput {
        want: 1,
        mode: Mode::Phone,
        busy_phones: 2,
        unready: Unready {
            locked: 1,
            asleep: 1,
            stay_awake_off: 1,
            offline: 1,
            unauthorized: 1,
        },
        ..PlanInput::default()
    };
    let why = plan_devices(&input).expect_err("no phone is free");
    assert_eq!(
        object(&wait_reason(&why)),
        json!({"wait": "phones", "busy": 2, "locked": 1, "asleep": 1, "stay_awake_off": 1, "offline": 1, "unauthorized": 1})
    );
    let capped = PlanInput {
        mode: Mode::Any,
        max_emulators: 3,
        emulators_running: 3,
        ..input
    };
    let why = plan_devices(&capped).expect_err("the cap is reached");
    assert_eq!(object(&wait_reason(&why))["wait"], "emulator-cap");
    assert_eq!(object(&wait_reason(&why))["cap"], 3);
}
