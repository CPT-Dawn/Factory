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
    /// Every USB device on the bus that has a vendor id and a serial, or why the bus can't be
    /// read: an unreadable bus is never an empty one.
    fn devices(&self) -> Result<Vec<UsbDevice>, String>;
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
    fn devices(&self) -> Result<Vec<UsbDevice>, String> {
        let bus = self.root.join("bus/usb/devices");
        let entries = std::fs::read_dir(&bus).map_err(|e| format!("{}: {e}", bus.display()))?;
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
        Ok(found.into_iter().map(|(_, device)| device).collect())
    }
}

/// A bus that shows what it is given, or fails with it.
pub struct FakeUsb(pub Result<Vec<UsbDevice>, String>);

impl Usb for FakeUsb {
    fn devices(&self) -> Result<Vec<UsbDevice>, String> {
        self.0.clone()
    }
}
