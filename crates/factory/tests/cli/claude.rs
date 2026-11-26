//! The Claude plan's quota: `factory hook statusline` and the StopFailure hook tell the daemon
//! what they saw; `factory quota claude` says whether a lead may dispatch, `factory wait claude` blocks
//! until it may, and `factory status` shows it.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::router::configure;
use crate::support::world::poll_until;
use crate::support::{Run, Sandbox, finish};

pub(crate) const HOUR: i64 = 3_600;

pub(crate) fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// The status line's input of a session whose 5-hour window is `used` percent spent and resets
/// at `resets` (seconds since the epoch), and whose weekly window is `weekly` percent spent and
/// resets a day later.
pub(crate) fn statusline_input(used: f64, resets: i64, weekly: f64) -> String {
    json!({
        "session_id": "s1",
        "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
        "workspace": {"current_dir": "/tmp/w"},
        "rate_limits": {
            "five_hour": {"used_percentage": used, "resets_at": resets},
            "seven_day": {"used_percentage": weekly, "resets_at": resets + 86_400}
        }
    })
    .to_string()
}

/// Hands the status line's input to `factory hook statusline` with the daemon up, as systemd's
/// socket keeps it: the hook never starts one itself.
pub(crate) fn hook(sandbox: &Sandbox, input: &str) -> Run {
    assert_eq!(sandbox.factory(&["monitor", "status"]).code, 0);
    let out = sandbox.factory_stdin(&["hook", "statusline"], input);
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", ""),
        "{out:?}"
    );
    out
}

pub(crate) fn statusline(sandbox: &Sandbox, used: f64, resets: i64) -> Run {
    hook(sandbox, &statusline_input(used, resets, 12.0))
}

fn quota(sandbox: &Sandbox) -> Run {
    sandbox.factory(&["quota", "claude"])
}

fn quota_json(sandbox: &Sandbox) -> Value {
    serde_json::from_str(&sandbox.factory(&["quota", "claude", "--json"]).stdout).unwrap()
}

/// `HH:MM` of a moment, in UTC (the sandbox runs with TZ=UTC).
fn clock(secs: i64) -> String {
    let minutes = secs.div_euclid(60).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// A moment as the factory prints one: `2026-11-23T04:13:00.000Z`.
fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Civil from days, after Howard Hinnant.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{}:{:02}.000Z",
        clock(secs),
        secs.rem_euclid(60)
    )
}

fn utc() -> Sandbox {
    Sandbox::new().env("TZ", "UTC")
}

fn claude_row(sandbox: &Sandbox) -> String {
    let out = sandbox.factory(&["status"]);
    out.stdout_lines()
        .into_iter()
        .find(|l| l.starts_with("claude:"))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn the_hook_records_both_windows_and_prints_nothing() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;

    statusline(&sandbox, 40.0, resets);

    let out = quota(&sandbox);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(
        out.stdout,
        format!(
            "claude: 5h 40% resets {}, weekly 12% resets {}; dispatch\n",
            stamp(resets),
            stamp(resets + 86_400)
        )
    );
    let shown = quota_json(&sandbox);
    assert_eq!(shown["verdict"], "go", "{shown}");
    assert_eq!(shown["used_percent"], 40, "{shown}");
    assert_eq!(shown["resets"], resets * 1000, "{shown}");
    assert_eq!(shown["windows"][1]["window"], "weekly", "{shown}");
    assert_eq!(shown["windows"][1]["used_percent"], 12, "{shown}");
    assert_eq!(
        shown["windows"][1]["resets"],
        (resets + 86_400) * 1000,
        "{shown}"
    );
}

#[test]
fn the_hook_exits_0_in_silence_on_partial_empty_or_garbage_input() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    statusline(&sandbox, 40.0, resets);

    // Only the weekly window, and a malformed 5-hour one: the 5-hour reading stands.
    let partial = json!({"rate_limits": {
        "five_hour": {"used_percentage": "lots"},
        "seven_day": {"used_percentage": 30.0, "resets_at": resets + 86_400}
    }});
    hook(&sandbox, &partial.to_string());
    // A share or a reset out of range is garbled too.
    for (used, at) in [
        (json!(-5), json!(resets)),
        (json!(250), json!(resets)),
        (json!(10), json!(-1)),
        (json!(10), json!(1e300)),
    ] {
        let input =
            json!({"rate_limits": {"five_hour": {"used_percentage": used, "resets_at": at}}});
        hook(&sandbox, &input.to_string());
    }
    for input in [
        "",
        "not json at all",
        "[1, 2, 3]",
        "{\"rate_limits\": null}",
        "\u{0}\u{1f}\u{7f}",
    ] {
        hook(&sandbox, input);
    }
    let raw = sandbox.factory_stdin(&["hook", "statusline", "--json"], "{\"model\": {}}");
    assert_eq!(
        (raw.code, raw.stdout.as_str(), raw.stderr.as_str()),
        (0, "", ""),
        "{raw:?}"
    );

    let shown = quota_json(&sandbox);
    assert_eq!(shown["windows"][0]["used_percent"], 40, "{shown}");
    assert_eq!(shown["windows"][1]["used_percent"], 30, "{shown}");
}

#[test]
fn the_hook_exits_0_at_once_with_the_daemon_down_and_starts_none() {
    let sandbox = utc();
    let input = statusline_input(40.0, now_secs() + HOUR, 12.0);

    let begun = Instant::now();
    let out = sandbox.factory_stdin(&["hook", "statusline"], &input);
    let took = begun.elapsed();

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", ""),
        "{out:?}"
    );
    assert!(took < Duration::from_millis(500), "took {took:?}");
    assert!(
        !sandbox.path("run/factory/daemon.pid").exists(),
        "the hook started a daemon"
    );
}

#[test]
fn the_statusline_command_is_gone_from_the_cli_and_its_help() {
    let sandbox = utc();
    let help = sandbox.factory(&["--help"]);
    assert_eq!(help.code, 0, "{help:?}");
    assert!(!help.stdout.contains("statusline"), "{help:?}");
    assert!(!help.stdout.contains("Statusline"), "{help:?}");

    let gone = sandbox.factory_stdin(&["statusline"], "{}");
    assert_eq!(gone.code, 2, "{gone:?}");

    let hooks = sandbox.factory(&["hook", "--help"]);
    assert!(hooks.stdout.contains("statusline"), "{hooks:?}");
}

#[test]
fn a_spent_weekly_window_holds_until_the_week_resets_and_is_named() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    let weekly_resets = resets + 86_400;
    hook(&sandbox, &statusline_input(41.0, resets, 91.0));

    let out = quota(&sandbox);

    assert_eq!((out.code, out.stdout.as_str()), (75, ""), "{out:?}");
    assert_eq!(
        out.stderr,
        format!(
            "factory: claude: hold: weekly 91%; hold new dispatches until {}; 5h 41%, weekly 91%; factory wait claude blocks until then\n",
            stamp(weekly_resets)
        )
    );
    let row = claude_row(&sandbox);
    assert!(row.starts_with("claude: hold: weekly 91% until "), "{row}");
    assert!(row.ends_with("; 5h 41%, weekly 91%"), "{row}");
    let record: Value = sandbox
        .factory(&["status", "--json"])
        .stdout
        .parse()
        .unwrap();
    assert_eq!(record["claude"]["hold"], "weekly 91%", "{record}");
    assert_eq!(record["claude"]["until"], weekly_resets * 1000, "{record}");
}

#[test]
fn a_reading_past_its_reset_shows_as_unknown_with_its_age() {
    let sandbox = utc();
    // The 5-hour window reset an hour ago; the week still stands.
    let reset = now_secs() - HOUR;
    hook(&sandbox, &statusline_input(97.0, reset, 12.0));

    let out = quota(&sandbox);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stdout
            .starts_with("claude: 5h unknown (seen 0m ago), weekly 12% resets "),
        "{out:?}"
    );
    let row = claude_row(&sandbox);
    assert!(
        row.starts_with("claude: 5h unknown (seen 0m ago), weekly 12% resets "),
        "{row}"
    );
    let shown = quota_json(&sandbox);
    assert_eq!(shown["windows"][0]["current"], false, "{shown}");
    assert_eq!(shown["used_percent"], Value::Null, "{shown}");

    // While the week holds, the line still names the stale window and its age.
    hook(&sandbox, &statusline_input(97.0, reset, 95.0));
    let held = quota(&sandbox);
    assert_eq!(held.code, 75, "{held:?}");
    assert!(
        held.stderr
            .contains("; 5h unknown (seen 0m ago), weekly 95%; "),
        "{held:?}"
    );
    let row = claude_row(&sandbox);
    // The screen may cut the row at its width; the stale window comes first.
    assert!(
        row.contains("; 5h unknown (seen 0m ago), weekly 9"),
        "{row}"
    );
}

#[test]
fn nothing_seen_lets_a_lead_dispatch() {
    let sandbox = utc();

    let out = quota(&sandbox);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout, "claude: no plan window seen yet; dispatch\n");
}

#[test]
fn a_window_with_room_lets_a_lead_dispatch() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    statusline(&sandbox, 40.0, resets);

    let out = quota(&sandbox);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert!(out.stdout.starts_with("claude: 5h 40% resets "), "{out:?}");
    assert!(out.stdout.ends_with("; dispatch\n"), "{out:?}");
    let shown = quota_json(&sandbox);
    assert_eq!(shown["verdict"], "go", "{shown}");
    assert_eq!(shown["hold_at"], 90.0, "{shown}");
}

#[test]
fn a_nearly_spent_window_exits_75_naming_the_reset() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    statusline(&sandbox, 93.0, resets);

    let out = quota(&sandbox);

    assert_eq!((out.code, out.stdout.as_str()), (75, ""), "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: claude: hold: 5h 93%; hold new dispatches until "),
        "{out:?}"
    );
}

#[test]
fn the_config_moves_the_mark() {
    let sandbox = utc();
    configure(&sandbox, "[claude]\nhold_at_percent = 50\n");
    statusline(&sandbox, 60.0, now_secs() + HOUR);

    assert_eq!(quota(&sandbox).code, 75);
}

#[test]
fn a_mark_out_of_range_is_refused_naming_the_file() {
    let sandbox = utc();
    configure(&sandbox, "[claude]\nhold_at_percent = 0\n");

    let out = quota(&sandbox);

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stderr.contains("config.toml") && out.stderr.contains("hold_at_percent"),
        "{out:?}"
    );
}

#[test]
fn a_turn_that_ends_on_the_plan_limit_holds_until_the_reset_it_names() {
    let sandbox = utc();
    // Four hours on, as a clock time the message names; whole minutes.
    let reset = (now_secs() + 4 * HOUR) / 60 * 60;
    let minutes = reset.div_euclid(60).rem_euclid(24 * 60);
    let (hour, minute) = (minutes / 60, minutes % 60);
    let half = if hour < 12 { "am" } else { "pm" };
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    let input = json!({
        "session_id": "s1", "hook_event_name": "StopFailure", "error": "rate_limit",
        "error_details": "429 Too Many Requests",
        "last_assistant_message": format!("You've hit your session limit · resets {twelve}:{minute:02}{half}"),
    });

    let hooked = sandbox.factory_stdin(&["hook", "stop-failure"], &input.to_string());

    assert_eq!(
        (hooked.code, hooked.stdout.as_str(), hooked.stderr.as_str()),
        (0, "", ""),
        "{hooked:?}"
    );
    let out = sandbox.factory(&["quota", "claude", "--json"]);
    assert_eq!(out.code, 75, "{out:?}");
    let refused: Value = serde_json::from_str(&out.stdout).unwrap();
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("hit its limit; hold new dispatches until"),
        "{refused}"
    );
    assert_eq!(
        sandbox
            .factory(&["status", "--json"])
            .stdout
            .parse::<Value>()
            .unwrap()["claude"]["until"],
        reset * 1000
    );
}

#[test]
fn a_turn_that_ends_on_another_error_changes_nothing() {
    let sandbox = utc();
    let input = json!({"hook_event_name": "StopFailure", "error": "server_error",
        "last_assistant_message": "API Error: 500"});

    let hooked = sandbox.factory_stdin(&["hook", "stop-failure"], &input.to_string());

    assert_eq!((hooked.code, hooked.stdout.as_str()), (0, ""), "{hooked:?}");
    assert_eq!(quota(&sandbox).code, 0);
}

#[test]
fn a_limit_outlives_a_daemon_restart() {
    let sandbox = utc();
    let input = json!({"hook_event_name": "StopFailure", "error": "rate_limit",
        "last_assistant_message": "API Error: Rate limit reached"});
    assert_eq!(
        sandbox
            .factory_stdin(&["hook", "stop-failure"], &input.to_string())
            .code,
        0
    );

    crate::router::kill_daemon(&sandbox);

    assert_eq!(quota(&sandbox).code, 75);
}

#[test]
fn the_status_screen_shows_the_window() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    let idle = sandbox.factory(&["status"]);
    assert!(
        idle.stdout_lines().contains(&"claude: no plan window seen"),
        "{idle:?}"
    );

    statusline(&sandbox, 40.0, resets);
    let line = claude_row(&sandbox);
    assert!(line.starts_with("claude: 5h 40% resets "), "{line}");
    assert!(
        line.contains(&format!("{}Z, weekly 12% resets ", clock(resets))),
        "{line}"
    );

    statusline(&sandbox, 95.0, resets);
    let line = claude_row(&sandbox);
    assert!(line.starts_with("claude: hold: 5h 95% until "), "{line}");
    assert!(
        line.ends_with(&format!("{}Z; 5h 95%, weekly 12%", clock(resets))),
        "{line}"
    );
}

#[test]
fn a_wait_returns_at_once_with_room_and_times_out_while_holding() {
    let sandbox = utc();
    let free = sandbox.factory(&["wait", "claude", "--timeout", "5s"]);
    assert_eq!(
        (free.code, free.stdout.as_str()),
        (0, "claude: the windows have room; dispatch\n"),
        "{free:?}"
    );

    statusline(&sandbox, 97.0, now_secs() + 2 * HOUR);
    let held = sandbox.factory(&["wait", "claude", "--timeout", "1s"]);
    assert_eq!(held.code, 124, "{held:?}");
    assert!(held.stderr.contains("wait again"), "{held:?}");
}

#[test]
fn a_wait_ends_when_a_new_window_has_room() {
    let sandbox = utc();
    statusline(&sandbox, 97.0, now_secs() + 2 * HOUR);
    let waiting =
        crate::support::spawn(sandbox.factory_command(&["wait", "claude", "--timeout", "60s"]));
    assert!(
        poll_until(|| {
            sandbox
                .factory(&["status", "--json"])
                .stdout
                .parse::<Value>()
                .is_ok_and(|s| {
                    s["waits"]
                        .as_array()
                        .is_some_and(|w| w.iter().any(|w| w["kind"] == "claude"))
                })
        }),
        "the wait never registered"
    );

    statusline(&sandbox, 5.0, now_secs() + 5 * HOUR);

    let out = finish(waiting);
    assert_eq!(out.code, 0, "{out:?}");
}

#[test]
fn a_version_5_store_migrates_in_place_keeping_its_reading() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;
    std::fs::create_dir_all(sandbox.path("state/factory")).unwrap();
    let path = sandbox.path("state/factory/factory.db");
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE claude_quota (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 used_percent REAL,
                 resets_ms INTEGER,
                 seen_ms INTEGER,
                 limit_until_ms INTEGER,
                 limit_at_ms INTEGER
             );
             CREATE TABLE quota (
                 backend TEXT PRIMARY KEY,
                 state TEXT NOT NULL CHECK (state IN ('available', 'cooling')),
                 until_ms INTEGER
             );
             PRAGMA user_version = 5;",
        )
        .unwrap();
        db.execute(
            "INSERT INTO claude_quota (id, used_percent, resets_ms, seen_ms) VALUES (1, 40.0, ?1, ?2)",
            [resets * 1000, now_secs() * 1000],
        )
        .unwrap();
    }

    let out = quota(&sandbox);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        out.stdout,
        format!("claude: 5h 40% resets {}; dispatch\n", stamp(resets))
    );
    // The weekly window the migrated store now keeps outlives a restart, and the migrated
    // store opens again as it is.
    hook(&sandbox, &statusline_input(41.0, resets, 91.0));
    assert_eq!(quota(&sandbox).code, 75);
    for _ in 0..2 {
        crate::router::kill_daemon(&sandbox);
        let held = quota(&sandbox);
        assert_eq!(held.code, 75, "{held:?}");
        assert!(
            held.stderr
                .starts_with("factory: claude: hold: weekly 91%; hold new dispatches until "),
            "{held:?}"
        );
        assert!(held.stderr.contains("; 5h 41%, weekly 91%; "), "{held:?}");
    }
}

#[test]
fn a_store_from_a_newer_factory_is_refused() {
    let sandbox = utc();
    std::fs::create_dir_all(sandbox.path("state/factory")).unwrap();
    let db = rusqlite::Connection::open(sandbox.path("state/factory/factory.db")).unwrap();
    db.execute_batch("PRAGMA user_version = 7;").unwrap();
    drop(db);

    let out = quota(&sandbox);

    assert_eq!(out.code, 3, "{out:?}");
}
