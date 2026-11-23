//! The life of an emulator devlab boots: AVD `devlab-<i>`, run as the transient user unit
//! `devlab-emu-<i>` on console port `base + 2i` (serial `emulator-<port>`).
//!
//! An emulator is [State::Booting] from the grant that asks for it until the one who boots it
//! sees it boot, and [State::Ready] from then on. It is only ever marked ready by that look
//! ([boot_step]); a booting emulator whose booter died is stopped and forgotten
//! ([after_restart]), never marked ready.

/// Seconds a booting emulator has to finish.
pub const BOOT_TIMEOUT_SEC: u64 = 240;

/// The note acquire writes before it boots `n` emulators.
pub fn booting_note(n: usize) -> String {
    format!("booting {n} emulator(s), about a minute (quick boot) or three (first boot)")
}

pub fn unit_name(idx: u32) -> String {
    format!("devlab-emu-{idx}")
}

pub fn avd_name(idx: u32) -> String {
    format!("devlab-{idx}")
}

/// Emulator `idx`'s console port.
pub fn port_of(idx: u32, base: u32) -> u32 {
    base + 2 * idx
}

pub fn serial_of(idx: u32, base: u32) -> String {
    format!("emulator-{}", port_of(idx, base))
}

/// The `n` lowest indexes not in `used`.
pub fn free_indexes(used: &[u32], n: u32) -> Vec<u32> {
    (0..)
        .filter(|i| !used.contains(i))
        .take(n as usize)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Booting,
    Ready,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Booting => "booting",
            State::Ready => "ready",
        }
    }

    pub fn from_word(word: &str) -> State {
        if word == "ready" {
            State::Ready
        } else {
            State::Booting
        }
    }
}

/// One look at a booting emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    /// Its unit is active: the emulator process runs.
    pub unit_active: bool,
    /// adb lists it as `device`.
    pub attached: bool,
    /// `sys.boot_completed` is 1.
    pub boot_completed: bool,
    /// Seconds since its boot began.
    pub elapsed_sec: u64,
}

/// What a look says of a booting emulator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boot {
    /// Still booting: look again.
    Wait,
    Ready,
    /// It will not boot: why (exit 77). It is stopped and forgotten.
    Failed(String),
}

/// Ready only while its process runs, adb lists it and it says it finished booting.
pub fn boot_step(idx: u32, serial: &str, look: Look) -> Boot {
    if !look.unit_active {
        let unit = unit_name(idx);
        return Boot::Failed(format!(
            "{unit} exited while booting; see: journalctl --user -u {unit} -n 50"
        ));
    }
    if look.attached && look.boot_completed {
        return Boot::Ready;
    }
    if look.elapsed_sec >= BOOT_TIMEOUT_SEC {
        return Boot::Failed(format!(
            "{serial} did not finish booting in {BOOT_TIMEOUT_SEC} s"
        ));
    }
    Boot::Wait
}

/// What a daemon that starts does with an emulator an earlier daemon left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    Keep,
    /// Its booter died with that daemon: stop its unit and forget it.
    StopAndForget,
}

pub fn after_restart(state: State) -> Settle {
    match state {
        State::Ready => Settle::Keep,
        State::Booting => Settle::StopAndForget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn look(unit_active: bool, attached: bool, boot_completed: bool, elapsed_sec: u64) -> Look {
        Look {
            unit_active,
            attached,
            boot_completed,
            elapsed_sec,
        }
    }

    #[test]
    fn emulators_are_named_by_index() {
        assert_eq!(
            (unit_name(0), avd_name(1), serial_of(1, 5580)),
            (
                "devlab-emu-0".to_owned(),
                "devlab-1".to_owned(),
                "emulator-5582".to_owned()
            )
        );
        assert_eq!(free_indexes(&[0, 2], 2), vec![1, 3]);
    }

    #[test]
    fn a_boot_ends_ready_dead_or_late() {
        assert_eq!(
            boot_step(0, "emulator-5580", look(true, true, true, 3)),
            Boot::Ready
        );
        assert_eq!(
            boot_step(0, "emulator-5580", look(true, true, false, 3)),
            Boot::Wait
        );
        assert_eq!(
            boot_step(0, "emulator-5580", look(false, true, true, 3)),
            Boot::Failed(
                "devlab-emu-0 exited while booting; see: journalctl --user -u devlab-emu-0 -n 50"
                    .into()
            )
        );
        assert_eq!(
            boot_step(0, "emulator-5580", look(true, false, false, 240)),
            Boot::Failed("emulator-5580 did not finish booting in 240 s".into())
        );
    }

    #[test]
    fn a_booting_emulator_left_by_a_dead_daemon_is_stopped_not_kept() {
        assert_eq!(after_restart(State::Booting), Settle::StopAndForget);
        assert_eq!(after_restart(State::Ready), Settle::Keep);
    }

    proptest! {
        #[test]
        fn an_emulator_whose_process_died_is_never_ready(
            active in any::<bool>(), attached in any::<bool>(), done in any::<bool>(), t in 0u64..500
        ) {
            if boot_step(0, "emulator-5580", look(active, attached, done, t)) == Boot::Ready {
                prop_assert!(active && attached && done);
            }
        }
    }
}
