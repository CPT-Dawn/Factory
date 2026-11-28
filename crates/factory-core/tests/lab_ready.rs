//! Whether a phone can run unattended (#55): what adb's probes say of it, and the one
//! notification each change of that earns.

use factory_core::lab::ready::{
    EpochMs, Probe, Readiness, Urgency, after, auth_timeout_warning, check_line, for_grant, judge,
    notice, probe_again, refresh_again, unplugged,
};

/// An instant a probe could be made at: 2026-11-24, in ms since the epoch.
const AT: EpochMs = EpochMs(1_764_000_000_000);

/// `ms` after [AT].
fn later(ms: i64) -> EpochMs {
    EpochMs(AT.0 + ms)
}

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
fn a_battery_a_test_faked_is_named_as_the_cause_not_stay_awake() {
    // `dumpsys battery unplug` stops its updates and says each source is off.
    let faked = Probe {
        battery: Some(
            "  (UPDATES STOPPED -- use 'reset' to restart)\n  AC powered: false\n  USB powered: false\n"
                .to_owned(),
        ),
        ..ready_phone()
    };
    assert_eq!(judge(&faked), Readiness::BatteryFaked);
    assert_eq!(Readiness::BatteryFaked.word(), "battery faked");
    assert_eq!(
        Readiness::from_word("battery faked"),
        Some(Readiness::BatteryFaked)
    );
    assert_eq!(
        check_line("FAKEPHONE01", "", Some(Readiness::BatteryFaked), None),
        "FAKEPHONE01: battery faked: a test left dumpsys battery faking its power; run devlab prep FAKEPHONE01"
    );
    let told = notice("Pixel 8", None, Readiness::BatteryFaked).unwrap();
    assert_eq!(told.summary, "Pixel 8's battery is faked");
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
    // adb lost it between two probes: what it can't tell is not taken as fine.
    let dropped = Probe {
        battery: None,
        power: None,
        lock: None,
        ..ready_phone()
    };
    assert_eq!(judge(&dropped), Readiness::Offline);
    for one_lost in [
        Probe {
            lock: None,
            ..ready_phone()
        },
        Probe {
            power: None,
            ..ready_phone()
        },
        Probe {
            battery: None,
            ..ready_phone()
        },
        Probe {
            stay_on: None,
            ..ready_phone()
        },
    ] {
        assert_eq!(judge(&one_lost), Readiness::Offline, "{one_lost:?}");
    }
}

#[test]
fn a_readiness_serializes_as_its_word() {
    for r in [
        Readiness::Ready,
        Readiness::Locked,
        Readiness::Asleep,
        Readiness::StayAwakeOff,
        Readiness::Unauthorized,
        Readiness::Offline,
    ] {
        assert_eq!(serde_json::to_value(r).unwrap(), r.word());
        assert_eq!(Readiness::from_word(r.word()), Some(r));
        assert_eq!(
            serde_json::from_value::<Readiness>(r.word().into()).unwrap(),
            r
        );
    }
}

#[test]
fn a_phone_on_no_power_cannot_stay_awake() {
    let unpowered = Probe {
        battery: Some(
            "  AC powered: false\n  USB powered: false\n  Wireless powered: false\n".to_owned(),
        ),
        ..ready_phone()
    };
    assert_eq!(judge(&unpowered), Readiness::StayAwakeOff);
    // A battery report that names no source at all is a phone on the USB adb reaches it by.
    let unnamed = Probe {
        battery: Some(String::new()),
        ..ready_phone()
    };
    assert_eq!(judge(&unnamed), Readiness::Ready);
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

#[test]
fn a_grant_probes_a_ready_phone_again_after_10_s_and_one_that_is_not_ready_every_time() {
    assert!(!probe_again(Readiness::Ready, 0));
    assert!(!probe_again(Readiness::Ready, 10_000));
    assert!(probe_again(Readiness::Ready, 10_001));
    assert!(probe_again(Readiness::Locked, 0));
    assert!(probe_again(Readiness::Offline, 5_000));
}

#[test]
fn a_grant_takes_a_phone_adb_cannot_use_as_adb_shows_it_whatever_its_last_probe_found() {
    assert_eq!(
        for_grant("unauthorized", Some((Readiness::Locked, 0))),
        Some(Readiness::Unauthorized)
    );
    assert_eq!(
        for_grant("unauthorized", None),
        Some(Readiness::Unauthorized)
    );
    assert_eq!(
        for_grant("offline", Some((Readiness::Ready, 0))),
        Some(Readiness::Offline)
    );
}

#[test]
fn a_grant_believes_a_usable_phone_s_probe_and_a_ready_one_for_10_s() {
    assert_eq!(for_grant("device", None), Some(Readiness::Offline));
    assert_eq!(
        for_grant("device", Some((Readiness::Locked, 60_000))),
        Some(Readiness::Locked)
    );
    assert_eq!(
        for_grant("device", Some((Readiness::Ready, 10_000))),
        Some(Readiness::Ready)
    );
    // Too old to grant on: the grant has it probed again.
    assert_eq!(for_grant("device", Some((Readiness::Ready, 10_001))), None);
    // Minutes old, as a probe of a phone locked since is.
    assert_eq!(for_grant("device", Some((Readiness::Ready, 300_000))), None);
}

#[test]
fn an_instant_says_how_many_ms_after_an_earlier_one_it_is() {
    assert_eq!(later(2_500).since(AT), 2_500);
    assert_eq!(AT.since(later(2_500)), -2_500);
}

#[test]
fn a_grant_s_refresh_probes_a_ready_phone_again_2_s_before_its_probe_is_too_old() {
    assert!(!refresh_again(Readiness::Ready, 8_000));
    assert!(refresh_again(Readiness::Ready, 8_001));
    assert!(refresh_again(Readiness::Locked, 0));
}
