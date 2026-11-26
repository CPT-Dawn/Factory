//! A phone with USB debugging off (#73): a USB device of a known Android vendor whose serial
//! adb doesn't list.

use factory_core::lab::ready::Readiness;
use factory_core::lab::usb::{UsbDevice, hidden_phones, vendor_name};

fn usb(vendor: &str, serial: &str) -> UsbDevice {
    UsbDevice {
        vendor: vendor.to_owned(),
        serial: serial.to_owned(),
    }
}

#[test]
fn a_known_vendor_device_adb_does_not_list_is_a_hidden_phone() {
    let seen = [usb("04e8", "R58M12345")];
    let hidden = hidden_phones(&seen, &[]);
    assert_eq!(
        hidden
            .iter()
            .map(|h| (h.serial.as_str(), h.vendor))
            .collect::<Vec<_>>(),
        vec![("R58M12345", "Samsung")]
    );
}

#[test]
fn a_phone_adb_lists_is_never_hidden_whatever_its_state() {
    let seen = [usb("18d1", "PIXEL01"), usb("04e8", "SAMS02")];
    assert!(hidden_phones(&seen, &["PIXEL01", "SAMS02"]).is_empty());
}

#[test]
fn a_device_of_no_android_vendor_is_ignored() {
    // A keyboard (Logitech) and a root hub (Linux Foundation).
    let seen = [usb("046d", "KEYB01"), usb("1d6b", "0000:00:14.0")];
    assert!(hidden_phones(&seen, &[]).is_empty());
}

#[test]
fn vendor_ids_match_in_any_case_and_each_phone_is_listed_once() {
    let seen = [usb("18D1", "A"), usb("18d1", "A"), usb("2717", "B")];
    let hidden = hidden_phones(&seen, &[]);
    assert_eq!(
        hidden
            .iter()
            .map(|h| (h.serial.as_str(), h.vendor))
            .collect::<Vec<_>>(),
        vec![("A", "Google"), ("B", "Xiaomi")]
    );
}

#[test]
fn the_list_names_the_vendors_the_spec_does() {
    for (id, name) in [
        ("18d1", "Google"),
        ("04e8", "Samsung"),
        ("2d95", "vivo"),
        ("2717", "Xiaomi"),
        ("2a70", "OnePlus"),
        ("22d9", "Oppo"),
        ("22b8", "Motorola"),
    ] {
        assert_eq!(vendor_name(id), Some(name), "{id}");
    }
    assert_eq!(vendor_name("046d"), None);
}

#[test]
fn the_readiness_word_is_usb_debugging_off_and_round_trips() {
    assert_eq!(Readiness::UsbDebuggingOff.word(), "usb-debugging-off");
    assert_eq!(
        Readiness::from_word("usb-debugging-off"),
        Some(Readiness::UsbDebuggingOff)
    );
    assert_eq!(
        serde_json::to_string(&Readiness::UsbDebuggingOff).unwrap(),
        "\"usb-debugging-off\""
    );
}
