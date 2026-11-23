//! The clock and the local zone, which the core takes as arguments.

use std::time::{SystemTime, UNIX_EPOCH};

use factory_core::timestamp::Timestamp;

pub fn now() -> Timestamp {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp::from_unix_millis(i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX))
}

/// How many seconds east of UTC the local zone (`$TZ`, else the system's) is at `at`; UTC
/// when the zone can't be read.
pub fn utc_offset(at: Timestamp) -> i32 {
    let Ok(at) = jiff::Timestamp::from_millisecond(at.unix_millis()) else {
        return 0;
    };
    jiff::tz::TimeZone::system().to_offset(at).seconds()
}
