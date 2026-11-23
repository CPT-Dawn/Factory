//! The context steward's rules.
//!
//! Claude Code logs every session to a JSONL file, one line per message. An assistant line
//! carries the usage of the request that produced it, and that request's input is the
//! session's context at the time. The steward keeps each session's latest reading.
//!
//! A lead over the ask threshold is asked, once, to stop at its next natural break; once it
//! has been idle for the grace period since, it is compacted. A lead with a registered wait is
//! never asked or compacted: it is idle on purpose. A subagent over its threshold is told
//! about to its lead, once.

use std::time::{Duration, Instant};

use serde_json::Value;

/// What one session-log line says about its session's context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The request's input: fresh, read from the cache and written to it.
    pub tokens: u64,
    /// Whether a subagent made the request (a line Claude Code marks `isSidechain`).
    pub sidechain: bool,
}

/// The context a session-log line reports, if it reports one.
pub fn reading(line: &str) -> Option<Reading> {
    // Most lines carry no usage, and some are megabytes long: skip them unparsed.
    if !line.contains("\"usage\"") {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let usage = value.get("message")?.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let tokens = count("input_tokens")
        .saturating_add(count("cache_read_input_tokens"))
        .saturating_add(count("cache_creation_input_tokens"));
    (tokens > 0).then(|| Reading {
        tokens,
        sidechain: value.get("isSidechain").and_then(Value::as_bool) == Some(true),
    })
}

/// The steward's thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The context at which an idle lead is asked to stop, then compacted.
    pub ask_at: u64,
    /// How long an asked lead must stay idle before it is compacted.
    pub grace: Duration,
    /// The context at which a subagent is told about to its lead.
    pub subagent_at: u64,
}

/// What the steward knows of one lead now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadNow {
    /// The lead's latest context; none before its log has said.
    pub tokens: Option<u64>,
    /// Since when the lead has been idle with no wait registered; none while it works.
    pub idle_since: Option<Instant>,
    /// Whether the lead has a wait registered.
    pub waiting: bool,
    /// When the steward asked the lead to stop; none when it hasn't since the lead last passed
    /// the threshold.
    pub asked_at: Option<Instant>,
}

/// What the steward does next with a lead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Ask it to stop at its next natural break.
    Ask,
    /// Compact it, then tell it to continue.
    Compact,
    /// It is back under the threshold: forget it was asked.
    Unask,
    /// Look again at this instant, when its grace period ends.
    At(Instant),
}

impl Policy {
    /// Decides a lead's next step at `now`.
    pub fn step(&self, lead: &LeadNow, now: Instant) -> Step {
        let Some(tokens) = lead.tokens else {
            return Step::Nothing;
        };
        if tokens < self.ask_at {
            return if lead.asked_at.is_some() {
                Step::Unask
            } else {
                Step::Nothing
            };
        }
        if lead.waiting {
            return Step::Nothing;
        }
        let Some(idle_since) = lead.idle_since else {
            return Step::Nothing;
        };
        let Some(asked_at) = lead.asked_at else {
            return Step::Ask;
        };
        // The grace period counts from the ask, or from the lead's going idle after it.
        let due = idle_since.max(asked_at) + self.grace;
        if now >= due {
            Step::Compact
        } else {
            Step::At(due)
        }
    }

    /// Whether a subagent at `tokens` is told about to its lead: once, and never once it has
    /// finished, since then there is no work left to hand on.
    pub fn tells(&self, tokens: u64, told: bool, finished: bool) -> bool {
        tokens >= self.subagent_at && !told && !finished
    }
}
