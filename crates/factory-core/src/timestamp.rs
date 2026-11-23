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

    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// Reads a UTC moment in ISO 8601, `2026-11-30T00:21:20Z`, with or without a fraction of a
    /// second (kept to the millisecond). None for anything else, a local time included.
    pub fn parse(text: &str) -> Option<Timestamp> {
        let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
        let mut date = date.splitn(3, '-').map(digits);
        let (year, month, day) = (date.next()??, date.next()??, date.next()??);
        let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
        let mut time = time.splitn(3, ':').map(digits);
        let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
        let millis = match fraction {
            "" => 0,
            _ => digits(format!("{fraction:0<3}").get(..3)?)?,
        };
        let valid = (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day)
            && hour < 24
            && minute < 60
            && second < 60
            && (fraction.is_empty() || digits(fraction).is_some());
        valid.then(|| {
            let seconds =
                days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
            Timestamp(seconds * 1000 + millis)
        })
    }
}

/// The number written in `text`, which must be ASCII digits only.
fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || text.len() > 9 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The days from 1970-01-01 to this proleptic Gregorian date, by Howard Hinnant's
/// `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
