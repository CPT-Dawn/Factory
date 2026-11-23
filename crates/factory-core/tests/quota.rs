//! The quota state machine, the router's pick and the out-of-quota readers (spec #1,
//! "Reviewer router").

use factory_core::quota::{self, Candidate, Pick, Quota, Reset, WhenBusy};
use factory_core::timestamp::Timestamp;

/// Seconds since the epoch, from `date -u -d <moment> +%s`.
fn at(seconds: i64) -> Timestamp {
    Timestamp::from_unix_millis(seconds * 1000)
}

const T10_00: i64 = 1_795_428_000; // 2026-11-23T10:00:00Z
const T11_00: i64 = 1_795_431_600; // 2026-11-23T11:00:00Z
const T13_00: i64 = 1_795_438_800; // 2026-11-23T13:00:00Z

fn idle(quota: Quota) -> Candidate {
    Candidate {
        quota,
        running: 0,
        concurrency: 1,
        when_busy: WhenBusy::Queue,
    }
}

fn full(when_busy: WhenBusy) -> Candidate {
    Candidate {
        quota: Quota::Unknown,
        running: 1,
        concurrency: 1,
        when_busy,
    }
}

#[test]
fn a_cooldown_holds_until_its_reset_and_then_the_quota_is_unknown_again() {
    let cooling = Quota::CoolingDown { until: at(T11_00) };

    assert_eq!(cooling.at(at(T10_00)), cooling);
    assert_eq!(cooling.at(at(T11_00)), Quota::Unknown);
    assert_eq!(Quota::Available.at(at(T13_00)), Quota::Available);
    assert_eq!(Quota::Unknown.at(at(T13_00)), Quota::Unknown);
}

#[test]
fn the_router_takes_the_first_backend_with_quota_and_a_free_slot_or_says_why_not() {
    let cooling = |seconds| Quota::CoolingDown { until: at(seconds) };
    let cases: Vec<(&str, Vec<Candidate>, Pick)> = vec![
        (
            "first free",
            vec![idle(Quota::Unknown), idle(Quota::Available)],
            Pick::Take(0),
        ),
        (
            "first cooling",
            vec![idle(cooling(T11_00)), idle(Quota::Unknown)],
            Pick::Take(1),
        ),
        (
            "cooldown over",
            vec![idle(cooling(T10_00)), idle(Quota::Unknown)],
            Pick::Take(0),
        ),
        (
            "busy, next",
            vec![full(WhenBusy::Next), idle(Quota::Unknown)],
            Pick::Take(1),
        ),
        (
            "busy, queue",
            vec![full(WhenBusy::Queue), idle(Quota::Unknown)],
            Pick::Wait,
        ),
        (
            "all busy",
            vec![full(WhenBusy::Next), full(WhenBusy::Next)],
            Pick::Wait,
        ),
        (
            "busy then cooling",
            vec![full(WhenBusy::Next), idle(cooling(T13_00))],
            Pick::Wait,
        ),
        (
            "all cooling",
            vec![idle(cooling(T13_00)), idle(cooling(T11_00))],
            Pick::OutOfQuota {
                until: Some(at(T11_00)),
            },
        ),
        ("none", vec![], Pick::OutOfQuota { until: None }),
    ];
    for (case, candidates, picked) in cases {
        assert_eq!(quota::pick(&candidates, at(T10_00)), picked, "{case}");
    }
}

#[test]
fn a_concurrency_above_one_lets_that_many_reviews_run() {
    let two = Candidate {
        quota: Quota::Unknown,
        running: 1,
        concurrency: 2,
        when_busy: WhenBusy::Queue,
    };

    assert_eq!(quota::pick(&[two], at(T10_00)), Pick::Take(0));
}

#[test]
fn a_codex_usage_limit_is_read_from_its_event_stream() {
    let real = concat!(
        "{\"type\":\"thread.started\"}\n",
        "{\"type\":\"error\",\"message\":\"You\u{2019}ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at 12:39 PM.\"}\n",
        "{\"type\":\"turn.failed\",\"error\":{\"message\":\"You\u{2019}ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at 12:39 PM.\"}}\n",
    );
    let cases = [
        (
            real,
            Some(Reset::At {
                hour: 12,
                minute: 39,
            }),
        ),
        (
            "usage limit ... or try again at 1:55 AM.",
            Some(Reset::At {
                hour: 1,
                minute: 55,
            }),
        ),
        (
            "usage limit ... or try again at 12:05 AM.",
            Some(Reset::At { hour: 0, minute: 5 }),
        ),
        ("You've hit your usage limit.", Some(Reset::Unknown)),
        ("{\"type\":\"turn.completed\"}", None),
    ];
    for (stream, limit) in cases {
        assert_eq!(quota::codex_limit(stream), limit, "{stream}");
    }
}

#[test]
fn a_reset_time_of_day_is_its_next_occurrence_in_local_time() {
    let half_past_noon = Reset::At {
        hour: 12,
        minute: 39,
    };
    let cases = [
        // (now, UTC offset in seconds, until)
        (T10_00, 0, 1_795_437_540),       // 2026-11-23T12:39:00Z
        (T13_00, 0, 1_795_523_940),       // 2026-11-24T12:39:00Z: today's has passed
        (T10_00, 3600, 1_795_433_940),    // 2026-11-23T11:39:00Z: 12:39 at UTC+1
        (T10_00, -18_000, 1_795_455_540), // 2026-11-23T17:39:00Z: 12:39 at UTC-5
    ];
    for (now, offset, until) in cases {
        assert_eq!(
            half_past_noon.until(at(now), offset),
            at(until),
            "{now} {offset}"
        );
    }
    assert_eq!(
        Reset::At { hour: 0, minute: 5 }.until(at(T10_00), 0),
        at(1_795_478_700) // 2026-11-24T00:05:00Z
    );
}

#[test]
fn an_unreadable_reset_cools_the_backend_down_for_an_hour() {
    assert_eq!(Reset::Unknown.until(at(T10_00), 3600), at(T11_00));
}
