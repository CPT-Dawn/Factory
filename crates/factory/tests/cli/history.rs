//! The usage-window history: the quota keeper appends each Claude reading that changed,
//! `factory quota history` prints it, and rows past 90 days are pruned.

use serde_json::Value;

use crate::claude::{HOUR, hook, now_secs, statusline_input};
use crate::router::kill_daemon;
use crate::support::Sandbox;

const DAY_MS: i64 = 86_400_000;

fn utc() -> Sandbox {
    Sandbox::new().env("TZ", "UTC")
}

fn rows(sandbox: &Sandbox, extra: &[&str]) -> Vec<Value> {
    let mut args = vec!["quota", "history", "--json"];
    args.extend_from_slice(extra);
    let out = sandbox.factory(&args);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    let document: Value = serde_json::from_str(&out.stdout).unwrap();
    document["rows"].as_array().cloned().unwrap()
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

fn seed(sandbox: &Sandbox, seen_ms: i64, used: f64) {
    let db = rusqlite::Connection::open(sandbox.path("state/factory/factory.db")).unwrap();
    db.execute(
        "INSERT INTO usage_history (harness, quota_group, window, used_percent, resets_ms, seen_ms)
         VALUES ('claude', NULL, 'five_hour', ?1, ?2, ?3)",
        rusqlite::params![used, seen_ms + 3_600_000, seen_ms],
    )
    .unwrap();
}

fn now_ms() -> i64 {
    now_secs() * 1000
}

#[test]
fn two_identical_readings_in_a_row_add_one_row_per_window() {
    let sandbox = utc();
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
    let sandbox = utc();
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
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    hook(&sandbox, &statusline_input(40.0, resets, 12.0));

    kill_daemon(&sandbox);
    hook(&sandbox, &statusline_input(40.0, resets, 12.0));

    assert_eq!(rows(&sandbox, &[]).len(), 2);
}

#[test]
fn rows_older_than_90_days_are_pruned_when_the_daemon_starts() {
    let sandbox = utc();
    hook(
        &sandbox,
        &statusline_input(40.0, now_secs() + 2 * HOUR, 12.0),
    );
    kill_daemon(&sandbox);
    seed(&sandbox, now_ms() - 91 * DAY_MS, 10.0);
    seed(&sandbox, now_ms() - 89 * DAY_MS, 20.0);

    let rows = rows(&sandbox, &[]);

    let shares: Vec<f64> = rows
        .iter()
        .map(|row| row["used_percent"].as_f64().unwrap())
        .collect();
    assert_eq!(shares, [20.0, 40.0, 12.0], "{rows:?}");
}

#[test]
fn since_keeps_the_rows_from_that_moment_on() {
    let sandbox = utc();
    hook(
        &sandbox,
        &statusline_input(40.0, now_secs() + 2 * HOUR, 12.0),
    );
    kill_daemon(&sandbox);
    seed(&sandbox, now_ms() - 10 * DAY_MS, 10.0);
    seed(&sandbox, now_ms() - 2 * DAY_MS, 20.0);

    let shares = |since: &str| -> Vec<f64> {
        rows(&sandbox, &["--since", since])
            .iter()
            .map(|row| row["used_percent"].as_f64().unwrap())
            .collect()
    };

    assert_eq!(shares("30d"), [10.0, 20.0, 40.0, 12.0]);
    assert_eq!(shares("5d"), [20.0, 40.0, 12.0]);
    assert_eq!(shares("1h"), [40.0, 12.0]);
    assert_eq!(shares("2999-01-01"), Vec::<f64>::new());
}

#[test]
fn since_takes_a_utc_time() {
    let sandbox = utc();
    hook(
        &sandbox,
        &statusline_input(40.0, now_secs() + 2 * HOUR, 12.0),
    );
    assert_eq!(
        rows(&sandbox, &["--since", "2020-01-01T00:00:00Z"]).len(),
        2
    );
    assert_eq!(
        rows(&sandbox, &["--since", "2999-01-01T00:00:00Z"]).len(),
        0
    );
}

#[test]
fn a_bad_since_is_a_usage_error() {
    let sandbox = utc();

    let out = sandbox.factory(&["quota", "history", "--since", "yesterday"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: bad time \"yesterday\""),
        "{out:?}"
    );
}

#[test]
fn a_few_rows_print_on_one_line_and_none_say_so() {
    let sandbox = utc();
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

#[test]
fn many_rows_print_one_line_naming_the_file_that_holds_them() {
    let sandbox = utc();
    hook(
        &sandbox,
        &statusline_input(40.0, now_secs() + 2 * HOUR, 12.0),
    );
    kill_daemon(&sandbox);
    for day in 1..=20 {
        seed(&sandbox, now_ms() - day * DAY_MS, day as f64);
    }

    let out = sandbox.factory(&["quota", "history"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = out.stdout_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(
        lines[0].starts_with("quota history: 22 rows in "),
        "{out:?}"
    );
    let path = lines[0].strip_prefix("quota history: 22 rows in ").unwrap();
    assert!(
        path.starts_with(sandbox.path("state/factory").to_str().unwrap()),
        "{out:?}"
    );
    let file: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(file["rows"].as_array().unwrap().len(), 22);
    assert_eq!(file["rows"][0]["used_percent"], 20.0);
}
