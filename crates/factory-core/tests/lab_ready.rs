//! Whether a phone can run unattended (#55): what adb's probes say of it, and the one
//! notification each change of that earns.

use factory_core::lab::ready::{
    Probe, Readiness, Urgency, after, auth_timeout_warning, check_line, judge, notice, unplugged,
};

/// A phone on USB power with stay-awake set, awake and unlocked.
fn ready_phone() -> Probe {
    Probe {
        state: "device".to_owned(),
        stay_on: Some("7\n".to_owned()),
        battery: Some(
            "  AC powered: false\n  USB powered: true\n  Wireless powered: false\n".to_owned(),
        ),
        power: Some("  mWakefulness=Awake\n".to_owned()),
        lock: Some(String::new()),
    }
}

#[test]
fn a_phone_awake_unlocked_and_kept_awake_on_its_power_source_is_ready() {
    assert_eq!(judge(&ready_phone()), Readiness::Ready);
}

#[test]
fn a_keyguard_makes_it_locked_and_a_screen_not_awake_makes_it_asleep() {
    let locked = Probe {
        lock: Some("  mDreamingLockscreen=true\n".to_owned()),
        ..ready_phone()
    };
    assert_eq!(judge(&locked), Readiness::Locked);
    let asleep = Probe {
        power: Some("  mWakefulness=Asleep\n".to_owned()),
        ..ready_phone()
    };
    assert_eq!(judge(&asleep), Readiness::Asleep);
}

#[test]
fn stay_awake_must_cover_the_power_source_the_phone_is_on() {
    // 2 keeps it awake on USB alone: on AC it would sleep.
    let usb_only = Probe {
        stay_on: Some("2".to_owned()),
        ..ready_phone()
    };
    assert_eq!(judge(&usb_only), Readiness::Ready);
    let on_ac = Probe {
        battery: Some("  AC powered: true\n  USB powered: false\n".to_owned()),
        ..usb_only
    };
    assert_eq!(judge(&on_ac), Readiness::StayAwakeOff);
    for off in ["0", "null"] {
        let off = Probe {
            stay_on: Some(off.to_owned()),
            ..ready_phone()
        };
        assert_eq!(judge(&off), Readiness::StayAwakeOff);
    }
}

#[test]
fn adbs_own_word_wins_and_a_phone_adb_cannot_reach_is_offline() {
    for (state, readiness) in [
        ("unauthorized", Readiness::Unauthorized),
        ("offline", Readiness::Offline),
    ] {
        let probe = Probe {
            state: state.to_owned(),
            ..ready_phone()
        };
        assert_eq!(judge(&probe), readiness);
    }
    let unreachable = Probe {
        state: "device".to_owned(),
        stay_on: None,
        battery: None,
        power: None,
        lock: None,
    };
    assert_eq!(judge(&unreachable), Readiness::Offline);
}

#[test]
fn stay_awake_off_is_said_before_the_lock_it_will_bring_back() {
    let both = Probe {
        stay_on: Some("0".to_owned()),
        lock: Some("isKeyguardShowing=true".to_owned()),
        ..ready_phone()
    };
    assert_eq!(judge(&both), Readiness::StayAwakeOff);
}

#[test]
fn a_phone_plugged_locked_asks_once_to_be_unlocked_and_says_ready_once_it_is() {
    let plugged = notice("Pixel 8", None, Readiness::Locked).unwrap();
    assert_eq!(
        (
            plugged.summary.as_str(),
            plugged.body.as_str(),
            plugged.urgency
        ),
        (
            "Pixel 8 needs unlocking",
            "Unlock it once: it stays awake and unlocked while plugged in.",
            Urgency::Critical
        )
    );
    let locked = after(None, Readiness::Locked);
    assert_eq!(notice("Pixel 8", Some(locked), Readiness::Locked), None);
    let ready = notice("Pixel 8", Some(locked), Readiness::Ready).unwrap();
    assert_eq!(ready.summary, "Pixel 8 ready");
    assert_ne!(ready.urgency, Urgency::Critical);
    let ready_known = after(Some(locked), Readiness::Ready);
    assert_eq!(notice("Pixel 8", Some(ready_known), Readiness::Ready), None);
}

#[test]
fn a_phone_that_was_ready_says_it_locked_again_and_then_that_it_is_ready_again() {
    let ready = after(None, Readiness::Ready);
    let relocked = notice("Pixel 8", Some(ready), Readiness::Locked).unwrap();
    assert_eq!(
        (relocked.summary.as_str(), relocked.urgency),
        ("Pixel 8 locked again", Urgency::Critical)
    );
    assert!(relocked.body.contains("Unlock it once"), "{relocked:?}");
    let locked = after(Some(ready), Readiness::Locked);
    let back = notice("Pixel 8", Some(locked), Readiness::Ready).unwrap();
    assert_eq!(back.summary, "Pixel 8 ready again");
}

#[test]
fn usb_debugging_to_allow_is_critical_and_says_where_to_tap() {
    let n = notice("FAKEPHONE01", None, Readiness::Unauthorized).unwrap();
    assert_eq!(
        (n.summary.as_str(), n.urgency),
        ("FAKEPHONE01: allow USB debugging", Urgency::Critical)
    );
    assert!(n.body.contains("tap Allow"), "{n:?}");
}

#[test]
fn an_unplug_is_critical_only_while_a_lease_holds_the_phone() {
    let leased = unplugged("Pixel 8", Some("lease 3 (demo-a)"));
    assert_eq!(
        (
            leased.summary.as_str(),
            leased.body.as_str(),
            leased.urgency
        ),
        (
            "Pixel 8 unplugged",
            "lease 3 (demo-a) was using it; its runs fail until it is back.",
            Urgency::Critical
        )
    );
    let free = unplugged("Pixel 8", None);
    assert_eq!(
        (free.summary.as_str(), free.urgency),
        ("Pixel 8 unplugged", Urgency::Low)
    );
}

#[test]
fn a_check_line_says_ready_or_what_is_wrong_and_what_to_do() {
    assert_eq!(
        check_line("FAKEPHONE01", "Pixel 8", Some(Readiness::Ready), None),
        "FAKEPHONE01 (Pixel 8): ready"
    );
    assert_eq!(
        check_line("FAKEPHONE01", "Pixel 8", Some(Readiness::Locked), None),
        "FAKEPHONE01 (Pixel 8): locked: unlock it once; it then stays awake and unlocked while plugged in"
    );
    assert_eq!(
        check_line("FAKEPHONE02", "", None, None),
        "FAKEPHONE02: not attached: plug it in and allow USB debugging"
    );
    assert_eq!(
        check_line("FAKEPHONE01", "", Some(Readiness::StayAwakeOff), None),
        "FAKEPHONE01: stay-awake off: replug it, or run devlab prep FAKEPHONE01"
    );
}

#[test]
fn an_authorization_timeout_left_on_is_a_warning_on_a_ready_phone() {
    assert_eq!(auth_timeout_warning(Some("0\n")), None);
    let warning = auth_timeout_warning(Some("null")).unwrap();
    assert_eq!(
        check_line("FAKEPHONE01", "", Some(Readiness::Ready), Some(warning)),
        format!("FAKEPHONE01: ready; {warning}")
    );
    assert!(
        warning.contains("Disable adb authorization timeout"),
        "{warning}"
    );
}
