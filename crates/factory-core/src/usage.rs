//! Usage windows: one rolling allowance of a harness's plan, its 5-hour or its weekly window,
//! with a share used and a reset, and the verdict every harness's quota shares: hold at a mark
//! on any window, and know nothing of a window once its reset has come.

use serde::{Deserialize, Serialize};

use crate::timestamp::Timestamp;

/// Which of a plan's windows a reading is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
}

impl WindowKind {
    /// How the screen, the quota line and the bar name it: `5h` or `weekly`.
    pub const fn label(self) -> &'static str {
        match self {
            WindowKind::FiveHour => "5h",
            WindowKind::Weekly => "weekly",
        }
    }
}

/// One window as a reading saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindow {
    pub kind: WindowKind,
    /// 0 to 100.
    pub used_percent: f64,
    pub resets: Timestamp,
    /// When the reading was taken.
    pub seen: Timestamp,
}

impl UsageWindow {
    /// Whether the reading still stands at `now`: its window has not reset since.
    pub fn current(&self, now: Timestamp) -> bool {
        self.resets > now
    }

    /// The reading as `factory status` and the bar record it at `now`.
    pub fn reading(&self, now: Timestamp) -> UsageReading {
        UsageReading {
            window: self.kind,
            used_percent: whole_percent(self.used_percent),
            resets: self.resets.unix_millis(),
            seen: self.seen.unix_millis(),
            current: self.current(now),
        }
    }
}

/// One window's last reading, as a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReading {
    pub window: WindowKind,
    /// The spent share, rounded.
    pub used_percent: u32,
    /// When the window resets, in milliseconds since the epoch.
    pub resets: i64,
    /// When the reading was taken, in milliseconds since the epoch.
    pub seen: i64,
    /// Whether it still stands. Once its window has reset, the share is unknown.
    pub current: bool,
}

impl UsageReading {
    /// `5h 41%`, or `5h unknown (seen 3h ago)` once its window has reset; `now` in
    /// milliseconds since the epoch.
    pub fn describe(&self, now: i64) -> String {
        let label = self.window.label();
        if self.current {
            format!("{label} {}%", self.used_percent)
        } else {
            format!(
                "{label} unknown (seen {} ago)",
                crate::lab::ago(now / 1000, self.seen / 1000)
            )
        }
    }
}

/// What a harness's windows say of dispatching now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    /// No window's reading still stands.
    Unknown,
    /// Every window that stands has room.
    Go,
    /// `by`, a window over the mark, holds dispatch until it resets: of several over the
    /// mark, the one that resets last.
    Hold { by: UsageWindow },
}

/// The verdict of `windows` at `now`, holding once `hold_at` percent of any window that still
/// stands is spent.
pub fn verdict(windows: &[UsageWindow], now: Timestamp, hold_at: f64) -> Verdict {
    let standing = windows.iter().filter(|window| window.current(now));
    let holding = standing
        .clone()
        .filter(|window| window.used_percent >= hold_at)
        .max_by_key(|window| window.resets);
    match (holding, standing.count()) {
        (Some(by), _) => Verdict::Hold { by: *by },
        (None, 0) => Verdict::Unknown,
        (None, _) => Verdict::Go,
    }
}

/// A share as a whole percent, 0 to 100.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn whole_percent(used: f64) -> u32 {
    used.round().clamp(0.0, 100.0) as u32
}
