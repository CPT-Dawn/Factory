//! Usage-window history: which readings add a row, when rows are pruned, and `--since`.

use std::time::Duration;

use factory_core::history::{
    HistoryRow, cutoff, overdue, parse_since, prune_due, window_key, window_of,
};
use factory_core::timestamp::Timestamp;
use factory_core::usage::WindowKind;

fn at(text: &str) -> Timestamp {
    Timestamp::parse(text).unwrap_or_else(|| panic!("bad fixture {text}"))
}

fn row(used: f64, resets: i64, seen: i64) -> HistoryRow {
    HistoryRow {
        harness: "claude".to_owned(),
        group: None,
        window: WindowKind::FiveHour,
        used_percent: used,
        resets,
        seen,
    }
}

#[test]
fn the_first_reading_adds_a_row() {
    assert!(row(40.0, 10, 1).adds_to(None));
}

#[test]
fn the_same_reading_seen_again_adds_nothing() {
    let last = row(40.0, 10, 1);
    assert!(!row(40.0, 10, 99).adds_to(Some(&last)));
}

#[test]
fn a_changed_share_or_reset_adds_a_row() {
    let last = row(40.0, 10, 1);
    assert!(row(41.0, 10, 2).adds_to(Some(&last)));
    assert!(row(40.0, 11, 2).adds_to(Some(&last)));
    // Back to an earlier share is still a change from the last one.
    assert!(row(40.0, 10, 3).adds_to(Some(&row(41.0, 10, 2))));
}

#[test]
fn rows_are_kept_for_90_days() {
    let now = at("2026-11-26T12:00:00Z");
    assert_eq!(cutoff(now), at("2026-08-28T12:00:00Z"));
}

#[test]
fn the_history_is_pruned_at_least_daily() {
    let last = at("2026-11-26T00:00:00Z");
    assert!(prune_due(None, last));
    assert!(!prune_due(Some(last), at("2026-11-26T22:59:59Z")));
    // An hour short of a day: a look that comes a little late still prunes within the day.
    assert!(prune_due(Some(last), at("2026-11-26T23:00:00Z")));
}

#[test]
fn a_day_of_elapsed_time_makes_a_prune_due_whatever_the_wall_clock_says() {
    assert!(!overdue(Duration::from_secs(22 * 3600)));
    assert!(overdue(Duration::from_secs(23 * 3600)));
}

#[test]
fn since_reads_a_time_a_date_or_an_age() {
    let now = at("2026-11-26T12:00:00Z");
    assert_eq!(
        parse_since("2026-11-20T06:30:00Z", now),
        Ok(at("2026-11-20T06:30:00Z"))
    );
    assert_eq!(
        parse_since("2026-11-20", now),
        Ok(at("2026-11-20T00:00:00Z"))
    );
    assert_eq!(parse_since("7d", now), Ok(at("2026-11-19T12:00:00Z")));
    assert_eq!(parse_since("6h", now), Ok(at("2026-11-26T06:00:00Z")));
    assert_eq!(parse_since("90", now), Ok(at("2026-11-26T11:58:30Z")));
}

#[test]
fn since_refuses_anything_else() {
    let now = at("2026-11-26T12:00:00Z");
    for text in [
        "",
        "yesterday",
        "2026-13-01",
        "7w",
        "-3h",
        "99999999999999999999d",
    ] {
        let refused = parse_since(text, now).unwrap_err();
        assert!(refused.starts_with("bad time"), "{text}: {refused}");
    }
}

#[test]
fn a_window_keeps_its_name_in_the_database() {
    for kind in [WindowKind::FiveHour, WindowKind::Weekly] {
        assert_eq!(window_of(window_key(kind)), Some(kind));
    }
    assert_eq!(window_of("monthly"), None);
}

#[test]
fn a_row_shows_as_one_short_line() {
    let mut row = row(41.9, 10, at("2026-11-26T14:00:00Z").unix_millis());
    assert_eq!(
        row.describe(),
        "claude 5h 41% seen 2026-11-26T14:00:00.000Z"
    );
    row.group = Some("pro".to_owned());
    assert!(row.describe().starts_with("claude/pro 5h 41%"));
}
