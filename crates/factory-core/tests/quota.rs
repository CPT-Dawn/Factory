//! The quota state machine, the router's pick and the out-of-quota readers (spec #1,
//! "Reviewer router").

use factory_core::quota::{self, AgyUsage, Candidate, Pick, Quota, Reset, WhenBusy};
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
fn a_finished_review_does_not_clear_a_cooldown_another_review_hit() {
    let cooling = Quota::CoolingDown { until: at(T11_00) };
    let cases = [
        // (known, learned, now, then)
        (cooling, Quota::Available, T10_00, cooling),
        (cooling, Quota::Available, T13_00, Quota::Available),
        (Quota::Available, cooling, T10_00, cooling),
        (Quota::Unknown, Quota::Available, T10_00, Quota::Available),
        (
            cooling,
            Quota::CoolingDown { until: at(T13_00) },
            T10_00,
            Quota::CoolingDown { until: at(T13_00) },
        ),
    ];
    for (known, learned, now, then) in cases {
        assert_eq!(
            known.learn(learned, at(now)),
            then,
            "{known:?} {learned:?} {now}"
        );
    }
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
        // What Codex read while reviewing is not its own error.
        (
            concat!(
                "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"aggregated_output\":\"you hit your usage limit, try again at 3:00 PM\"}}\n",
                "{\"type\":\"turn.failed\",\"error\":{\"message\":\"stream disconnected\"}}\n",
            ),
            None,
        ),
        (
            concat!(
                "{\"type\":\"item.completed\",\"item\":{\"text\":\"try again at 3:00 PM\"}}\n",
                "{\"type\":\"error\",\"message\":\"You've hit your usage limit.\"}\n",
            ),
            Some(Reset::Unknown),
        ),
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

/// What `agy -p /usage --output-format json` printed on 2026-11-23, with 14% of the Gemini
/// five-hour bucket left.
const AGY_USAGE: &str = include_str!("fixtures/agy-usage.json");

/// The real usage with these buckets' `remaining_fraction` set to 0.
fn agy_usage_with_empty(ids: &[&str]) -> String {
    let mut usage: serde_json::Value = serde_json::from_str(AGY_USAGE).unwrap_or_default();
    let groups = usage
        .pointer_mut("/command/data/groups")
        .and_then(serde_json::Value::as_array_mut);
    let buckets = groups
        .into_iter()
        .flatten()
        .filter_map(|group| group.get_mut("buckets")?.as_array_mut())
        .flatten();
    for bucket in buckets {
        let id = bucket.get("id").and_then(serde_json::Value::as_str);
        if id.is_some_and(|id| ids.contains(&id)) {
            bucket["remaining_fraction"] = 0.into();
        }
    }
    usage.to_string()
}

#[test]
fn agys_usage_says_whether_a_model_groups_buckets_have_quota_left() {
    let gemini = "Gemini Models";
    let cases = [
        ("14% left", AGY_USAGE.to_owned(), gemini, AgyUsage::Left),
        (
            "five-hour empty",
            agy_usage_with_empty(&["gemini-5h"]),
            gemini,
            AgyUsage::Empty {
                until: Timestamp::parse("2026-11-23T05:21:20Z").unwrap(),
            },
        ),
        (
            "both empty: the later reset",
            agy_usage_with_empty(&["gemini-5h", "gemini-weekly"]),
            gemini,
            AgyUsage::Empty {
                until: Timestamp::parse("2026-11-30T00:21:20Z").unwrap(),
            },
        ),
        (
            "another group's empty bucket",
            agy_usage_with_empty(&["gemini-5h"]),
            "Claude and GPT models",
            AgyUsage::Left,
        ),
    ];
    for (case, usage, group, said) in cases {
        assert_eq!(quota::agy_usage(&usage, group), said, "{case}");
    }
}

#[test]
fn agys_usage_that_names_no_buckets_for_the_group_is_unreadable() {
    let failed = r#"{"conversation_id":"","status":"ERROR","response":"","error":"Eligibility check failed"}"#;
    for (case, usage) in [
        ("no such group", AGY_USAGE),
        ("not json", "Eligibility check failed"),
        ("an error", failed),
        (
            "no reset",
            r#"{"command":{"data":{"groups":[{"name":"G","buckets":[{"remaining_fraction":0}]}]}}}"#,
        ),
    ] {
        let group = if case == "no reset" {
            "G"
        } else {
            "Gemini 4 Carbon"
        };
        assert!(
            matches!(quota::agy_usage(usage, group), AgyUsage::Unreadable(_)),
            "{case}"
        );
    }
}

#[test]
fn an_agy_model_draws_on_its_groups_quota() {
    let cases = [
        ("gemini-4-neon", "Gemini Models"),
        ("gemini-4-neon-high", "Gemini Models"),
        ("gemini-3.8-flash-low", "Gemini Models"),
        ("gemini-4-argon-high", "Gemini 4 Argon"),
        ("claude-opus-5-5-thinking", "Claude and GPT models"),
        ("gpt-oss-120b-medium", "Claude and GPT models"),
    ];
    for (model, group) in cases {
        assert_eq!(quota::agy_group(model), group, "{model}");
    }
}

#[test]
fn agys_individual_quota_error_is_read_with_the_time_until_it_resets() {
    let agy_error = |message: &str| {
        format!(
            "[agy] turn failed\nAGY_ERROR: {}\n",
            serde_json::json!({"status": "RESOURCE_EXHAUSTED", "code": 429, "retryable": false, "message": message})
        )
    };
    let reached =
        "Individual quota reached. Please upgrade your subscription to increase your limits.";
    let failed = |error: &str| {
        serde_json::json!({"status": "ERROR", "response": "", "error": error, "finished_reason": "error"}).to_string()
    };
    let cases = [
        (
            "stderr",
            String::new(),
            agy_error(&format!("{reached} Resets in 2h13m5s.")),
            Some(Reset::In { millis: 7_985_000 }),
        ),
        (
            "result json",
            failed(&format!("{reached} Resets in 45m.")),
            String::new(),
            Some(Reset::In { millis: 2_700_000 }),
        ),
        (
            "days",
            String::new(),
            agy_error(&format!("{reached} Resets in 1d2h.")),
            Some(Reset::In { millis: 93_600_000 }),
        ),
        (
            "no time it can read",
            String::new(),
            agy_error(&format!("{reached} Resets in a while.")),
            Some(Reset::Unknown),
        ),
        ("no reset named", String::new(), agy_error(reached), None),
        (
            "another error",
            failed("INTERNAL: stream closed"),
            agy_error("stream closed"),
            None,
        ),
        // What agy reviewed is not its own error.
        (
            "in the response",
            serde_json::json!({"status": "ERROR", "response": format!("{reached} Resets in 1h."), "error": "boom"}).to_string(),
            format!("[agy] read {reached} Resets in 1h.\n"),
            None,
        ),
    ];
    for (case, result, stderr, limit) in cases {
        assert_eq!(quota::agy_limit(&result, &stderr), limit, "{case}");
    }
}

#[test]
fn a_reset_after_a_while_or_at_a_moment_is_that_moment() {
    assert_eq!(
        Reset::In { millis: 3_600_000 }.until(at(T10_00), 3600),
        at(T11_00)
    );
    assert_eq!(Reset::On(at(T13_00)).until(at(T10_00), -18_000), at(T13_00));
}
