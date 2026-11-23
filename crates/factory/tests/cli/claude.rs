//! The Claude plan's quota: the statusline and the StopFailure hook tell the daemon what they
//! saw; `factory quota claude` says whether a lead may dispatch, `factory wait claude` blocks
//! until it may, and `factory status` shows it.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::router::configure;
use crate::support::world::poll_until;
use crate::support::{Run, Sandbox, finish};

const HOUR: i64 = 3_600;

fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// The statusline input of a session whose 5-hour window is `used` percent spent and resets
/// at `resets` (seconds since the epoch).
fn statusline_input(used: f64, resets: i64) -> String {
    json!({
        "session_id": "s1",
        "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
        "workspace": {"current_dir": "/tmp/w"},
        "rate_limits": {
            "five_hour": {"used_percentage": used, "resets_at": resets},
            "seven_day": {"used_percentage": 12.0, "resets_at": resets + 86_400}
        }
    })
    .to_string()
}

fn statusline(sandbox: &Sandbox, used: f64, resets: i64) -> Run {
    let out = sandbox.factory_stdin(&["statusline"], &statusline_input(used, resets));
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    out
}

fn quota(sandbox: &Sandbox) -> Run {
    sandbox.factory(&["quota", "claude"])
}

/// `HH:MM` of a moment, in UTC (the sandbox runs with TZ=UTC).
fn clock(secs: i64) -> String {
    let minutes = secs.div_euclid(60).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn utc() -> Sandbox {
    Sandbox::new().env("TZ", "UTC")
}

#[test]
fn the_statusline_shows_the_model_and_the_window_in_one_line() {
    let sandbox = utc();
    let resets = now_secs() + 2 * HOUR;

    let out = statusline(&sandbox, 23.5, resets);

    assert_eq!(
        out.stdout,
        format!("Opus 5.5 · 5h 24% · resets {}\n", clock(resets))
    );
}

#[test]
fn a_statusline_without_rate_limits_shows_the_model_alone() {
    let sandbox = utc();
    let out = sandbox.factory_stdin(
        &["statusline"],
        r#"{"model": {"display_name": "Sonnet 5.5"}}"#,
    );
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "Sonnet 5.5\n"),
        "{out:?}"
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
    assert!(
        out.stdout
            .starts_with("claude: 40% of the 5-hour window used, resets "),
        "{out:?}"
    );
    assert!(out.stdout.ends_with("; dispatch\n"), "{out:?}");
    let shown: Value =
        serde_json::from_str(&sandbox.factory(&["quota", "claude", "--json"]).stdout).unwrap();
    assert_eq!(shown["verdict"], "go", "{shown}");
    assert_eq!(shown["used_percent"], 40.0, "{shown}");
    assert_eq!(shown["resets"], resets * 1000, "{shown}");
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
        out.stderr.starts_with(
            "factory: claude: 93% of the 5-hour window used; hold new dispatches until "
        ),
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
    let going = sandbox.factory(&["status"]);
    let line = going
        .stdout_lines()
        .into_iter()
        .find(|l| l.starts_with("claude:"))
        .unwrap_or_default()
        .to_owned();
    assert!(
        line.starts_with("claude: 40% of 5h used, resets "),
        "{going:?}"
    );
    assert!(line.ends_with(&format!("{}Z", clock(resets))), "{going:?}");

    statusline(&sandbox, 95.0, resets);
    let holding = sandbox.factory(&["status"]);
    let line = holding
        .stdout_lines()
        .into_iter()
        .find(|l| l.starts_with("claude:"))
        .unwrap_or_default()
        .to_owned();
    assert!(
        line.starts_with("claude: holding dispatches until "),
        "{holding:?}"
    );
    assert!(
        line.ends_with(&format!("{}Z, 95% of 5h used", clock(resets))),
        "{holding:?}"
    );
}

#[test]
fn a_wait_returns_at_once_with_room_and_times_out_while_holding() {
    let sandbox = utc();
    let free = sandbox.factory(&["wait", "claude", "--timeout", "5s"]);
    assert_eq!(
        (free.code, free.stdout.as_str()),
        (0, "claude: the window has room; dispatch\n"),
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
