//! The lab's side of phones and emulators: following adb's device list, readying a device, and
//! looking at an emulator while it boots. The decisions are factory-core's (`lab::devices`,
//! `lab::emulator`); this runs the tools they need.

use std::io::Read;
use std::time::{Duration, Instant};

use factory_adapters::adb::{Adb, Pace, TrackDevices};
use factory_adapters::emulator::{EmulatorSpec, Emulators};
use factory_core::lab::devices::{
    Attached, LOCK_PROBE, parse_track, prep_command, prep_warning, restart_delay,
};
use factory_core::lab::emulator::{Look, avd_name, port_of, serial_of, unit_name};

use super::wire::DeviceAsk;

/// Follows adb's device list for as long as the process runs: `frame` gets each list a stream
/// shows; `lost` is told each time a stream ends, since the list it showed last is then no
/// longer followed; and `quiet` is told when it ended without showing one (adb missing or
/// failing), so that no one waits on it. A stream that ends is replaced after a wait that
/// doubles with each stream in a row that did not get going, from `pace.restart_base` to
/// `pace.restart_cap`; `log` is told one line before each wait.
pub fn track_forever(
    source: &dyn TrackDevices,
    pace: Pace,
    frame: &dyn Fn(Vec<Attached>),
    lost: &dyn Fn(),
    quiet: &dyn Fn(),
    log: &dyn Fn(&str),
) -> ! {
    let mut restarts: u32 = 0;
    loop {
        let began = Instant::now();
        let mut shown = false;
        let ended = match source.start() {
            Ok(mut stream) => {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break "ended".to_owned(),
                        Err(e) => break e.to_string(),
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
            Err(e) => format!("cannot start: {e}"),
        };
        lost();
        if !shown {
            quiet();
        }
        // A stream that showed a list and then ran for as long as the longest wait was healthy.
        if shown && began.elapsed() >= pace.restart_cap {
            restarts = 0;
        }
        let wait = restart_delay(restarts, pace.restart_base, pace.restart_cap);
        restarts = restarts.saturating_add(1);
        log(&format!(
            "adb track-devices: {ended}; restarting in {} (restart {restarts})",
            show_wait(wait)
        ));
        std::thread::sleep(wait);
    }
}

/// A wait as the log shows it: whole seconds, else milliseconds.
fn show_wait(wait: Duration) -> String {
    if wait.subsec_millis() == 0 {
        format!("{}s", wait.as_secs())
    } else {
        format!("{}ms", wait.as_millis())
    }
}

/// Readies a device for unattended runs; a warning when its lock screen stays up or adb can't
/// reach it.
pub fn prep(
    adb: &dyn Adb,
    serial: &str,
    stay_awake: bool,
    animations_off: bool,
) -> Option<&'static str> {
    let prepped = adb
        .shell(serial, &prep_command(stay_awake, animations_off))
        .is_some();
    prep_warning(prepped, adb.shell(serial, LOCK_PROBE).as_deref())
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
