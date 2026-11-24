//! What the daemon stops because nothing used it for long enough, as devlab's watcher did
//! (#18): a ready emulator no lease holds, after the emulator timer, and a stack devlab started
//! on a slot no lease holds, after the stack timer. A stack started outside devlab is never
//! stopped. And what it takes back (#57): a device whose lease ran nothing for the device timer,
//! only while a waiter needs that kind of device.

use super::devices::Mode;

/// The idle timers, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timers {
    pub emulator_sec: f64,
    pub stack_sec: f64,
    /// How long a leased device's lease may run nothing before a waiter may take the device.
    pub device_sec: f64,
}

impl Timers {
    /// The timers from the machine config's minutes.
    pub fn from_minutes(emulator: f64, stack: f64, device: f64) -> Timers {
        Timers {
            emulator_sec: emulator * 60.0,
            stack_sec: stack * 60.0,
            device_sec: device * 60.0,
        }
    }
}

/// An emulator in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleEmulator {
    pub idx: u32,
    /// It finished booting.
    pub ready: bool,
    /// A lease holds it.
    pub held: bool,
    /// Since when no lease holds it, in seconds since the epoch.
    pub idle_since: Option<i64>,
}

/// A stack slot in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleSlot {
    pub project: String,
    pub slot: u32,
    /// devlab started its stack.
    pub stack_ours: bool,
    /// A lease holds the slot.
    pub held: bool,
    /// The worktree it last ran, whose config its stack is stopped with.
    pub last_worktree: Option<String>,
    /// Since when no lease holds it, in seconds since the epoch.
    pub idle_since: Option<i64>,
}

/// A device a lease holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleDevice {
    pub lease: i64,
    pub serial: String,
    /// A phone; else an emulator.
    pub phone: bool,
    /// Its lease is in use: a run or an exec of it runs, or a request of it waits.
    pub busy: bool,
    /// When its lease was last used, in seconds since the epoch.
    pub touched: i64,
}

/// What a request that waits for devices asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    pub mode: Mode,
    pub want: u32,
}

/// What `needs` still lacks once the free devices go to it: `free_phones` and `free_emulators`
/// nobody holds go to the needs of their kind first, then to those that take either. A device
/// just taken back for a waiter is free until the waiter's grant holds it, so its need asks for
/// no other.
pub fn unmet(needs: &[Need], free_phones: u32, free_emulators: u32) -> Vec<Need> {
    let (mut phones, mut emulators) = (free_phones, free_emulators);
    let take = |want: u32, free: &mut u32| {
        let met = want.min(*free);
        *free -= met;
        want - met
    };
    let mut left: Vec<Need> = needs
        .iter()
        .map(|n| Need {
            mode: n.mode,
            want: match n.mode {
                Mode::Phone => take(n.want, &mut phones),
                Mode::Emulator => take(n.want, &mut emulators),
                Mode::Any => n.want,
            },
        })
        .collect();
    for n in left.iter_mut().filter(|n| n.mode == Mode::Any) {
        let want = take(n.want, &mut phones);
        n.want = take(want, &mut emulators);
    }
    left.retain(|n| n.want > 0);
    left
}

/// What to stop now, and when the next thing falls due.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sweep {
    /// Emulators to stop, by index.
    pub emulators: Vec<u32>,
    /// Stacks to stop, by project and slot.
    pub stacks: Vec<(String, u32)>,
    /// Devices to take off their idle leases for the waiters, by lease and serial.
    pub devices: Vec<(i64, String)>,
    /// When the soonest of the rest falls due, in seconds since the epoch; none when nothing
    /// idles.
    pub next: Option<f64>,
}

/// What is due at `now` (seconds since the epoch). A held device is due only while `needs`
/// asks for its kind, and no more of them than the waiters want, the longest idle first.
pub fn sweep(
    emulators: &[IdleEmulator],
    slots: &[IdleSlot],
    devices: &[IdleDevice],
    needs: &[Need],
    timers: Timers,
    now: f64,
) -> Sweep {
    let mut due = Sweep::default();
    for e in emulators.iter().filter(|e| e.ready && !e.held) {
        if let Some(at) = falls_due(e.idle_since, timers.emulator_sec) {
            if at <= now {
                due.emulators.push(e.idx);
            } else {
                due.later(at);
            }
        }
    }
    let stoppable = |s: &&IdleSlot| s.stack_ours && !s.held && s.last_worktree.is_some();
    for s in slots.iter().filter(stoppable) {
        if let Some(at) = falls_due(s.idle_since, timers.stack_sec) {
            if at <= now {
                due.stacks.push((s.project.clone(), s.slot));
            } else {
                due.later(at);
            }
        }
    }
    due.reclaim(devices, needs, timers.device_sec, now);
    due
}

impl Sweep {
    /// Takes the idle devices the waiters need: one asking for its kind first, then one that
    /// takes any kind.
    fn reclaim(&mut self, devices: &[IdleDevice], needs: &[Need], timer: f64, now: f64) {
        let wanted = |mode: Mode| -> u32 {
            needs
                .iter()
                .filter(|n| n.mode == mode)
                .map(|n| n.want)
                .sum()
        };
        let (mut phones, mut emulators, mut any) = (
            wanted(Mode::Phone),
            wanted(Mode::Emulator),
            wanted(Mode::Any),
        );
        let mut idle: Vec<&IdleDevice> = devices.iter().filter(|d| !d.busy).collect();
        idle.sort_by_key(|d| (d.touched, d.lease));
        for d in idle {
            let own = if d.phone { &mut phones } else { &mut emulators };
            let Some(at) = falls_due(Some(d.touched), timer) else {
                continue;
            };
            if *own == 0 && any == 0 {
                continue;
            }
            if at > now {
                self.later(at);
                continue;
            }
            if *own > 0 {
                *own -= 1;
            } else {
                any -= 1;
            }
            self.devices.push((d.lease, d.serial.clone()));
        }
    }

    fn later(&mut self, at: f64) {
        self.next = Some(self.next.map_or(at, |next| next.min(at)));
    }
}

/// When something idle since `since` falls due, after `timer` seconds.
fn falls_due(since: Option<i64>, timer: f64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    since.map(|since| since as f64 + timer)
}
