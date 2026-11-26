//! The quota state machine, the router's pick and the out-of-quota readers (spec #1,
//! "Reviewer router").

use factory_core::quota::{self, AgyUsage, Candidate, Pick, Quota, Reset, WhenBusy};
use factory_core::timestamp::Timestamp;
use factory_core::usage::{GroupWindow, QuotaGroup, WindowKind, WindowState};

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

/// What `agy -p /usage --output-format json` printed on 2026-11-24 (agy 1.5.15), with 70% of
/// the Gemini five-hour bucket left: Argon is one of the "Gemini Models".
const AGY_USAGE: &str = include_str!("fixtures/agy-usage.json");

/// What it printed on 2026-11-23, in the old shape: Gemini 4 Argon a group of its own, which
/// lists no models.
const AGY_USAGE_OLD: &str = include_str!("fixtures/agy-usage-2026-11-23.json");

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
        ("70% left", AGY_USAGE.to_owned(), gemini, AgyUsage::Left),
        (
            "five-hour empty",
            agy_usage_with_empty(&["gemini-5h"]),
            gemini,
            AgyUsage::Empty {
                until: Timestamp::parse("2026-11-24T03:32:03Z").unwrap(),
            },
        ),
        (
            "both empty: the later reset",
            agy_usage_with_empty(&["gemini-5h", "gemini-weekly"]),
            gemini,
            AgyUsage::Empty {
                until: Timestamp::parse("2026-11-30T22:32:03Z").unwrap(),
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
fn an_agy_model_draws_on_the_group_whose_model_list_names_it() {
    let cases = [
        ("gemini-4-neon", "Gemini Models"),
        ("gemini-4-neon-high", "Gemini Models"),
        ("gemini-4-argon-high", "Gemini Models"),
        ("gemini-4-argon-low", "Gemini Models"),
        ("gemini-4-carbon-medium", "Gemini Models"),
        ("gemini-4-hydrogen-high", "Gemini Models"),
        ("gemini-3.8-flash-low", "Gemini Models"),
        ("claude-opus-5-5-high", "Claude and GPT models"),
        ("claude-sonnet-5-5-thinking", "Claude and GPT models"),
        ("gpt-oss-120b-medium", "Claude and GPT models"),
        // Labels, as agy's `/model` saves them in its settings.
        ("Gemini 4 Neon (High)", "Gemini Models"),
        ("Gemini 4 Argon (Low)", "Gemini Models"),
        ("Claude Opus 5.5 (Thinking)", "Claude and GPT models"),
        ("GPT-OSS 120B (Medium)", "Claude and GPT models"),
    ];
    for (model, group) in cases {
        assert_eq!(quota::agy_group(AGY_USAGE, model), group, "{model}");
    }
}

#[test]
fn in_the_old_answer_argon_draws_on_the_group_named_for_it() {
    let cases = [
        ("gemini-4-argon-high", "Gemini 4 Argon"),
        ("Gemini 4 Argon (Low)", "Gemini 4 Argon"),
        ("gemini-4-neon-high", "Gemini Models"),
        ("gemini-3.8-flash-high", "Gemini Models"),
        ("claude-opus-5-5-high", "Claude and GPT models"),
    ];
    for (model, group) in cases {
        assert_eq!(quota::agy_group(AGY_USAGE_OLD, model), group, "{model}");
    }
}

#[test]
fn an_answer_that_lists_no_models_leaves_the_group_to_the_static_map() {
    let unlisted = r#"{"command": {"data": {"groups": [
        {"name": "Gemini Models", "buckets": []},
        {"name": "Claude and GPT models", "buckets": []}]}}}"#;
    let cases = [
        ("gemini-4-neon-high", "Gemini Models"),
        ("gemini-4-argon-high", "Gemini Models"),
        ("Gemini 4 Argon (High)", "Gemini Models"),
        ("claude-opus-5-5-high", "Claude and GPT models"),
        ("GPT-OSS 120B (Medium)", "Claude and GPT models"),
    ];
    for usage in [unlisted, "not json", "{}"] {
        for (model, group) in cases {
            assert_eq!(quota::agy_group(usage, model), group, "{model} in {usage}");
        }
    }
}

#[test]
fn a_model_no_list_names_falls_back_to_the_static_map() {
    assert_eq!(
        quota::agy_group(AGY_USAGE, "gemini-5-zinc-high"),
        "Gemini Models"
    );
    assert_eq!(
        quota::agy_group(AGY_USAGE, "claude-haiku-5-low"),
        "Claude and GPT models"
    );
}

#[test]
fn with_no_model_configured_agy_draws_on_the_model_its_settings_chose_else_its_default() {
    let cases = [
        (
            Some(r#"{"model": "Claude Opus 5.5 (Thinking)"}"#),
            "Claude Opus 5.5 (Thinking)",
        ),
        (
            Some(r#"{"model": " Gemini 4 Argon (High) "}"#),
            "Gemini 4 Argon (High)",
        ),
        (Some(r#"{"model": ""}"#), "gemini-4-neon"),
        (Some(r#"{"trustedWorkspaces": ["~"]}"#), "gemini-4-neon"),
        (Some(r#"{"model": 4}"#), "gemini-4-neon"),
        (Some("not json"), "gemini-4-neon"),
        (None, "gemini-4-neon"),
    ];
    for (settings, model) in cases {
        assert_eq!(quota::agy_chosen_model(settings), model, "{settings:?}");
    }
    assert_eq!(
        quota::agy_group(AGY_USAGE, quota::AGY_DEFAULT_MODEL),
        "Gemini Models"
    );
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

#[test]
fn a_groups_bucket_under_a_fraction_is_below_its_reserve() {
    let usage = r#"{"command": {"data": {"groups": [
        {"name": "Gemini Models", "buckets": [
            {"window": "weekly", "remaining_fraction": 0.1},
            {"window": "5h", "remaining_fraction": 0.19}]},
        {"name": "Gemini 4 Argon", "buckets": [{"window": "5h", "remaining_fraction": 0.5}]}]}}}"#;

    assert!(quota::agy_below(usage, "Gemini Models", "5h", 0.2));
    assert!(!quota::agy_below(usage, "Gemini Models", "5h", 0.19));
    assert!(!quota::agy_below(usage, "Gemini 4 Argon", "5h", 0.2));
    assert!(!quota::agy_below(usage, "Claude and GPT models", "5h", 0.2));
    assert!(!quota::agy_below("not json", "Gemini Models", "5h", 0.2));
}

/// The real usage with this bucket's fraction and reset set.
fn agy_usage_with(id: &str, left: f64, reset: &str) -> String {
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
        if bucket.get("id").and_then(serde_json::Value::as_str) == Some(id) {
            bucket["remaining_fraction"] = left.into();
            bucket["reset_time"] = reset.into();
        }
    }
    usage.to_string()
}

const EPOCH: &str = "1970-01-01T00:00:00Z";
const CLAUDE_GPT: &str = "Claude and GPT models";

#[test]
fn an_epoch_reset_leaves_the_group_available_only_when_the_whole_quota_is_left() {
    // agy 1.5.17 reports the untouched Claude and GPT group's windows with the epoch's reset.
    let untouched = agy_usage_with("3p-5h", 1.0, EPOCH);
    assert_eq!(quota::agy_usage(&untouched, CLAUDE_GPT), AgyUsage::Left);

    // Any less is a reading it cannot place: never "the reset has passed, so quota is back".
    for left in [0.0, 0.4, 0.999, 0.999_999_9] {
        let usage = agy_usage_with("3p-5h", left, EPOCH);
        assert!(
            matches!(
                quota::agy_usage(&usage, CLAUDE_GPT),
                AgyUsage::Unreadable(_)
            ),
            "{left} left with the epoch's reset: {:?}",
            quota::agy_usage(&usage, CLAUDE_GPT)
        );
    }
}

fn window(kind: WindowKind, state: WindowState) -> GroupWindow {
    GroupWindow {
        window: kind,
        state,
    }
}

fn used(percent: u32, resets: &str) -> WindowState {
    WindowState::Used {
        used_percent: percent,
        resets: Timestamp::parse(resets).map_or(0, Timestamp::unix_millis),
    }
}

#[test]
fn agys_usage_reads_each_quota_groups_five_hour_and_weekly_windows() {
    let groups = quota::agy_groups(AGY_USAGE).expect("the real answer reads");

    assert_eq!(
        groups,
        vec![
            QuotaGroup {
                name: "Gemini Models".to_owned(),
                windows: vec![
                    window(WindowKind::FiveHour, used(30, "2026-11-24T03:32:03Z")),
                    window(WindowKind::Weekly, used(7, "2026-11-30T22:32:03Z")),
                ],
            },
            QuotaGroup {
                name: CLAUDE_GPT.to_owned(),
                windows: vec![
                    window(WindowKind::FiveHour, used(24, "2026-11-24T03:33:56Z")),
                    window(WindowKind::Weekly, used(63, "2026-11-30T01:54:01Z")),
                ],
            },
        ]
    );
}

#[test]
fn a_whole_percent_is_not_lost_to_float_noise() {
    // 1 - 0.9 is 0.09999999999999998 in floats: 10% used all the same.
    let usage = agy_usage_with("gemini-5h", 0.9, "2026-11-24T03:32:03Z");
    let groups = quota::agy_groups(&usage).expect("it reads");
    assert_eq!(
        groups[0].windows[0],
        window(WindowKind::FiveHour, used(10, "2026-11-24T03:32:03Z"))
    );
}

#[test]
fn an_epoch_reset_is_an_unused_window_at_a_full_fraction_and_an_unknown_one_otherwise() {
    let usage = agy_usage_with("3p-5h", 1.0, EPOCH);
    let groups = quota::agy_groups(&usage).expect("it reads");
    assert_eq!(
        groups[1].windows[0],
        window(WindowKind::FiveHour, WindowState::Unused)
    );

    for left in [0.0, 0.4, 0.999] {
        let usage = agy_usage_with("3p-5h", left, EPOCH);
        let groups = quota::agy_groups(&usage).expect("it reads");
        assert_eq!(
            groups[1].windows[0],
            window(WindowKind::FiveHour, WindowState::Unknown),
            "{left} left"
        );
        // The group's other window is read as it is.
        assert_eq!(
            groups[1].windows[1],
            window(WindowKind::Weekly, used(63, "2026-11-30T01:54:01Z"))
        );
    }
}

/// An answer of one group `G` whose 5-hour bucket is `bucket`, beside a weekly one that reads.
fn with_5h_bucket(bucket: &str) -> String {
    format!(
        r#"{{"command":{{"data":{{"groups":[{{"name":"G","buckets":[{bucket},
        {{"window":"weekly","remaining_fraction":0.5,"reset_time":"2026-11-30T01:54:01Z"}}]}}]}}}}}}"#
    )
}

#[test]
fn a_bucket_with_no_fraction_or_no_reset_it_can_read_is_an_unknown_window() {
    for (case, bucket) in [
        (
            "no fraction",
            r#"{"window":"5h","reset_time":"2026-11-24T03:32:03Z"}"#,
        ),
        (
            "a fraction above 1",
            r#"{"window":"5h","remaining_fraction":1.5,"reset_time":"2026-11-24T03:32:03Z"}"#,
        ),
        ("no reset", r#"{"window":"5h","remaining_fraction":0.5}"#),
        (
            "a garbled reset",
            r#"{"window":"5h","remaining_fraction":0.5,"reset_time":"soon"}"#,
        ),
        (
            "a fraction that is a string",
            r#"{"window":"5h","remaining_fraction":"0.5","reset_time":"2026-11-24T03:32:03Z"}"#,
        ),
    ] {
        let groups = quota::agy_groups(&with_5h_bucket(bucket)).expect("it reads");
        assert_eq!(
            groups,
            vec![QuotaGroup {
                name: "G".to_owned(),
                windows: vec![
                    window(WindowKind::FiveHour, WindowState::Unknown),
                    window(WindowKind::Weekly, used(50, "2026-11-30T01:54:01Z")),
                ],
            }],
            "{case}"
        );
    }
}

#[test]
fn an_answer_whose_every_bucket_is_garbled_is_unreadable_not_a_row_of_unknowns() {
    let garbled = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[
        {"window":"5h","remaining_fraction":"most","reset_time":"2026-11-24T03:32:03Z"},
        {"window":"weekly","remaining_fraction":0.5,"reset_time":"soon"}]}]}}}"#;
    assert_eq!(
        quota::agy_groups(garbled),
        Err("its answer has no bucket it can read".to_owned())
    );
}

#[test]
fn a_lower_fraction_at_the_epoch_is_a_reading_that_is_placed_as_unknown_not_a_garbled_answer() {
    let epoch = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[
        {"window":"5h","remaining_fraction":0.4,"reset_time":"1970-01-01T00:00:00Z"}]}]}}}"#;
    let groups = quota::agy_groups(epoch).expect("it reads");
    assert_eq!(
        groups[0].windows[0],
        window(WindowKind::FiveHour, WindowState::Unknown)
    );
}

#[test]
fn a_group_always_has_both_windows_and_a_missing_one_is_unknown() {
    let only_5h = r#"{"command":{"data":{"groups":[
        {"name":"A","buckets":[{"window":"5h","remaining_fraction":0.5,"reset_time":"2026-11-24T03:32:03Z"}]},
        {"name":"B","buckets":[]}]}}}"#;
    let groups = quota::agy_groups(only_5h).expect("it reads");
    assert_eq!(
        groups,
        vec![
            QuotaGroup {
                name: "A".to_owned(),
                windows: vec![
                    window(WindowKind::FiveHour, used(50, "2026-11-24T03:32:03Z")),
                    window(WindowKind::Weekly, WindowState::Unknown),
                ],
            },
            QuotaGroup {
                name: "B".to_owned(),
                windows: vec![
                    window(WindowKind::FiveHour, WindowState::Unknown),
                    window(WindowKind::Weekly, WindowState::Unknown),
                ],
            },
        ]
    );
}

#[test]
fn a_group_with_no_name_or_no_buckets_list_makes_the_answer_unreadable() {
    for usage in [
        r#"{"command":{"data":{"groups":[{"buckets":[]}]}}}"#,
        r#"{"command":{"data":{"groups":[{"name":"G"}]}}}"#,
        r#"{"command":{"data":{"groups":["G"]}}}"#,
    ] {
        assert_eq!(
            quota::agy_groups(usage),
            Err("a quota group of its answer has no name or buckets".to_owned()),
            "{usage}"
        );
    }
}

#[test]
fn an_epoch_reset_with_a_fraction_above_one_is_not_an_unused_window_to_routing_either() {
    let usage = agy_usage_with("3p-5h", 1.5, EPOCH);
    assert!(matches!(
        quota::agy_usage(&usage, CLAUDE_GPT),
        AgyUsage::Unreadable(_)
    ));
}

#[test]
fn an_answer_with_no_quota_group_to_read_says_why_it_is_unreadable() {
    let failed = r#"{"conversation_id":"","status":"ERROR","response":"","error":"Eligibility check failed"}"#;
    for (case, usage, why) in [
        (
            "not json",
            "Eligibility check failed",
            "its answer is not JSON",
        ),
        ("an error", failed, "its answer lists no quota groups"),
        ("empty", "", "its answer is not JSON"),
        (
            "groups with no window",
            r#"{"command":{"data":{"groups":[{"name":"G","buckets":[{"window":"monthly"}]}]}}}"#,
            "its answer lists no 5-hour or weekly window",
        ),
    ] {
        assert_eq!(quota::agy_groups(usage), Err(why.to_owned()), "{case}");
    }
}

#[test]
fn an_answer_whose_every_reset_is_out_of_range_is_unreadable_not_a_row_of_unknowns() {
    let before_the_epoch = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[
        {"window":"5h","remaining_fraction":0.4,"reset_time":"1960-01-01T00:00:00Z"},
        {"window":"weekly","remaining_fraction":0.4,"reset_time":"1960-01-02T00:00:00Z"}]}]}}}"#;
    assert_eq!(
        quota::agy_groups(before_the_epoch),
        Err("its answer has no bucket it can read".to_owned())
    );
}

#[test]
fn a_fraction_just_below_one_at_the_epoch_is_no_untouched_window() {
    let almost = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[
        {"window":"5h","remaining_fraction":0.9999999,"reset_time":"1970-01-01T00:00:00Z"}]}]}}}"#;
    let groups = quota::agy_groups(almost).expect("it reads");
    assert_eq!(
        groups[0].windows[0],
        window(WindowKind::FiveHour, WindowState::Unknown)
    );
}
