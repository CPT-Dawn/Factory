//! What a finished review's ledger event says to a caller waiting on it: its line and exit code.

use factory_core::event::Fields;
use factory_core::exit::Exit;
use factory_core::finished::{self, Finished};
use factory_core::review::{Counts, Reading, Verdict};
use serde_json::{Value, json};

fn event(value: Value) -> Fields {
    match value {
        Value::Object(fields) => fields,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn a_clean_review_is_exit_0_and_a_review_with_findings_is_exit_10() {
    let table = [
        ("clean", Exit::Ok),
        ("findings", Exit::NeedsAttention),
        // A verdict this build doesn't know asks for a look rather than passing.
        ("odd", Exit::NeedsAttention),
    ];
    for (verdict, exit) in table {
        let ended = finished::of(&event(json!({
            "type": "review", "label": "t1-r1", "outcome": "ok", "verdict": verdict,
            "blocking": 0, "advisory": 0, "question": 0, "report": "/r/t1-r1.md",
        })));
        assert_eq!(ended.exit, exit, "{verdict}");
    }
}

#[test]
fn the_line_is_the_one_the_review_itself_printed() {
    let counts = Counts {
        blocking: 2,
        advisory: 1,
        question: 3,
    };
    let reading = Reading {
        verdict: Verdict::Findings,
        counts,
        findings: None,
        tallies: Vec::new(),
    };
    let ended = finished::of(&event(json!({
        "type": "review", "label": "t1-r1", "outcome": "ok", "verdict": "findings",
        "blocking": 2, "advisory": 1, "question": 3, "report": "/r/t1-r1.md",
    })));
    assert_eq!(ended.line, reading.summary("t1-r1", "/r/t1-r1.md"));
    assert_eq!(
        ended.line,
        "review t1-r1: findings (2 blocking, 1 advisory, 3 question) → /r/t1-r1.md"
    );
}

#[test]
fn a_failed_review_says_why_and_where_its_files_are_and_asks_for_attention() {
    let table = [
        (
            json!({"label": "t1", "outcome": "error", "reason": "codex timed out after 1s", "report": "/r/t1.md"}),
            "review t1: error (codex timed out after 1s) → /r/t1.md",
        ),
        // Failed before a report was named: no arrow to nothing.
        (
            json!({"label": "t1", "outcome": "error", "reason": "codex exited 3"}),
            "review t1: error (codex exited 3)",
        ),
        (
            json!({"label": "t1", "outcome": "error"}),
            "review t1: error",
        ),
    ];
    for (fields, line) in table {
        let ended = finished::of(&event(fields));
        assert_eq!(
            (ended.line.as_str(), ended.exit),
            (line, Exit::NeedsAttention)
        );
    }
}

#[test]
fn counts_an_event_lacks_read_as_zero() {
    let ended = finished::of(&event(
        json!({"label": "t1", "verdict": "clean", "report": "/r"}),
    ));
    assert_eq!(
        ended,
        Finished {
            line: "review t1: clean (0 blocking, 0 advisory, 0 question) → /r".to_owned(),
            exit: Exit::Ok,
        }
    );
}
