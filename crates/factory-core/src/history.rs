//! Usage-window history: every reading the quota keeper stores is also kept here when it differs
//! from the one before it, for factory-stats to line quota burn up against nights, leads and
//! tokens (spec #67, stories 19 to 21).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::timestamp::Timestamp;
use crate::usage::{self, UsageWindow, WindowKind};

const MILLIS_PER_DAY: i64 = 86_400_000;

/// How long a row is kept.
pub const RETENTION_DAYS: i64 = 90;

/// How long after a prune the history is due the next: an hour short of a day, so a daemon
/// that looks a little late still prunes at least daily.
pub const PRUNE_EVERY_MS: i64 = MILLIS_PER_DAY - 3_600_000;

/// One reading of one usage window, as history keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryRow {
    /// The harness the reading is of: `claude`, and later `codex` and `agy`.
    pub harness: String,
    /// The quota group inside the harness, for a harness that meters several; none otherwise.
    pub group: Option<String>,
    pub window: WindowKind,
    /// 0 to 100, as the harness reported it.
    pub used_percent: f64,
    /// When the window resets, in milliseconds since the epoch.
    pub resets: i64,
    /// When the reading was taken, in milliseconds since the epoch.
    pub seen: i64,
}

impl HistoryRow {
    pub fn of(harness: &str, group: Option<&str>, window: &UsageWindow) -> HistoryRow {
        HistoryRow {
            harness: harness.to_owned(),
            group: group.map(str::to_owned),
            window: window.kind,
            used_percent: window.used_percent,
            resets: window.resets.unix_millis(),
            seen: window.seen.unix_millis(),
        }
    }

    /// Whether this reading adds to the history after `last`, the latest row of its harness,
    /// group and window: when it is the first, or its share or its reset changed. A reading
    /// that only came later is the same reading.
    pub fn adds_to(&self, last: Option<&HistoryRow>) -> bool {
        last.is_none_or(|last| {
            last.used_percent.to_bits() != self.used_percent.to_bits() || last.resets != self.resets
        })
    }

    /// `claude 5h 41% seen 2026-11-26T14:00:00.000Z`: a row as one line shows it.
    pub fn describe(&self) -> String {
        let name = match &self.group {
            Some(group) => format!("{}/{group}", self.harness),
            None => self.harness.clone(),
        };
        format!(
            "{name} {} {}% seen {}",
            self.window.label(),
            usage::whole_percent(self.used_percent),
            Timestamp::from_unix_millis(self.seen)
        )
    }
}

/// The oldest moment a row is kept from at `now`: older rows are pruned.
pub fn cutoff(now: Timestamp) -> Timestamp {
    Timestamp::from_unix_millis(now.unix_millis() - RETENTION_DAYS * MILLIS_PER_DAY)
}

/// Whether the history is due a prune at `now` by the wall clock, the last one having run at
/// `last`; never pruned is due.
pub fn prune_due(last: Option<Timestamp>, now: Timestamp) -> bool {
    last.is_none_or(|last| now.unix_millis() - last.unix_millis() >= PRUNE_EVERY_MS)
}

/// Whether `elapsed`, the time that has passed since the last prune by a clock that never
/// steps back, makes the history due one: the wall clock stepped back doesn't hold a prune off.
pub fn overdue(elapsed: Duration) -> bool {
    i128::try_from(elapsed.as_millis()).is_ok_and(|elapsed| elapsed >= i128::from(PRUNE_EVERY_MS))
}

/// The moment `--since` names at `now`: a UTC time (`2026-11-20T00:00:00Z`), a UTC date
/// (`2026-11-20`, from its midnight), or an age before now: `90` or `90s`, `30m`, `6h`, `7d`.
pub fn parse_since(text: &str, now: Timestamp) -> Result<Timestamp, String> {
    let bad = || {
        format!(
            "bad time {text:?}: give a UTC time such as 2026-11-20T00:00:00Z, a date such as 2026-11-20, or an age such as 6h or 7d"
        )
    };
    if let Some(at) = Timestamp::parse(text) {
        return Ok(at);
    }
    if text.len() == 10
        && let Some(at) = Timestamp::parse(&format!("{text}T00:00:00Z"))
    {
        return Ok(at);
    }
    let age = match text.strip_suffix('d') {
        Some(days) => days
            .parse::<u64>()
            .ok()
            .and_then(|days| days.checked_mul(86_400))
            .map(Duration::from_secs),
        None => crate::duration::parse(text).ok(),
    };
    let millis = age
        .and_then(|age| i64::try_from(age.as_millis()).ok())
        .ok_or_else(bad)?;
    now.unix_millis()
        .checked_sub(millis)
        .map(Timestamp::from_unix_millis)
        .ok_or_else(bad)
}

/// What a window's name is in the database, its serde name. A new window kind fails to
/// compile until it has one.
pub fn window_key(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::FiveHour => "five_hour",
        WindowKind::Weekly => "weekly",
    }
}

/// The window a database name is of: the windows `window_key` names, found by their key, so
/// the key is written once.
pub fn window_of(key: &str) -> Option<WindowKind> {
    [WindowKind::FiveHour, WindowKind::Weekly]
        .into_iter()
        .find(|kind| window_key(*kind) == key)
}
