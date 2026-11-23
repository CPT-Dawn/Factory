//! The adb device stream's watch (#42): how far a stream has come through its lists, and how
//! long the daemon waits before it restarts one (the wait doubles with each restart that
//! follows a stream that never got going, up to a cap).

use std::time::Duration;

use factory_core::lab::devices::{ListFraming, restart_delay, same_devices};

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

#[test]
fn a_stream_with_nothing_yet_stands_between_lists() {
    let framing = ListFraming::default();
    assert!(!framing.mid_list());
}

#[test]
fn whole_lists_are_counted_and_leave_the_stream_between_lists() {
    let mut framing = ListFraming::default();
    // Two lists in one read: a phone, then none.
    let finished = framing.feed(b"0013FAKEPHONE01\tdevice\n0000");
    assert_eq!(finished, 2);
    assert!(!framing.mid_list());
}

#[test]
fn a_list_cut_anywhere_stands_mid_list_until_its_last_byte() {
    let stream = b"0013FAKEPHONE01\tdevice\n";
    for cut in 1..stream.len() {
        let mut framing = ListFraming::default();
        let (head, tail) = stream.split_at(cut);
        assert_eq!(framing.feed(head), 0, "cut at {cut}");
        assert!(framing.mid_list(), "cut at {cut}");
        assert_eq!(framing.feed(tail), 1, "cut at {cut}");
        assert!(!framing.mid_list(), "cut at {cut}");
    }
}

#[test]
fn a_list_that_ends_where_the_next_begins_stands_mid_list() {
    let mut framing = ListFraming::default();
    assert_eq!(framing.feed(b"000000"), 1);
    assert!(framing.mid_list());
}

#[test]
fn a_length_that_is_not_hex_never_comes_whole() {
    let mut framing = ListFraming::default();
    assert_eq!(framing.feed(b"zzzz0000"), 0);
    assert!(framing.mid_list());
}

#[test]
fn the_last_whole_list_is_kept_and_a_list_under_way_is_not() {
    let mut framing = ListFraming::default();
    assert_eq!(framing.last_list(), None);
    framing.feed(b"0013FAKEPHONE01\tdevice\n0013FAKE");
    assert_eq!(framing.last_list(), Some(&b"FAKEPHONE01\tdevice\n"[..]));
    framing.feed(b"PHONE02\tdevice\n0000");
    assert_eq!(framing.last_list(), Some(&b""[..]));
}

#[test]
fn a_list_names_the_same_devices_as_adb_devices_whatever_their_order_and_properties() {
    let list = b"FAKEPHONE01\tdevice usb:1-1 model:Pixel_8\nemulator-5580\toffline\n";
    let devices = "List of devices attached\nemulator-5580          offline\nFAKEPHONE01            device usb:1-1 product:x model:Pixel_8\n";
    assert!(same_devices(list, devices));
}

#[test]
fn a_device_adb_lists_that_the_list_lacks_or_shows_in_another_state_differs() {
    let list = b"FAKEPHONE01\tdevice\n";
    assert!(!same_devices(
        list,
        "List of devices attached\nFAKEPHONE01 device\nFAKEPHONE02 device\n"
    ));
    assert!(!same_devices(
        list,
        "List of devices attached\nFAKEPHONE01 unauthorized\n"
    ));
    assert!(same_devices(b"", "List of devices attached\n\n"));
}
