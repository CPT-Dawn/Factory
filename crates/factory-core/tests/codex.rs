//! Codex's usage windows, as `codex app-server` answers `account/rateLimits/read`: the reply
//! the parser reads (a real answer's shape, in `fixtures/codex-rate-limits.json`) and the quota
//! that keeps it, holds dispatch on it and goes stale after five minutes.

use factory_core::codex::{self, CodexQuota};
use factory_core::status::ClaudeState;
use factory_core::timestamp::Timestamp;
use factory_core::usage::WindowKind;
use serde_json::{Value, json};

const REPLY: &str = include_str!("fixtures/codex-rate-limits.json");

const FIVE_RESETS: i64 = 1_795_715_725;
const WEEK_RESETS: i64 = 1_795_831_256;

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_unix_millis(seconds * 1000)
}

/// 2026-11-26T13:50:00Z, a little before the five-hour window resets.
fn now() -> Timestamp {
    at(1_795_701_000)
}

fn reply(result: Value) -> String {
    json!({"id": 2, "result": result}).to_string()
}

fn limits(primary: Value, secondary: Value) -> String {
    reply(json!({"rateLimits": {"primary": primary, "secondary": secondary}}))
}

fn window(used: u32, minutes: i64, resets: i64) -> Value {
    json!({"usedPercent": used, "windowDurationMins": minutes, "resetsAt": resets})
}

#[test]
fn a_real_answer_gives_both_windows_the_plan_and_the_reset_credits() {
    let usage = codex::read_usage(REPLY, now()).unwrap_or_else(|e| panic!("{e:?}"));

    let kinds: Vec<_> = usage.windows.iter().map(|w| w.kind).collect();
    assert_eq!(kinds, [WindowKind::FiveHour, WindowKind::Weekly]);
    assert_eq!(usage.windows[0].used_percent, 18.0);
    assert_eq!(usage.windows[0].resets, at(FIVE_RESETS));
    assert_eq!(usage.windows[0].seen, now());
    assert_eq!(usage.windows[1].used_percent, 92.0);
    assert_eq!(usage.windows[1].resets, at(WEEK_RESETS));
    assert_eq!(usage.plan.as_deref(), Some("plus"));
    assert_eq!(usage.reset_credits, 2);
}

#[test]
fn a_window_is_named_by_its_length_not_its_place() {
    // A plan with only a weekly window sends it as primary.
    let usage = codex::read_usage(&limits(window(40, 10_080, WEEK_RESETS), Value::Null), now())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(usage.windows.len(), 1);
    assert_eq!(usage.windows[0].kind, WindowKind::Weekly);

    // A window of another length is not one of the two the factory paces on.
    let usage = codex::read_usage(
        &limits(window(95, 60, FIVE_RESETS), window(7, 300, FIVE_RESETS)),
        now(),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(usage.windows.len(), 1);
    assert_eq!(usage.windows[0].kind, WindowKind::FiveHour);
    assert_eq!(usage.windows[0].used_percent, 7.0);
}

#[test]
fn a_window_that_cannot_be_read_makes_the_answer_unreadable_not_zero_and_not_absent() {
    // A weekly window with no reset beside a good 5-hour one must not read as an absent
    // weekly window: the keeper forgets those, and a hold on the week would go with it.
    let torn_week = codex::read_usage(
        &limits(
            window(10, 300, FIVE_RESETS),
            json!({"usedPercent": 30, "windowDurationMins": 10_080}),
        ),
        now(),
    );
    let past_the_end =
        codex::read_usage(&limits(window(140, 300, FIVE_RESETS), json!(null)), now());
    assert_eq!(
        torn_week.err().unwrap_or_default(),
        "its weekly window could not be read"
    );
    assert_eq!(
        past_the_end.err().unwrap_or_default(),
        "its 5h window could not be read"
    );
}

#[test]
fn a_window_of_another_length_or_none_is_no_trouble() {
    let usage = codex::read_usage(
        &limits(
            window(10, 300, FIVE_RESETS),
            json!({"usedPercent": 30, "windowDurationMins": 60}),
        ),
        now(),
    );
    assert_eq!(usage.map(|usage| usage.windows.len()), Ok(1));
}

#[test]
fn a_window_whose_length_is_no_number_of_minutes_makes_the_answer_unreadable() {
    // A weekly window with its length as text must not read as absent beside a good 5-hour
    // one: the standing hold on the week would go with it.
    let usage = codex::read_usage(
        &limits(
            window(10, 300, FIVE_RESETS),
            json!({"usedPercent": 95, "windowDurationMins": "10080", "resetsAt": WEEK_RESETS}),
        ),
        now(),
    );
    assert_eq!(
        usage.err().unwrap_or_default(),
        "one of its windows has a length that could not be read"
    );
}

#[test]
fn a_window_that_names_no_length_or_is_no_window_makes_the_answer_unreadable() {
    // Beside a good 5-hour window, a weekly one whose length is missing, null, or that is no
    // object at all is torn, not absent: an absent one would erase a standing hold on the week.
    let torn = |week: Value| {
        codex::read_usage(&limits(window(10, 300, FIVE_RESETS), week), now())
            .err()
            .unwrap_or_default()
    };
    for week in [
        json!({"usedPercent": 95, "resetsAt": WEEK_RESETS}),
        json!({"usedPercent": 95, "windowDurationMins": null, "resetsAt": WEEK_RESETS}),
        json!("weekly"),
        json!(95),
    ] {
        assert_eq!(
            torn(week.clone()),
            "one of its windows has a length that could not be read",
            "{week}"
        );
    }
}

#[test]
fn reset_credits_count_only_the_available_ones() {
    let credits = |credits: Value, count: Value| {
        let mut answer: Value = serde_json::from_str(REPLY).unwrap_or_else(|e| panic!("{e:?}"));
        answer["result"]["rateLimitResetCredits"] =
            json!({"availableCount": count, "credits": credits});
        codex::read_usage(&answer.to_string(), now())
            .unwrap_or_else(|e| panic!("{e:?}"))
            .reset_credits
    };
    let row = |status: &str| json!({"id": "x", "resetType": "codexRateLimits", "status": status, "grantedAt": 1});

    assert_eq!(
        credits(
            json!([row("available"), row("redeemed"), row("available")]),
            json!(2)
        ),
        2
    );
    assert_eq!(
        credits(json!([row("redeemed"), row("redeeming")]), json!(0)),
        0
    );
    // The list may be capped: the count still stands.
    assert_eq!(credits(json!([row("available")]), json!(3)), 3);
    // Details not fetched: only the count is known.
    assert_eq!(credits(Value::Null, json!(1)), 1);

    let mut none: Value = serde_json::from_str(REPLY).unwrap_or_else(|e| panic!("{e:?}"));
    none["result"]["rateLimitResetCredits"] = Value::Null;
    assert_eq!(
        codex::read_usage(&none.to_string(), now())
            .unwrap_or_else(|e| panic!("{e:?}"))
            .reset_credits,
        0
    );
}

#[test]
fn an_answer_that_is_no_usage_says_why() {
    let cases = [
        ("", "its answer is not JSON"),
        ("not json", "its answer is not JSON"),
        (r#"{"id":2}"#, "its answer has no rate limits"),
        (r#"{"id":2,"result":{}}"#, "its answer has no rate limits"),
        (
            r#"{"id":2,"error":{"code":-32601,"message":"method not found"}}"#,
            "it answered with an error: method not found",
        ),
        (
            r#"{"id":2,"error":"boom"}"#,
            "it answered with an error: \"boom\"",
        ),
        (
            r#"{"id":2,"result":{"rateLimits":{"primary":null,"secondary":null}}}"#,
            "its answer names no 5-hour or weekly window it can read",
        ),
    ];
    for (answer, why) in cases {
        assert_eq!(
            codex::read_usage(answer, now()).err().unwrap_or_default(),
            why,
            "{answer}"
        );
    }
}

fn quota_of(used_five: u32, used_weekly: u32) -> CodexQuota {
    let mut quota = CodexQuota::default();
    let usage = codex::read_usage(
        &limits(
            window(used_five, 300, FIVE_RESETS),
            window(used_weekly, 10_080, WEEK_RESETS),
        ),
        now(),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    quota.see(usage, now());
    quota
}

#[test]
fn dispatch_holds_at_the_mark_on_either_window_until_it_resets() {
    let hold = |five, weekly| quota_of(five, weekly).record(now(), 90.0);

    let go = hold(89, 89);
    assert_eq!(go.verdict, ClaudeState::Go);
    assert_eq!((go.hold, go.until), (None, None));

    let weekly = hold(10, 90);
    assert_eq!(weekly.verdict, ClaudeState::Hold);
    assert_eq!(weekly.hold.as_deref(), Some("weekly 90%"));
    assert_eq!(weekly.until, Some(WEEK_RESETS * 1000));

    let five = hold(91, 10);
    assert_eq!(five.hold.as_deref(), Some("5h 91%"));
    assert_eq!(five.until, Some(FIVE_RESETS * 1000));

    // Both over: until the later reset.
    let both = hold(95, 97);
    assert_eq!(both.hold.as_deref(), Some("weekly 97%"));
    assert_eq!(both.until, Some(WEEK_RESETS * 1000));
}

#[test]
fn a_share_is_judged_by_the_whole_percent_it_reached() {
    let mut quota = CodexQuota::default();
    let usage = codex::read_usage(&limits(window(89, 300, FIVE_RESETS), Value::Null), now())
        .unwrap_or_else(|e| panic!("{e:?}"));
    quota.see(usage, now());
    assert_eq!(quota.record(now(), 89.5).verdict, ClaudeState::Go);
    assert_eq!(quota.record(now(), 89.0).verdict, ClaudeState::Hold);
}

#[test]
fn a_window_past_its_reset_is_unknown_and_holds_nothing() {
    let quota = quota_of(10, 97);

    let record = quota.record(at(WEEK_RESETS + 1), 90.0);

    assert_eq!(record.verdict, ClaudeState::Unknown);
    assert!(record.windows.iter().all(|reading| !reading.current));
    assert_eq!(record.hold, None);
}

#[test]
fn a_new_answer_is_the_whole_picture() {
    let mut quota = quota_of(95, 97);
    let usage = codex::read_usage(
        &limits(window(5, 300, FIVE_RESETS), Value::Null),
        at(1_795_702_000),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));

    quota.see(usage, at(1_795_702_000));

    let record = quota.record(at(1_795_702_000), 90.0);
    assert_eq!(record.windows.len(), 1);
    assert_eq!(record.verdict, ClaudeState::Go);
}

#[test]
fn an_unreadable_answer_says_why_and_keeps_the_last_reading() {
    let mut quota = quota_of(10, 97);

    quota.fail("it did not answer within 20s", at(1_795_701_100));

    let record = quota.record(at(1_795_701_100), 90.0);
    assert_eq!(
        record.unreadable.as_deref(),
        Some("it did not answer within 20s")
    );
    // What was seen stands until its window resets; it was never zero.
    assert_eq!(record.windows.len(), 2);
    assert_eq!(record.windows[1].used_percent, 97);
    assert_eq!(record.verdict, ClaudeState::Hold);

    // The next answer clears it.
    let usage = codex::read_usage(REPLY, at(1_795_701_200)).unwrap_or_else(|e| panic!("{e:?}"));
    quota.see(usage, at(1_795_701_200));
    assert_eq!(quota.record(at(1_795_701_200), 90.0).unreadable, None);
}

#[test]
fn a_reading_is_stale_after_five_minutes_whether_or_not_it_could_be_read() {
    let mut quota = CodexQuota::default();
    assert!(quota.stale(now()), "never asked");

    quota.fail("it is not there", now());
    assert!(!quota.stale(at(1_795_701_000 + 299)));
    assert!(quota.stale(at(1_795_701_000 + 300)));

    let usage = codex::read_usage(REPLY, at(1_795_701_400)).unwrap_or_else(|e| panic!("{e:?}"));
    quota.see(usage, at(1_795_701_400));
    assert!(!quota.stale(at(1_795_701_400 + 299)));
    assert!(quota.stale(at(1_795_701_400 + 300)));
    // A clock that went back is no reason to trust a reading.
    assert!(quota.stale(at(1_795_700_000)));
}
