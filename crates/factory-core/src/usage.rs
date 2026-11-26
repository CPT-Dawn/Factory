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

/// The latest reset a reading may name: the last millisecond of the year 9999.
pub const LAST_RESET_MILLIS: i64 = 253_402_300_799_999;

/// Whether `millis`, a reset in milliseconds since the epoch, is one a reading may name: from
/// the epoch to the year 9999.
pub fn representable(millis: i64) -> bool {
    (0..=LAST_RESET_MILLIS).contains(&millis)
}

impl UsageWindow {
    /// A reading of `kind` seen at `seen`: `used_percent` spent, resetting at `resets_millis`.
    /// None when the share is outside 0 to 100 or the reset isn't [`representable`]: a garbled
    /// reading must not replace a good one.
    pub fn read(
        kind: WindowKind,
        used_percent: f64,
        resets_millis: i64,
        seen: Timestamp,
    ) -> Option<UsageWindow> {
        ((0.0..=100.0).contains(&used_percent) && representable(resets_millis)).then(|| {
            UsageWindow {
                kind,
                used_percent,
                resets: Timestamp::from_unix_millis(resets_millis),
                seen,
            }
        })
    }

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
    /// The spent share, in whole percent rounded down, so a tone or a mark that a whole
    /// percent crosses reads the same from it as from the share itself.
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

    /// `5h 41% resets <reset>` while it stands, the reset written by `reset`; once its window
    /// has reset, as [`UsageReading::describe`] says.
    pub fn describe_with_reset(&self, now: i64, reset: impl FnOnce(i64) -> String) -> String {
        if self.current {
            format!("{} resets {}", self.describe(now), reset(self.resets))
        } else {
            self.describe(now)
        }
    }
}

/// One quota group's window as a reading saw it: agy's model groups each have a 5-hour and a
/// weekly window of their own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupWindow {
    pub window: WindowKind,
    pub state: WindowState,
}

/// What a reading says of one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WindowState {
    /// Some of it is spent, and it resets at `resets` (milliseconds since the epoch).
    Used { used_percent: u32, resets: i64 },
    /// None of it is spent, and nothing has started its clock: agy reports the reset of a
    /// window never touched as the Unix epoch.
    Unused,
    /// The reading can't be placed: no share, or a reset that is not a time.
    Unknown,
}

impl GroupWindow {
    /// The share spent at `now` (milliseconds since the epoch), in whole percent: none when
    /// the reading is unknown or its window has reset since. A window nobody has used is 0.
    pub fn used_percent(&self, now: i64) -> Option<u32> {
        match self.state {
            WindowState::Used {
                used_percent,
                resets,
            } if resets > now => Some(used_percent),
            WindowState::Unused => Some(0),
            _ => None,
        }
    }

    /// `5h 30%`, or `5h unknown` when no share stands.
    pub fn describe(&self, now: i64) -> String {
        let label = self.window.label();
        match self.used_percent(now) {
            Some(used) => format!("{label} {used}%"),
            None => format!("{label} unknown"),
        }
    }
}

/// A harness's quota group and its windows, the 5-hour one first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaGroup {
    pub name: String,
    pub windows: Vec<GroupWindow>,
}

impl QuotaGroup {
    /// Its windows as `describe` says each, joined by `separator`: `5h 30%, weekly 7%`.
    pub fn describe(&self, now: i64, separator: &str) -> String {
        let windows: Vec<String> = self.windows.iter().map(|w| w.describe(now)).collect();
        windows.join(separator)
    }

    /// The most spent of its windows' shares at `now`; none when none stands.
    pub fn worst(&self, now: i64) -> Option<u32> {
        self.windows
            .iter()
            .filter_map(|w| w.used_percent(now))
            .max()
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

/// A share as a whole percent, 0 to 100, rounded down: 89.6% is 89%, under a mark of 90.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn whole_percent(used: f64) -> u32 {
    used.floor().clamp(0.0, 100.0) as u32
}
