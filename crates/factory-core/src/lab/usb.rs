//! A phone with USB debugging off (#73): adb doesn't list it, so only the USB bus shows it is
//! there. A USB device of a known Android vendor whose serial adb doesn't list is one.

/// What the USB bus says of one device: its vendor id (four hex digits, as sysfs writes them)
/// and its serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub vendor: String,
    pub serial: String,
}

/// The known Android vendors, by USB vendor id: the one list a device is judged by.
pub const ANDROID_VENDORS: [(&str, &str); 13] = [
    ("18d1", "Google"),
    ("04e8", "Samsung"),
    ("2d95", "vivo"),
    ("2717", "Xiaomi"),
    ("2a70", "OnePlus"),
    ("22d9", "Oppo"),
    ("22b8", "Motorola"),
    ("0bb4", "HTC"),
    ("12d1", "Huawei"),
    ("19d2", "ZTE"),
    ("0fce", "Sony"),
    ("1004", "LG"),
    ("2ae5", "Fairphone"),
];

/// The vendor behind USB vendor id `id`, in any case; none when it makes no Android phone.
pub fn vendor_name(id: &str) -> Option<&'static str> {
    ANDROID_VENDORS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(id.trim()))
        .map(|(_, name)| *name)
}

/// A phone adb doesn't list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenPhone {
    pub serial: String,
    /// Its maker, as [`ANDROID_VENDORS`] names it.
    pub vendor: &'static str,
}

/// The phones on `seen` that adb doesn't list: those of a known vendor whose serial is not among
/// `listed`, the serials adb shows in any state. Each once, in the order seen.
pub fn hidden_phones(seen: &[UsbDevice], listed: &[&str]) -> Vec<HiddenPhone> {
    let mut hidden: Vec<HiddenPhone> = Vec::new();
    for device in seen {
        let Some(vendor) = vendor_name(&device.vendor) else {
            continue;
        };
        if device.serial.is_empty()
            || listed.contains(&device.serial.as_str())
            || hidden.iter().any(|h| h.serial == device.serial)
        {
            continue;
        }
        hidden.push(HiddenPhone {
            serial: device.serial.clone(),
            vendor,
        });
    }
    hidden
}
