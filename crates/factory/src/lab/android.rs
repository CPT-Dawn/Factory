//! The lab's side of phones and emulators: following adb's device list, readying a device, and
//! looking at an emulator while it boots. The decisions are factory-core's (`lab::devices`,
//! `lab::emulator`); this runs the tools they need.

use std::io::Read;
use std::time::Duration;

use factory_adapters::adb::{Adb, TrackDevices};
use factory_adapters::emulator::{EmulatorSpec, Emulators};
use factory_core::lab::devices::{Attached, LOCK_PROBE, lock_warning, parse_track, prep_command};
use factory_core::lab::emulator::{Look, avd_name, port_of, serial_of, unit_name};

use super::wire::DeviceAsk;

/// How long after a stream ends a new one starts.
const RESTART: Duration = Duration::from_secs(3);

/// Follows adb's device list for as long as the process runs: `frame` gets each list a stream
/// shows, and `quiet` is told when a stream ended without showing one (adb missing or
/// failing), so that no one waits on it. A new stream starts [`RESTART`] after one ends.
pub fn track_forever(
    source: &dyn TrackDevices,
    frame: &dyn Fn(Vec<Attached>),
    quiet: &dyn Fn(),
) -> ! {
    loop {
        let mut shown = false;
        if let Ok(mut stream) = source.start() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                }
                let (frames, used) = parse_track(&buf);
                buf.drain(..used.min(buf.len()));
                for list in frames {
                    shown = true;
                    frame(list);
                }
            }
        }
        if !shown {
            quiet();
        }
        std::thread::sleep(RESTART);
    }
}

/// Readies a device for unattended runs; a warning when its lock screen stays up.
pub fn prep(
    adb: &dyn Adb,
    serial: &str,
    stay_awake: bool,
    animations_off: bool,
) -> Option<&'static str> {
    let _ = adb.shell(serial, &prep_command(stay_awake, animations_off));
    lock_warning(&adb.shell(serial, LOCK_PROBE).unwrap_or_default())
}

/// The device's ABI, as `DEVLAB_ABIS` shows it; none when adb can't tell.
pub fn abi(adb: &dyn Adb, serial: &str) -> Option<String> {
    adb.shell(serial, "getprop ro.product.cpu.abi")
        .map(|out| out.trim().to_owned())
        .filter(|abi| !abi.is_empty())
}

/// Emulator `idx` as the ask's machine runs it.
pub fn spec(idx: u32, ask: &DeviceAsk) -> EmulatorSpec {
    EmulatorSpec {
        idx,
        avd: avd_name(idx),
        unit: unit_name(idx),
        port: port_of(idx, ask.port_base),
        serial: serial_of(idx, ask.port_base),
        image: ask.image.clone(),
        device: ask.device.clone(),
        gpu: ask.gpu.clone(),
    }
}

/// How a booting emulator is doing: its unit, whether adb lists it, whether it finished booting.
pub fn look(
    emus: &dyn Emulators,
    adb: &dyn Adb,
    spec: &EmulatorSpec,
    attached: &[Attached],
    elapsed_sec: u64,
) -> Look {
    let listed = attached
        .iter()
        .any(|a| a.serial == spec.serial && a.state == "device");
    let unit_active = emus.unit_active(&spec.unit);
    let boot_completed = listed
        && unit_active
        && adb
            .shell(&spec.serial, "getprop sys.boot_completed")
            .is_some_and(|out| out.trim() == "1");
    Look {
        unit_active,
        attached: listed,
        boot_completed,
        elapsed_sec,
    }
}
