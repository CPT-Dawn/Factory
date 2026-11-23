//! Durations as a command line takes them: a whole number and a unit, such as `90s`, `9m`, `2h`.

use std::fmt;
use std::time::Duration;

/// Reads `<n>s`, `<n>m` or `<n>h`.
pub fn parse(text: &str) -> Result<Duration, BadDuration> {
    let bad = || BadDuration(text.to_owned());
    let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (number, unit) = text.split_at(split);
    let n: u64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        _ => None,
    };
    seconds.map(Duration::from_secs).ok_or_else(bad)
}

/// Shows a duration in the largest unit that keeps it whole, as [`parse`] reads it back.
pub fn show(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds > 0 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds > 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadDuration(pub String);

impl fmt::Display for BadDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bad duration {:?}: give a whole number and s, m or h, such as 90s or 9m",
            self.0
        )
    }
}
