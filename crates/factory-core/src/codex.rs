//! The Codex plan's usage windows, as `codex app-server` reports them: its 5-hour and weekly
//! windows from `account/rateLimits/read` (JSON-RPC over stdio; it spends no model tokens), the
//! plan's name and its available rate-limit reset credits, and whether a review may dispatch on
//! Codex now (spec #67, stories 9 to 14).
//!
//! The adapter hands this module the reply line with id 2; nothing here starts a process or
//! reads a clock.

use serde_json::Value;

use crate::status::{self, ClaudeState};
use crate::timestamp::Timestamp;
use crate::usage::{self, UsageWindow, WindowKind};

/// How much of either window may be spent before a Codex review holds, by default.
pub const HOLD_AT_PERCENT: f64 = 90.0;

/// How long a reading, or a failure to read one, stands before the next ask: Codex is asked at
/// most 12 times an hour.
pub const FRESH_MILLIS: i64 = 300_000;

/// The length of the 5-hour window, in minutes.
const FIVE_HOUR_MINUTES: i64 = 300;
/// The length of the weekly window, in minutes.
const WEEKLY_MINUTES: i64 = 10_080;

/// What one answer of the app-server says.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    /// The windows it names, the 5-hour one first.
    pub windows: Vec<UsageWindow>,
    /// The plan, as Codex names it: `plus`, `pro`.
    pub plan: Option<String>,
    /// The rate-limit reset credits still available.
    pub reset_credits: u32,
}

/// Reads `reply`, the app-server's answer to `account/rateLimits/read`, seen at `now`. A
/// window is the 5-hour one at 300 minutes and the weekly one at 10080, whichever of
/// `primary` and `secondary` carries it; one of another length, or none, is left out. A window
/// of those two lengths whose share or reset can't be read makes the whole answer unreadable: it
/// must not pass for an absent window, which the keeper forgets. The reason an answer is no
/// usage, for the line `codex usage unreadable: <reason>`.
pub fn read_usage(reply: &str, now: Timestamp) -> Result<Usage, String> {
    let reply: Value =
        serde_json::from_str(reply).map_err(|_| "its answer is not JSON".to_owned())?;
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_owned);
        return Err(format!("it answered with an error: {message}"));
    }
    let limits = reply
        .pointer("/result/rateLimits")
        .filter(|limits| limits.is_object())
        .ok_or_else(|| "its answer has no rate limits".to_owned())?;
    let mut windows: Vec<UsageWindow> = Vec::new();
    for slot in ["primary", "secondary"] {
        let Some(read) = limits
            .get(slot)
            .map(|window| read_window(window, now))
            .transpose()?
            .flatten()
        else {
            continue;
        };
        if windows.iter().all(|seen| seen.kind != read.kind) {
            windows.push(read);
        }
    }
    windows.sort_by_key(|window| window.kind);
    if windows.is_empty() {
        return Err("its answer names no 5-hour or weekly window it can read".to_owned());
    }
    Ok(Usage {
        windows,
        plan: limits
            .get("planType")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reset_credits: reset_credits(&reply),
    })
}

/// A window: none when there is no window, or its length is neither the 5-hour nor the weekly
/// one; an error when it is one of those and its share or reset can't be read, or when it names
/// a length that is no whole number of minutes (so it may be one of those, torn).
fn read_window(window: &Value, now: Timestamp) -> Result<Option<UsageWindow>, String> {
    let kind = match window.get("windowDurationMins") {
        None | Some(Value::Null) => return Ok(None),
        Some(length) => match length.as_i64() {
            Some(FIVE_HOUR_MINUTES) => WindowKind::FiveHour,
            Some(WEEKLY_MINUTES) => WindowKind::Weekly,
            Some(_) => return Ok(None),
            None => return Err("one of its windows has a length that could not be read".to_owned()),
        },
    };
    let read = (|| {
        let used = window.get("usedPercent")?.as_f64()?;
        let resets = window.get("resetsAt")?.as_i64()?.checked_mul(1000)?;
        UsageWindow::read(kind, used, resets, now)
    })();
    read.map(Some)
        .ok_or_else(|| format!("its {} window could not be read", kind.label()))
}

/// The available reset credits: the detail rows marked `available`, or the count the answer
/// gives when it lists none or lists fewer (the backend may cap the list).
fn reset_credits(reply: &Value) -> u32 {
    let Some(credits) = reply.pointer("/result/rateLimitResetCredits") else {
        return 0;
    };
    let listed = credits
        .get("credits")
        .and_then(Value::as_array)
        .map_or(0, |rows| {
            rows.iter()
                .filter(|row| row.get("status").and_then(Value::as_str) == Some("available"))
                .count() as u64
        });
    let counted = credits
        .get("availableCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    u32::try_from(listed.max(counted)).unwrap_or(u32::MAX)
}

/// What is known of the Codex plan's quota: the last reading of each window, the plan and
/// reset credits it came with, and whether the last ask could be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexQuota {
    pub five_hour: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    pub plan: Option<String>,
    pub reset_credits: u32,
    /// Why the last ask could not be read; none once one could. Kept in memory only.
    pub unreadable: Option<String>,
    /// When Codex was last asked, read or not. Kept with the readings: a daemon that starts
    /// within 5 minutes of an ask does not ask again.
    pub asked: Option<Timestamp>,
}

impl CodexQuota {
    /// Whether the next look should ask Codex again at `now`: it was never asked, was asked
    /// five minutes ago, or the clock has since gone back.
    pub fn stale(&self, now: Timestamp) -> bool {
        self.asked.is_none_or(|asked| {
            let age = now.unix_millis().saturating_sub(asked.unix_millis());
            !(0..FRESH_MILLIS).contains(&age)
        })
    }

    /// Codex answered at `now`: its answer is the whole picture, so a window it leaves out is
    /// forgotten.
    pub fn see(&mut self, usage: Usage, now: Timestamp) {
        self.five_hour = usage
            .windows
            .iter()
            .find(|w| w.kind == WindowKind::FiveHour)
            .copied();
        self.weekly = usage
            .windows
            .iter()
            .find(|w| w.kind == WindowKind::Weekly)
            .copied();
        self.plan = usage.plan;
        self.reset_credits = usage.reset_credits;
        self.unreadable = None;
        self.asked = Some(now);
    }

    /// Codex could not be read at `now`, for `reason`. What was seen stands until its window
    /// resets.
    pub fn fail(&mut self, reason: impl Into<String>, now: Timestamp) {
        self.unreadable = Some(reason.into());
        self.asked = Some(now);
    }

    /// The windows a reading was seen of, the 5-hour one first.
    pub fn windows(&self) -> Vec<UsageWindow> {
        self.five_hour.into_iter().chain(self.weekly).collect()
    }

    /// What the windows say of dispatching at `now`, holding once `hold_at` percent of either
    /// window that still stands is spent.
    pub fn verdict(&self, now: Timestamp, hold_at: f64) -> usage::Verdict {
        usage::verdict(&self.windows(), now, hold_at)
    }

    /// The quota at `now` as `factory status`, the bar and `factory quota codex` record it.
    pub fn record(&self, now: Timestamp, hold_at: f64) -> status::Codex {
        let (verdict, until, hold) = match self.verdict(now, hold_at) {
            usage::Verdict::Unknown => (ClaudeState::Unknown, None, None),
            usage::Verdict::Go => (ClaudeState::Go, None, None),
            usage::Verdict::Hold { by } => (
                ClaudeState::Hold,
                Some(by.resets.unix_millis()),
                Some(format!(
                    "{} {}%",
                    by.kind.label(),
                    usage::whole_percent(by.used_percent)
                )),
            ),
        };
        status::Codex {
            verdict,
            until,
            hold,
            windows: self
                .windows()
                .iter()
                .map(|window| window.reading(now))
                .collect(),
            plan: self.plan.clone(),
            reset_credits: self.reset_credits,
            unreadable: self.unreadable.clone(),
        }
    }
}
