//! `factory log <type> key=value…`: one event lands in the ledger, written by the daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::Value;

use crate::support::{Sandbox, finish, lines_of, spawn};

/// Splits a ledger line into its timestamp and the rest, checking the timestamp has the shape
/// JavaScript's `toISOString` gives (UTC, milliseconds, `Z`), as factory-stats writes it.
fn split_ts(line: &str) -> (&str, String) {
    let ts = line
        .strip_prefix("{\"ts\":\"")
        .and_then(|rest| rest.get(..24))
        .unwrap_or_else(|| panic!("no leading ts in {line}"));
    let shape: String = ts
        .chars()
        .map(|c| if c.is_ascii_digit() { 'd' } else { c })
        .collect();
    assert_eq!(shape, "dddd-dd-ddTdd:dd:dd.dddZ", "ts {ts}");
    (ts, line.replacen(ts, "<ts>", 1))
}

#[test]
fn log_appends_one_event_with_a_timestamp() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["log", "review", "repo=X", "label=Y", "round=1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = sandbox.ledger_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let (ts, rest) = split_ts(&lines[0]);
    assert_eq!(
        rest,
        r#"{"ts":"<ts>","type":"review","repo":"X","label":"Y","round":1}"#
    );
    assert_eq!(out.stdout_lines(), vec![format!("logged review at {ts}")]);
    assert_eq!(out.stderr, "");
}

#[test]
fn log_records_the_session_it_runs_in() {
    let sandbox = Sandbox::new().env("CLAUDE_CODE_SESSION_ID", "s-1");

    let out = sandbox.factory(&["log", "standin", "repo=X", "kind=answer"]);

    assert_eq!(out.code, 0, "{out:?}");
    let (_, rest) = split_ts(&sandbox.ledger_lines()[0]);
    assert_eq!(
        rest,
        r#"{"ts":"<ts>","type":"standin","repo":"X","kind":"answer","session":"s-1"}"#
    );
}

#[test]
fn log_writes_the_ledger_factory_state_names() {
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.path("elsewhere");
    let sandbox = sandbox.env("FACTORY_STATE", &elsewhere);

    let out = sandbox.factory(&["log", "lead-start", "repo=X"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lines_of(&elsewhere.join("events.jsonl")).len(), 1);
    assert!(sandbox.ledger_lines().is_empty());
}

#[test]
fn a_malformed_pair_is_refused_and_nothing_is_appended() {
    let sandbox = Sandbox::new();
    sandbox.factory(&["log", "review", "n=1"]);

    for args in [
        &["log", "review", "repo=X", "label"][..],
        &["log", "review", "=Y"],
        &["log", "Review", "repo=X"],
    ] {
        let out = sandbox.factory(args);

        assert_eq!(out.code, 2, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr_lines().len(), 1, "{args:?}: {out:?}");
        assert!(out.stderr.starts_with("factory: "), "{out:?}");
    }
    let out = sandbox.factory(&["log", "review", "repo=X", "label"]);
    assert_eq!(out.stderr, "factory: expected key=value, got: label\n");
    assert_eq!(sandbox.ledger_lines().len(), 1);
}

#[test]
fn log_exits_3_in_one_line_when_the_daemon_cannot_start() {
    let sandbox = Sandbox::new();
    let not_a_dir = sandbox.path("state-is-a-file");
    std::fs::write(&not_a_dir, "").unwrap();
    let sandbox = sandbox.env("FACTORY_STATE", &not_a_dir);

    let out = sandbox.factory(&["log", "review", "repo=X"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    let lines = out.stderr_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(
        lines[0].starts_with("factory: daemon unreachable at "),
        "{out:?}"
    );
    assert!(lines[0].contains("cannot create the state dir"), "{out:?}");
}

#[test]
fn log_exits_3_when_the_runtime_dir_is_unusable() {
    let sandbox = Sandbox::new();
    let not_a_dir = sandbox.path("run-is-a-file");
    std::fs::write(&not_a_dir, "").unwrap();
    let sandbox = sandbox.env("XDG_RUNTIME_DIR", &not_a_dir);

    let out = sandbox.factory(&["log", "review", "repo=X"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("check XDG_RUNTIME_DIR"), "{out:?}");
    assert!(sandbox.ledger_lines().is_empty());
}

#[test]
fn two_hundred_concurrent_logs_land_two_hundred_intact_lines() {
    let sandbox = Sandbox::new();

    let children: Vec<_> = (0..200)
        .map(|n| spawn(sandbox.factory_command(&["log", "review", "repo=X", &format!("n={n}")])))
        .collect();
    for child in children {
        let out = finish(child);
        assert_eq!(out.code, 0, "{out:?}");
    }

    let mut seen: Vec<i64> = sandbox
        .ledger_lines()
        .iter()
        .map(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                (event["type"].as_str(), event["repo"].as_str()),
                (Some("review"), Some("X"))
            );
            event["n"].as_str().unwrap().parse::<i64>().unwrap()
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, (0..200).collect::<Vec<_>>());
}

#[test]
fn log_json_prints_one_object_naming_the_event_and_the_ledger() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["log", "--json", "review", "repo=X"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    let landed: Value = serde_json::from_str(&sandbox.ledger_lines()[0]).unwrap();
    assert_eq!(shown["event"], landed);
    assert_eq!(
        shown["ledger"].as_str().map(std::path::PathBuf::from),
        Some(sandbox.ledger())
    );
}

#[test]
fn a_refusal_with_json_is_one_object_on_stdout() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["--json", "log", "review", "label"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stderr, "");
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        shown,
        serde_json::json!({"exit": 2, "error": "expected key=value, got: label"})
    );
}

#[test]
fn a_state_dir_that_is_not_utf8_still_gets_its_event() {
    use std::os::unix::ffi::OsStrExt;
    let sandbox = Sandbox::new();
    let state = sandbox.path(std::ffi::OsStr::from_bytes(b"state-\xff"));
    let sandbox = sandbox.env("FACTORY_STATE", &state);

    let out = sandbox.factory(&["log", "review", "repo=X"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lines_of(&state.join("events.jsonl")).len(), 1);
}

#[test]
fn the_confirmation_stays_one_short_line_however_long_the_event() {
    let sandbox = Sandbox::new();
    let long = format!("ts={}", "x".repeat(300));

    let out = sandbox.factory(&["log", "review", &long]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    assert!(out.stdout_lines()[0].chars().count() <= 200, "{out:?}");
    assert!(sandbox.ledger_lines()[0].contains(&"x".repeat(300)));
}

#[test]
fn a_sha_of_digits_stays_a_string_and_only_counts_become_numbers() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&[
        "log",
        "review",
        "head=0475004",
        "from=4750048",
        "round=2",
        "duration_s=158",
    ]);

    assert_eq!(out.code, 0, "{out:?}");
    let (_, rest) = split_ts(&sandbox.ledger_lines()[0]);
    assert_eq!(
        rest,
        r#"{"ts":"<ts>","type":"review","head":"0475004","from":"4750048","round":2,"duration_s":158}"#
    );
}

#[test]
fn the_daemon_keeps_a_typed_sha_a_string_and_a_count_a_number() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.factory(&["log", "review", "n=1"]).code, 0);
    let mut stream = UnixStream::connect(sandbox.path("run/factory/factory.sock")).unwrap();
    let request = r#"{"id":1,"method":"log","params":{"type":"review","fields":{"head":4750048,"round":"2","tokens":1426343}}}"#;

    writeln!(stream, "{request}").unwrap();

    let answered = BufReader::new(&stream)
        .lines()
        .map(Result::unwrap)
        .map(|line| serde_json::from_str::<Value>(&line).unwrap())
        .find(|answer| answer.get("note").is_none())
        .unwrap();
    assert!(answered.get("result").is_some(), "{answered}");
    let lines = sandbox.ledger_lines();
    let (_, rest) = split_ts(&lines[1]);
    assert_eq!(
        rest,
        r#"{"ts":"<ts>","type":"review","head":"4750048","round":2,"tokens":1426343}"#
    );
}
