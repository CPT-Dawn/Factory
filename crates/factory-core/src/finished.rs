//! What a finished review's ledger event says to a caller waiting on it: the line `factory
//! wait review` prints and the exit code it ends with.

use serde_json::Value;

use crate::event::Fields;
use crate::exit::Exit;
use crate::review::{Counts, Verdict};

/// A finished review as a caller sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub line: String,
    /// [`Exit::Ok`] for a clean review, `Exit::NeedsAttention` for anything else.
    pub exit: Exit,
}

/// What a fix round settled, as its line says: findings fixed, still present, and new ones dropped for
/// lying outside the fix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fixes {
    pub fixed: u64,
    pub still_present: u64,
    pub dropped: u64,
}

/// The line of a review that reached a verdict, the one `factory review` prints:
/// `review <label>: <verdict> (n blocking, n advisory, n question[; n fixed, ...]) → <report>`.
pub fn summary(label: &str, verdict: &str, counts: Counts, fixes: Fixes, report: &str) -> String {
    let Counts {
        blocking,
        advisory,
        question,
    } = counts;
    // A fix round adds what it settled, what it left and what it dropped, when any.
    let fix: Vec<String> = [
        (fixes.fixed, "fixed"),
        (fixes.still_present, "still present"),
        (fixes.dropped, "dropped"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    let fix = if fix.is_empty() {
        String::new()
    } else {
        format!("; {}", fix.join(", "))
    };
    format!(
        "review {label}: {verdict} ({blocking} blocking, {advisory} advisory, {question} question{fix}) → {report}"
    )
}

/// What the `review` event says: the counts it lacks read as zero, and a review that failed
/// says why and asks for attention, so a caller never takes a failure for a pass.
pub fn of(event: &Fields) -> Finished {
    // A label of digits is stored as a number, as the ledger's own readers know.
    let text = |key: &str| match event.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    let count = |key: &str| event.get(key).and_then(Value::as_u64).unwrap_or(0);
    let label = text("label");
    let report = text("report");
    if text("outcome") == "error" {
        let mut line = format!("review {label}: error");
        if !text("reason").is_empty() {
            line.push_str(&format!(" ({})", text("reason")));
        }
        if !report.is_empty() {
            line.push_str(&format!(" → {report}"));
        }
        return Finished {
            line,
            exit: Exit::NeedsAttention,
        };
    }
    let verdict = text("verdict");
    let counts = Counts {
        blocking: count("blocking"),
        advisory: count("advisory"),
        question: count("question"),
    };
    let fixes = Fixes {
        fixed: count("fixed"),
        still_present: count("still_present"),
        dropped: count("dropped"),
    };
    Finished {
        line: summary(&label, &verdict, counts, fixes, &report),
        exit: if verdict == Verdict::Clean.to_string() {
            Exit::Ok
        } else {
            Exit::NeedsAttention
        },
    }
}
