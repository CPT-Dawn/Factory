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

/// The line of a review that reached a verdict, the one `factory review` prints:
/// `review <label>: <verdict> (n blocking, n advisory, n question) → <report>`.
pub fn summary(label: &str, verdict: &str, counts: Counts, report: &str) -> String {
    let Counts {
        blocking,
        advisory,
        question,
    } = counts;
    format!(
        "review {label}: {verdict} ({blocking} blocking, {advisory} advisory, {question} question) → {report}"
    )
}

/// What the `review` event says: the counts it lacks read as zero, and a review that failed
/// says why and asks for attention, so a caller never takes a failure for a pass.
pub fn of(event: &Fields) -> Finished {
    let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
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
    Finished {
        line: summary(label, verdict, counts, report),
        exit: if verdict == Verdict::Clean.to_string() {
            Exit::Ok
        } else {
            Exit::NeedsAttention
        },
    }
}
