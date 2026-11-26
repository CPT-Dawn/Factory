//! The Claude plan's quota: what the statusline and a 429 say of its 5-hour and weekly windows,
//! and whether a lead may dispatch.

use factory_core::claude::{
    ClaudeQuota, Holding, LimitHit, Verdict, failure_limit, limit_reset, statusline_windows,
    stop_failure,
};
use factory_core::quota::Reset;
use factory_core::timestamp::Timestamp;
use factory_core::usage::{UsageWindow, WindowKind};
use serde_json::json;

/// 2026-11-23 05:00:00Z.
const NOW: i64 = 1_795_410_000_000;
const HOUR: i64 = 3_600_000;
/// India: 5h30 east of UTC.
const IST: i32 = 19_800;

fn at(millis: i64) -> Timestamp {
    Timestamp::from_unix_millis(millis)
}

fn statusline(used: f64, resets_secs: i64) -> serde_json::Value {
    json!({"model": {"display_name": "Opus 5.5"},
           "rate_limits": {"five_hour": {"used_percentage": used, "resets_at": resets_secs},
                           "seven_day": {"used_percentage": 41.2, "resets_at": resets_secs + 86_400}}})
}

/// The windows a statusline seen at `seen` gives: the 5-hour one `used` percent spent and
/// resetting at `resets`, the weekly one 41.2% spent a day later.
fn seen(used: f64, resets: i64, seen: i64) -> Vec<UsageWindow> {
    statusline_windows(&statusline(used, resets / 1000), at(seen))
}

fn five_hour(used: f64, resets: i64) -> UsageWindow {
    window(WindowKind::FiveHour, used, resets)
}

fn window(kind: WindowKind, used: f64, resets: i64) -> UsageWindow {
    UsageWindow {
        kind,
        used_percent: used,
        resets: at(resets),
        seen: at(NOW),
    }
}

#[test]
fn the_statusline_gives_both_windows() {
    assert_eq!(
        seen(23.5, NOW + 2 * HOUR, NOW),
        vec![
            five_hour(23.5, NOW + 2 * HOUR),
            window(WindowKind::Weekly, 41.2, NOW + 2 * HOUR + 24 * HOUR),
        ]
    );
}

#[test]
fn a_statusline_gives_the_windows_it_has_and_none_it_lacks() {
    assert_eq!(
        statusline_windows(&json!({"model": {"display_name": "Opus 5.5"}}), at(NOW)),
        vec![]
    );
    assert_eq!(
        statusline_windows(
            &json!({"rate_limits": {"seven_day": {"used_percentage": 3, "resets_at": 1_795_500_000}}}),
            at(NOW)
        ),
        vec![window(WindowKind::Weekly, 3.0, 1_795_500_000_000)]
    );
    assert_eq!(
        statusline_windows(
            &json!({"rate_limits": {"five_hour": {"used_percentage": "lots"}, "seven_day": 7}}),
            at(NOW)
        ),
        vec![]
    );
    assert_eq!(statusline_windows(&json!("garbage"), at(NOW)), vec![]);
}

#[test]
fn nothing_seen_lets_a_lead_dispatch() {
    assert_eq!(
        ClaudeQuota::default().verdict(at(NOW), 90.0),
        Verdict::Unknown
    );
}

#[test]
fn a_window_under_the_mark_lets_a_lead_dispatch_and_one_over_it_holds_until_its_reset() {
    let mut quota = ClaudeQuota::default();
    quota.see(&seen(40.0, NOW + 2 * HOUR, NOW), at(NOW));
    assert_eq!(quota.verdict(at(NOW), 90.0), Verdict::Go);
    quota.see(&seen(92.0, NOW + 2 * HOUR, NOW + 1), at(NOW + 1));
    let Verdict::Hold { until, by } = quota.verdict(at(NOW + 2), 90.0) else {
        panic!("a window over the mark holds");
    };
    assert_eq!(until, at(NOW + 2 * HOUR));
    assert!(matches!(by, Holding::Window(w) if w.kind == WindowKind::FiveHour));
}

#[test]
fn a_weekly_window_over_the_mark_holds_until_the_week_resets() {
    let week = NOW + 50 * HOUR;
    let mut quota = ClaudeQuota::default();
    quota.see(
        &[
            five_hour(10.0, NOW + 2 * HOUR),
            window(WindowKind::Weekly, 91.0, week),
        ],
        at(NOW),
    );
    let Verdict::Hold { until, by } = quota.verdict(at(NOW), 90.0) else {
        panic!("the weekly window holds");
    };
    assert_eq!(until, at(week));
    assert!(matches!(by, Holding::Window(w) if w.kind == WindowKind::Weekly));
}

#[test]
fn with_both_windows_over_the_mark_the_one_that_resets_last_holds() {
    let week = NOW + 50 * HOUR;
    let mut quota = ClaudeQuota::default();
    quota.see(
        &[
            five_hour(95.0, NOW + 2 * HOUR),
            window(WindowKind::Weekly, 92.0, week),
        ],
        at(NOW),
    );
    assert!(matches!(
        quota.verdict(at(NOW), 90.0),
        Verdict::Hold { until, by: Holding::Window(w) } if until == at(week) && w.kind == WindowKind::Weekly
    ));
}

#[test]
fn a_reading_keeps_the_window_it_does_not_mention() {
    let week = NOW + 50 * HOUR;
    let mut quota = ClaudeQuota::default();
    quota.see(&[window(WindowKind::Weekly, 93.0, week)], at(NOW));
    quota.see(&[five_hour(5.0, NOW + 2 * HOUR)], at(NOW + 1));
    assert_eq!(quota.weekly.map(|w| w.used_percent), Some(93.0));
    assert!(matches!(
        quota.verdict(at(NOW + 2), 90.0),
        Verdict::Hold { .. }
    ));
}

#[test]
fn a_window_whose_reset_has_come_says_nothing_any_more() {
    let mut quota = ClaudeQuota::default();
    quota.see(&[five_hour(99.0, NOW + HOUR)], at(NOW));
    assert_eq!(quota.verdict(at(NOW + HOUR), 90.0), Verdict::Unknown);
}

#[test]
fn a_stale_window_leaves_the_verdict_to_the_one_that_stands() {
    let mut quota = ClaudeQuota::default();
    quota.see(
        &[
            five_hour(99.0, NOW + HOUR),
            window(WindowKind::Weekly, 20.0, NOW + 50 * HOUR),
        ],
        at(NOW),
    );
    assert_eq!(quota.verdict(at(NOW + 2 * HOUR), 90.0), Verdict::Go);
}

#[test]
fn a_limit_holds_until_the_reset_it_names() {
    let mut quota = ClaudeQuota::default();
    quota.hit_limit(Some(at(NOW + 3 * HOUR)), at(NOW));
    assert_eq!(
        quota.verdict(at(NOW + HOUR), 90.0),
        Verdict::Hold {
            until: at(NOW + 3 * HOUR),
            by: Holding::Limit
        }
    );
    assert_eq!(quota.verdict(at(NOW + 3 * HOUR), 90.0), Verdict::Unknown);
}

#[test]
fn a_limit_without_a_time_holds_until_the_windows_reset_else_an_hour() {
    let mut quota = ClaudeQuota::default();
    quota.hit_limit(None, at(NOW));
    assert_eq!(
        quota.verdict(at(NOW), 90.0),
        Verdict::Hold {
            until: at(NOW + HOUR),
            by: Holding::Limit
        }
    );
    let mut seen_full = ClaudeQuota::default();
    seen_full.see(&seen(97.0, NOW + 2 * HOUR, NOW), at(NOW));
    seen_full.hit_limit(None, at(NOW + 1));
    assert_eq!(
        seen_full.verdict(at(NOW + 2), 99.5),
        Verdict::Hold {
            until: at(NOW + 2 * HOUR),
            by: Holding::Limit
        }
    );
}

#[test]
fn a_later_limit_keeps_the_first_reset_unless_it_names_a_later_one() {
    let mut quota = ClaudeQuota::default();
    quota.hit_limit(Some(at(NOW + 3 * HOUR)), at(NOW));
    quota.hit_limit(None, at(NOW + 1));
    assert_eq!(quota.held_until(at(NOW + 2)), Some(at(NOW + 3 * HOUR)));
    quota.hit_limit(Some(at(NOW + 4 * HOUR)), at(NOW + 2));
    assert_eq!(quota.held_until(at(NOW + 3)), Some(at(NOW + 4 * HOUR)));
}

#[test]
fn a_window_with_room_seen_after_a_limit_lifts_it() {
    // A 429 from a capacity limit, not the plan's: the statusline still shows room.
    let mut quota = ClaudeQuota::default();
    quota.hit_limit(None, at(NOW));
    quota.see(&seen(35.0, NOW + 4 * HOUR, NOW + 1), at(NOW + 1));
    assert_eq!(quota.verdict(at(NOW + 2), 90.0), Verdict::Go);
    // A full window keeps it.
    let mut full = ClaudeQuota::default();
    full.hit_limit(Some(at(NOW + 2 * HOUR)), at(NOW));
    full.see(&seen(100.0, NOW + 2 * HOUR, NOW + 1), at(NOW + 1));
    assert_eq!(full.held_until(at(NOW + 2)), Some(at(NOW + 2 * HOUR)));
    // So does a full week, whatever the 5-hour window says.
    let mut week = ClaudeQuota::default();
    week.hit_limit(Some(at(NOW + 9 * HOUR)), at(NOW));
    week.see(
        &[
            five_hour(5.0, NOW + 4 * HOUR),
            window(WindowKind::Weekly, 100.0, NOW + 9 * HOUR),
        ],
        at(NOW + 1),
    );
    assert_eq!(week.held_until(at(NOW + 2)), Some(at(NOW + 9 * HOUR)));
}

#[test]
fn the_documented_limit_messages_give_their_reset_in_local_time() {
    // 05:00Z is 10:30 IST.
    let cases = [
        (
            "You've hit your session limit · resets 3:45pm",
            Some(NOW + 5 * HOUR + 15 * 60_000),
        ),
        (
            "Usage limit reached · limit resets 11am",
            Some(NOW + 30 * 60_000),
        ),
        // 9:30 has passed today: tomorrow's.
        (
            "5-hour limit reached ∙ resets 9:30am",
            Some(NOW + 23 * HOUR),
        ),
        (
            "You've hit your Opus limit · resets 12:15 PM (Asia/Kolkata)",
            Some(NOW + HOUR + 45 * 60_000),
        ),
        // Monday 2026-11-23 10:30 IST; next Monday 00:00 IST is 6 days 13.5 hours on.
        (
            "You've hit your weekly limit · resets Mon 12:00am",
            Some(NOW + 6 * 24 * HOUR + 13 * HOUR + 30 * 60_000),
        ),
        (
            "Claude AI usage limit reached|1795417200",
            Some(NOW + 2 * HOUR),
        ),
    ];
    for (text, until) in cases {
        assert_eq!(
            limit_reset(text, at(NOW), IST),
            until.map(|until| LimitHit::Resets(at(until))),
            "{text}"
        );
    }
}

#[test]
fn other_errors_are_no_plan_limit() {
    for text in [
        "API Error: 500 Internal Server Error",
        "Prompt is too long",
        "",
    ] {
        assert_eq!(limit_reset(text, at(NOW), IST), None, "{text}");
    }
    assert_eq!(
        limit_reset("You've hit your session limit", at(NOW), IST),
        Some(LimitHit::Unnamed)
    );
}

#[test]
fn a_stop_failure_for_a_rate_limit_is_a_limit_with_the_reset_its_message_names() {
    let input = json!({"hook_event_name": "StopFailure", "error": "rate_limit",
        "error_details": "429 Too Many Requests",
        "last_assistant_message": "You've hit your session limit · resets 3:45pm"});
    assert_eq!(
        stop_failure(&input, at(NOW), IST),
        Some(LimitHit::Resets(at(NOW + 5 * HOUR + 15 * 60_000)))
    );
    let bare = json!({"hook_event_name": "StopFailure", "error": "rate_limit",
        "last_assistant_message": "API Error: Rate limit reached"});
    assert_eq!(stop_failure(&bare, at(NOW), IST), Some(LimitHit::Unnamed));
    let other = json!({"hook_event_name": "StopFailure", "error": "server_error",
        "last_assistant_message": "API Error: 500"});
    assert_eq!(stop_failure(&other, at(NOW), IST), None);
}

#[test]
fn a_failed_claude_that_hit_a_limit_names_its_reset_when_it_can() {
    assert_eq!(
        failure_limit("You've hit your session limit \u{b7} resets 3:45pm"),
        Some(Reset::At {
            hour: 15,
            minute: 45
        })
    );
    assert_eq!(
        failure_limit("You've hit your weekly limit \u{b7} resets Mon 12:00am"),
        Some(Reset::Weekly {
            weekday: 1,
            hour: 0,
            minute: 0
        })
    );
    assert_eq!(
        failure_limit("Claude AI usage limit reached|1795417200"),
        Some(Reset::On(at(1_795_417_200_000)))
    );
    assert_eq!(failure_limit("You've hit your limit"), Some(Reset::Unknown));
}

#[test]
fn a_rate_limit_or_a_429_is_a_limit_with_no_reset() {
    for text in [
        "API Error: 429 Too Many Requests",
        "HTTP 429",
        "HTTP/1.1 429 Too Many",
        "HTTP/2 429",
        "HTTP/2.0 429",
        "HTTP/1 429 Too Many Requests",
        "HTTP error 429",
        "{\"type\":\"error\",\"status\":429}",
        "request failed with status code 429",
        "{\"error\":{\"type\":\"rate_limit_error\"}}",
        "This request would exceed your account's rate limit.",
    ] {
        assert_eq!(failure_limit(text), Some(Reset::Unknown), "{text}");
    }
}

#[test]
fn another_failure_is_no_limit() {
    for text in [
        "",
        "API Error: 500 internal server error",
        "request 4290 failed",
        "API Error: 500 internal server error; request 429 failed",
        "read 429 files before it failed",
        "API Error: 500; request code 429 failed",
        "Could not load schedule presets 3:45pm and 5:00pm",
        "Could not load schedule presets: size limit exceeded",
        "proxy substatus 429 on a 500",
        "Failed to load resetsHourly: size limit exceeded",
        "Failed to load resets_hourly: size limit exceeded",
        "Failed to load hourly_resets: size limit exceeded",
        "Invalid API key",
        "Invalid settings: rate_limit_config must be a number",
        "unknown option rate_limit_retries",
        "Failed to read ratelimit.json",
    ] {
        assert_eq!(failure_limit(text), None, "{text}");
    }
}

#[test]
fn a_weekly_reset_is_the_next_such_weekday_in_the_users_zone() {
    // NOW is a Monday 05:00Z, 10:30 IST.
    let weekly = Reset::Weekly {
        weekday: 1,
        hour: 0,
        minute: 0,
    };
    assert_eq!(weekly.until(at(NOW), 0), at(NOW + 7 * 24 * HOUR - 5 * HOUR));
    assert_eq!(
        Reset::Weekly {
            weekday: 2,
            hour: 0,
            minute: 0
        }
        .until(at(NOW), IST),
        at(NOW + 24 * HOUR - 10 * HOUR - HOUR / 2)
    );
}

#[test]
fn a_reset_named_with_a_429_or_alone_is_the_cooldowns_end() {
    assert_eq!(
        failure_limit("API Error: 429 \u{b7} resets 3:45pm"),
        Some(Reset::At {
            hour: 15,
            minute: 45
        })
    );
    assert_eq!(
        failure_limit("Try again, it resets 3:45pm"),
        Some(Reset::At {
            hour: 15,
            minute: 45
        })
    );
    assert_eq!(
        failure_limit("HTTP 429, the quota resets Mon 12:00am"),
        Some(Reset::Weekly {
            weekday: 1,
            hour: 0,
            minute: 0
        })
    );
}
