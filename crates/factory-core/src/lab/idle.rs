//! What the daemon stops because nothing used it for long enough, as devlab's watcher did
//! (#18): a ready emulator no lease holds, after the emulator timer, and a stack devlab started
//! on a slot no lease holds, after the stack timer. A stack started outside devlab is never
//! stopped.

/// The idle timers, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timers {
    pub emulator_sec: f64,
    pub stack_sec: f64,
}

impl Timers {
    /// The timers from the machine config's minutes.
    pub fn from_minutes(emulator: f64, stack: f64) -> Timers {
        Timers {
            emulator_sec: emulator * 60.0,
            stack_sec: stack * 60.0,
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

/// What to stop now, and when the next thing falls due.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sweep {
    /// Emulators to stop, by index.
    pub emulators: Vec<u32>,
    /// Stacks to stop, by project and slot.
    pub stacks: Vec<(String, u32)>,
    /// When the soonest of the rest falls due, in seconds since the epoch; none when nothing
    /// idles.
    pub next: Option<f64>,
}

/// What is due at `now` (seconds since the epoch).
pub fn sweep(emulators: &[IdleEmulator], slots: &[IdleSlot], timers: Timers, now: f64) -> Sweep {
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
    due
}

impl Sweep {
    fn later(&mut self, at: f64) {
        self.next = Some(self.next.map_or(at, |next| next.min(at)));
    }
}

/// When something idle since `since` falls due, after `timer` seconds.
fn falls_due(since: Option<i64>, timer: f64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    since.map(|since| since as f64 + timer)
}
