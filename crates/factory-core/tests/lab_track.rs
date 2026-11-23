//! How long the daemon waits before it restarts the adb device stream (#42): the wait doubles
//! with each restart that follows a stream that never got going, up to a cap.

use std::time::Duration;

use factory_core::lab::devices::restart_delay;

const BASE: Duration = Duration::from_secs(3);
const CAP: Duration = Duration::from_secs(60);

#[test]
fn the_first_restart_waits_the_base_and_each_next_one_twice_as_long() {
    let waits: Vec<u64> = (0..5)
        .map(|n| restart_delay(n, BASE, CAP).as_secs())
        .collect();
    assert_eq!(waits, [3, 6, 12, 24, 48]);
}

#[test]
fn the_wait_stops_growing_at_the_cap() {
    assert_eq!(restart_delay(5, BASE, CAP), CAP);
    assert_eq!(restart_delay(6, BASE, CAP), CAP);
}

#[test]
fn a_very_long_run_of_failures_does_not_overflow() {
    assert_eq!(restart_delay(u32::MAX, BASE, CAP), CAP);
}
