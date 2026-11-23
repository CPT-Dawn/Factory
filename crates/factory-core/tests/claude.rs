//! The Claude plan's quota: what the statusline and a 429 say of the 5-hour window, and whether a
//! lead may dispatch.

use factory_core::claude::{ClaudeQuota, Verdict, limit_reset, statusline_window, stop_failure};
use factory_core::timestamp::Timestamp;
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

#[test]
fn the_statusline_gives_the_five_hour_window() {
    let window = statusline_window(&statusline(23.5, (NOW + 2 * HOUR) / 1000)).unwrap();
    assert!((window.used_percent - 23.5).abs() < f64::EPSILON);
    assert_eq!(window.resets, at(NOW + 2 * HOUR));
}

#[test]
fn a_statusline_without_rate_limits_gives_no_window() {
    assert_eq!(
        statusline_window(&json!({"model": {"display_name": "Opus 5.5"}})),
        None
    );
    assert_eq!(
        statusline_window(
            &json!({"rate_limits": {"seven_day": {"used_percentage": 3, "resets_at": 1}}})
        ),
        None
    );
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
    quota.see_window(
        statusline_window(&statusline(40.0, (NOW + 2 * HOUR) / 1000)).unwrap(),
        at(NOW),
    );
    assert_eq!(
        quota.verdict(at(NOW), 90.0),
        Verdict::Go {
            used_percent: 40.0,
            resets: at(NOW + 2 * HOUR)
        }
    );
    quota.see_window(
        statusline_window(&statusline(92.0, (NOW + 2 * HOUR) / 1000)).unwrap(),
        at(NOW + 1),
    );
    assert_eq!(
        quota.verdict(at(NOW + 2), 90.0),
        Verdict::Hold {
            until: at(NOW + 2 * HOUR),
            used_percent: Some(92.0)
        }
    );
}

#[test]
fn a_window_whose_reset_has_come_says_nothing_any_more() {
    let mut quota = ClaudeQuota::default();
    quota.see_window(
        statusline_window(&statusline(99.0, (NOW + HOUR) / 1000)).unwrap(),
        at(NOW),
    );
    assert_eq!(quota.verdict(at(NOW + HOUR), 90.0), Verdict::Unknown);
}

#[test]
fn a_limit_holds_until_the_reset_it_names() {
    let mut quota = ClaudeQuota::default();
    quota.hit_limit(Some(at(NOW + 3 * HOUR)), at(NOW));
    assert_eq!(
        quota.verdict(at(NOW + HOUR), 90.0),
        Verdict::Hold {
            until: at(NOW + 3 * HOUR),
            used_percent: None
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
            used_percent: None
        }
    );
    let mut seen = ClaudeQuota::default();
    seen.see_window(
        statusline_window(&statusline(97.0, (NOW + 2 * HOUR) / 1000)).unwrap(),
        at(NOW),
    );
    seen.hit_limit(None, at(NOW + 1));
    assert_eq!(
        seen.verdict(at(NOW + 2), 99.5),
        Verdict::Hold {
            until: at(NOW + 2 * HOUR),
            used_percent: Some(97.0)
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
    quota.see_window(
        statusline_window(&statusline(35.0, (NOW + 4 * HOUR) / 1000)).unwrap(),
        at(NOW + 1),
    );
    assert_eq!(
        quota.verdict(at(NOW + 2), 90.0),
        Verdict::Go {
            used_percent: 35.0,
            resets: at(NOW + 4 * HOUR)
        }
    );
    // A full window keeps it.
    let mut full = ClaudeQuota::default();
    full.hit_limit(Some(at(NOW + 2 * HOUR)), at(NOW));
    full.see_window(
        statusline_window(&statusline(100.0, (NOW + 2 * HOUR) / 1000)).unwrap(),
        at(NOW + 1),
    );
    assert_eq!(full.held_until(at(NOW + 2)), Some(at(NOW + 2 * HOUR)));
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
            Some(until.map(at)),
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
        Some(None)
    );
}

#[test]
fn a_stop_failure_for_a_rate_limit_is_a_limit_with_the_reset_its_message_names() {
    let input = json!({"hook_event_name": "StopFailure", "error": "rate_limit",
        "error_details": "429 Too Many Requests",
        "last_assistant_message": "You've hit your session limit · resets 3:45pm"});
    assert_eq!(
        stop_failure(&input, at(NOW), IST),
        Some(Some(at(NOW + 5 * HOUR + 15 * 60_000)))
    );
    let bare = json!({"hook_event_name": "StopFailure", "error": "rate_limit",
        "last_assistant_message": "API Error: Rate limit reached"});
    assert_eq!(stop_failure(&bare, at(NOW), IST), Some(None));
    let other = json!({"hook_event_name": "StopFailure", "error": "server_error",
        "last_assistant_message": "API Error: 500"});
    assert_eq!(stop_failure(&other, at(NOW), IST), None);
}
