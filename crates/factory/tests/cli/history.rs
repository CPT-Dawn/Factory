//! The usage-window history: the quota keeper appends each Claude reading that changed,
//! `factory quota history` prints it, and rows past 90 days are pruned. The test clock's skew
//! file moves the daemon's clock a day at a time, so the 90 days pass without a wait.

use std::path::PathBuf;

use serde_json::Value;

use crate::claude::{HOUR, hook, now_secs, statusline_input};
use crate::router::kill_daemon;
use crate::support::Sandbox;
use crate::support::world::poll_until;

const DAY: i64 = 86_400;

/// A sandbox whose clock a test moves by writing seconds into the returned file.
fn skewed() -> (Sandbox, PathBuf) {
    let sandbox = Sandbox::new().env("TZ", "UTC");
    let skew = sandbox.path("clock-skew");
    (sandbox.env("FACTORY_CLOCK_SKEW_FILE", &skew), skew)
}

fn days_on(skew: &PathBuf, days: i64) {
    std::fs::write(skew, (days * DAY).to_string()).unwrap();
}

/// The history, as `--json` prints it, and nothing else on stdout or stderr.
fn rows(sandbox: &Sandbox, extra: &[&str]) -> Vec<Value> {
    let mut args = vec!["quota", "history", "--json"];
    args.extend_from_slice(extra);
    let out = sandbox.factory(&args);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    let document: Value = serde_json::from_str(&out.stdout).unwrap();
    document["rows"].as_array().cloned().unwrap()
}

fn shares(rows: &[Value]) -> Vec<f64> {
    rows.iter()
        .map(|row| row["used_percent"].as_f64().unwrap())
        .collect()
}

fn five_hour(rows: &[Value]) -> Vec<(f64, i64)> {
    rows.iter()
        .filter(|row| row["window"] == "five_hour")
        .map(|row| {
            (
                row["used_percent"].as_f64().unwrap(),
                row["resets"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn two_identical_readings_in_a_row_add_one_row_per_window() {
    let (sandbox, _) = skewed();
    let resets = now_secs() + 2 * HOUR;

    hook(&sandbox, &statusline_input(40.0, resets, 12.0));
    hook(&sandbox, &statusline_input(40.0, resets, 12.0));

    let rows = rows(&sandbox, &[]);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["harness"], "claude");
    assert_eq!(rows[0]["group"], Value::Null);
    assert_eq!(rows[0]["window"], "five_hour");
    assert_eq!(rows[0]["used_percent"], 40.0);
    assert_eq!(rows[0]["resets"], resets * 1000);
    assert!(rows[0]["seen"].as_i64().unwrap() > 0, "{rows:?}");
    assert_eq!(rows[1]["window"], "weekly");
    assert_eq!(rows[1]["used_percent"], 12.0);
    assert_eq!(rows[1]["resets"], (resets + 86_400) * 1000);
}

#[test]
fn a_changed_share_or_reset_adds_a_row_oldest_first() {
    let (sandbox, _) = skewed();
    let resets = now_secs() + 2 * HOUR;

    hook(&sandbox, &statusline_input(40.0, resets, 12.0));
    hook(&sandbox, &statusline_input(41.0, resets, 12.0));
    hook(&sandbox, &statusline_input(41.0, resets + 5 * HOUR, 12.0));
    hook(&sandbox, &statusline_input(41.0, resets + 5 * HOUR, 12.0));

    let rows = rows(&sandbox, &[]);
    assert_eq!(
        five_hour(&rows),
        [
            (40.0, resets * 1000),
            (41.0, resets * 1000),
            (41.0, (resets + 5 * HOUR) * 1000)
        ]
    );
    let seen: Vec<i64> = rows
        .iter()
        .map(|row| row["seen"].as_i64().unwrap())
        .collect();
    assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]), "{rows:?}");
    // The third reading moved the weekly reset along with the 5-hour one: two weekly rows.
    assert_eq!(rows.len(), 5, "{rows:?}");
}

#[test]
fn a_reading_that_stands_after_a_restart_adds_nothing() {
    let (sandbox, _) = skewed();
    let resets = now_secs() + 2 * HOUR;
    hook(&sandbox, &statusline_input(40.0, resets, 12.0));

    kill_daemon(&sandbox);
    hook(&sandbox, &statusline_input(40.0, resets, 12.0));

    assert_eq!(rows(&sandbox, &[]).len(), 2);
}

/// Readings of 40/12 percent on day 0 and 41/13 on day 30.
fn two_readings(sandbox: &Sandbox, skew: &PathBuf) {
    let resets = now_secs() + 2 * HOUR;
    hook(sandbox, &statusline_input(40.0, resets, 12.0));
    days_on(skew, 30);
    hook(sandbox, &statusline_input(41.0, resets, 13.0));
}

#[test]
fn rows_past_90_days_are_pruned_when_the_daemon_starts() {
    let (sandbox, skew) = skewed();
    two_readings(&sandbox, &skew);
    kill_daemon(&sandbox);

    // Day 91: the first reading is 91 days old, the second 61.
    days_on(&skew, 91);

    assert_eq!(shares(&rows(&sandbox, &[])), [41.0, 13.0]);
}

#[test]
fn rows_past_90_days_are_pruned_in_a_daemon_that_sits_idle() {
    let (sandbox, skew) = skewed();
    two_readings(&sandbox, &skew);
    assert_eq!(rows(&sandbox, &[]).len(), 4);

    days_on(&skew, 91);

    // No reading, no restart: the daemon prunes on its own.
    let mut left = Vec::new();
    assert!(
        poll_until(|| {
            left = shares(&rows(&sandbox, &[]));
            left.len() == 2
        }),
        "{left:?}"
    );
    assert_eq!(left, [41.0, 13.0]);
}

#[test]
fn since_keeps_the_rows_from_that_moment_on() {
    let (sandbox, skew) = skewed();
    let resets = now_secs() + 2 * HOUR;
    hook(&sandbox, &statusline_input(10.0, resets, 11.0));
    days_on(&skew, 5);
    hook(&sandbox, &statusline_input(20.0, resets, 21.0));
    days_on(&skew, 10);
    hook(&sandbox, &statusline_input(30.0, resets, 31.0));

    let since = |text: &str| shares(&rows(&sandbox, &["--since", text]));

    // An age counts back from the clock's day 10.
    assert_eq!(since("30d"), [10.0, 11.0, 20.0, 21.0, 30.0, 31.0]);
    assert_eq!(since("6d"), [20.0, 21.0, 30.0, 31.0]);
    assert_eq!(since("1h"), [30.0, 31.0]);
    assert_eq!(since("2999-01-01"), Vec::<f64>::new());
    assert_eq!(
        shares(&rows(&sandbox, &["--since", "2020-01-01T00:00:00Z"])).len(),
        6
    );
}

#[test]
fn a_bad_since_is_a_usage_error() {
    let (sandbox, _) = skewed();

    let out = sandbox.factory(&["quota", "history", "--since", "yesterday"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: bad time \"yesterday\""),
        "{out:?}"
    );
}

#[test]
fn a_few_rows_print_on_one_line_and_none_say_so() {
    let (sandbox, _) = skewed();
    let none = sandbox.factory(&["quota", "history"]);
    assert_eq!(
        (none.code, none.stdout.as_str()),
        (0, "quota history: no readings\n"),
        "{none:?}"
    );

    hook(
        &sandbox,
        &statusline_input(40.0, now_secs() + 2 * HOUR, 12.0),
    );

    let out = sandbox.factory(&["quota", "history"]);
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    assert!(
        out.stdout
            .starts_with("quota history: 2 rows: claude 5h 40% seen 20"),
        "{out:?}"
    );
    assert!(
        out.stdout.contains("; claude weekly 12% seen 20"),
        "{out:?}"
    );
}

/// Readings that make `rows` rows, two to a reading, none of them alike.
fn many_readings(sandbox: &Sandbox, readings: usize) {
    let resets = now_secs() + 2 * HOUR;
    for n in 0..readings {
        let n = n as f64;
        hook(sandbox, &statusline_input(10.0 + n, resets, 50.0 + n));
    }
}

fn history_file(out: &str) -> PathBuf {
    let line = out.lines().next().unwrap();
    PathBuf::from(line.split(" rows in ").nth(1).unwrap())
}

#[test]
fn many_rows_print_one_line_naming_the_file_that_holds_them() {
    let (sandbox, _) = skewed();
    many_readings(&sandbox, 6);

    let out = sandbox.factory(&["quota", "history"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    assert!(
        out.stdout.starts_with("quota history: 12 rows in "),
        "{out:?}"
    );
    let path = history_file(&out.stdout);
    assert!(path.starts_with(sandbox.path("state/factory")), "{out:?}");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file["rows"].as_array().unwrap().len(), 12);
    assert_eq!(file["rows"][0]["used_percent"], 10.0);
}

#[test]
fn each_result_gets_a_file_of_its_own() {
    let (sandbox, skew) = skewed();
    let resets = now_secs() + 2 * HOUR;
    for n in 0..6 {
        days_on(&skew, i64::from(n));
        let n = f64::from(n);
        hook(&sandbox, &statusline_input(10.0 + n, resets, 50.0 + n));
    }

    let all = sandbox.factory(&["quota", "history"]);
    let recent = sandbox.factory(&["quota", "history", "--since", "4d"]);
    let again = sandbox.factory(&["quota", "history"]);

    let (all, recent) = (history_file(&all.stdout), history_file(&recent.stdout));
    assert_ne!(all, recent);
    let count = |path: &PathBuf| {
        let file: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        file["rows"].as_array().unwrap().len()
    };
    assert_eq!((count(&all), count(&recent)), (12, 8));
    assert_eq!(history_file(&again.stdout), all);
}

#[test]
fn json_prints_the_rows_and_writes_no_file() {
    let (sandbox, _) = skewed();
    many_readings(&sandbox, 6);

    assert_eq!(rows(&sandbox, &[]).len(), 12);

    let exported = std::fs::read_dir(sandbox.path("state/factory"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("quota-history")
        });
    assert!(!exported, "--json wrote a file");
}
