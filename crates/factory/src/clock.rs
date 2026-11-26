//! The clock and the local zone, which the core takes as arguments.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use factory_core::timestamp::Timestamp;

/// The wall clock, moved by the test clock's skew when a test set one.
pub fn now() -> Timestamp {
    Timestamp::from_unix_millis(lab_now_ms())
}

/// The variable that names the test clock's file: the seconds, possibly fractional or negative,
/// that the lab's wall clock runs ahead of the system's. Read on each look, so a test can move
/// time on while a daemon runs; set by no one but the tests (the lab's 2 h reservation expiry, the usage history's 90 days).
pub const SKEW_FILE: &str = "FACTORY_CLOCK_SKEW_FILE";

/// The lab's wall clock in ms since the epoch: the system's, plus the test clock's skew.
pub fn lab_now_ms() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis())
        .unwrap_or(i64::MAX)
        .saturating_add(skew_ms())
}

/// The test clock's skew in ms; 0 when no test set one, or its file can't be read.
fn skew_ms() -> i64 {
    static FILE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(file) = FILE.get_or_init(|| {
        std::env::var_os(SKEW_FILE)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    }) else {
        return 0;
    };
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite())
        .map_or(0, |secs| (secs * 1000.0) as i64)
}

/// How many seconds east of UTC the local zone (`$TZ`, else the system's) is at `at`; UTC
/// when the zone can't be read.
pub fn utc_offset(at: Timestamp) -> i32 {
    let Ok(at) = jiff::Timestamp::from_millisecond(at.unix_millis()) else {
        return 0;
    };
    jiff::tz::TimeZone::system().to_offset(at).seconds()
}
