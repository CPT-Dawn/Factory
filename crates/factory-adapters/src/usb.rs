//! The USB bus as sysfs shows it, for the phone whose USB debugging is off (#73): adb doesn't
//! list it, but the bus does. Reading sysfs spawns no process.
//!
//! The root is `/sys` unless `FACTORY_SYSFS_ROOT` names another: the one seam tests use to lay
//! out a bus of their own (`bus/usb/devices/<n>/idVendor` and `serial`).

use std::path::{Path, PathBuf};

use factory_core::lab::usb::UsbDevice;

/// The variable that moves sysfs's root.
pub const ROOT_VAR: &str = "FACTORY_SYSFS_ROOT";

pub trait Usb: Send + Sync {
    /// Every USB device on the bus that has a vendor id and a serial.
    fn devices(&self) -> Vec<UsbDevice>;
}

/// Sysfs under a root.
pub struct Sysfs {
    root: PathBuf,
}

impl Sysfs {
    pub fn at(root: impl Into<PathBuf>) -> Sysfs {
        Sysfs { root: root.into() }
    }

    /// Sysfs under `$FACTORY_SYSFS_ROOT`, else `/sys`.
    pub fn from_env() -> Sysfs {
        Sysfs::at(
            std::env::var_os(ROOT_VAR)
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from("/sys"), PathBuf::from),
        )
    }
}

fn attribute(device: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(device.join(name)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

impl Usb for Sysfs {
    fn devices(&self) -> Vec<UsbDevice> {
        let Ok(entries) = std::fs::read_dir(self.root.join("bus/usb/devices")) else {
            return Vec::new();
        };
        let mut found: Vec<(String, UsbDevice)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                // An interface (`1-1:1.0`) has neither: only a device does.
                let vendor = attribute(&entry.path(), "idVendor")?;
                let serial = attribute(&entry.path(), "serial")?;
                Some((
                    entry.file_name().to_string_lossy().into_owned(),
                    UsbDevice { vendor, serial },
                ))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found.into_iter().map(|(_, device)| device).collect()
    }
}

/// A bus that shows what it is given.
pub struct FakeUsb(pub Vec<UsbDevice>);

impl Usb for FakeUsb {
    fn devices(&self) -> Vec<UsbDevice> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(root: &Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join("bus/usb/devices").join(name);
        std::fs::create_dir_all(&dir).expect("make the device");
        for (file, text) in files {
            std::fs::write(dir.join(file), text).expect("write the attribute");
        }
    }

    #[test]
    fn devices_come_with_their_vendor_and_serial_in_name_order() {
        let root = tempfile::tempdir().expect("a temp dir");
        device(
            root.path(),
            "2-1",
            &[("idVendor", "04e8\n"), ("serial", "SAMS02\n")],
        );
        device(
            root.path(),
            "1-3",
            &[("idVendor", "18d1\n"), ("serial", "PIXEL01\n")],
        );
        // No serial, an interface, and a vendor with a blank serial: none is a device adb could list.
        device(root.path(), "1-4", &[("idVendor", "046d\n")]);
        device(root.path(), "1-3:1.0", &[("bInterfaceClass", "ff\n")]);
        device(
            root.path(),
            "1-5",
            &[("idVendor", "18d1\n"), ("serial", "\n")],
        );
        let vendors: Vec<(String, String)> = Sysfs::at(root.path())
            .devices()
            .into_iter()
            .map(|d| (d.vendor, d.serial))
            .collect();
        assert_eq!(
            vendors,
            vec![
                ("18d1".to_owned(), "PIXEL01".to_owned()),
                ("04e8".to_owned(), "SAMS02".to_owned())
            ]
        );
    }

    #[test]
    fn a_root_with_no_bus_has_no_devices() {
        let root = tempfile::tempdir().expect("a temp dir");
        assert!(Sysfs::at(root.path()).devices().is_empty());
    }
}
