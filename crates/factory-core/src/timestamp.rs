//! Moments as the ledger shows them.

use std::fmt;

const MILLIS_PER_DAY: i64 = 86_400_000;

/// A moment, as milliseconds since the Unix epoch.
///
/// It displays as JavaScript's `Date.prototype.toISOString` does, which is how factory-stats
/// has always written `ts`: UTC, millisecond precision, a `Z` suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const fn from_unix_millis(millis: i64) -> Timestamp {
        Timestamp(millis)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.0.div_euclid(MILLIS_PER_DAY));
        let ms = self.0.rem_euclid(MILLIS_PER_DAY);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    }
}

/// The proleptic Gregorian date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}
