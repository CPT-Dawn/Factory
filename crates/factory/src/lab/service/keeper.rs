//! What devlab's watcher did, inside the daemon (#18): an owner's leases are freed the moment
//! the owner exits, watched through a pidfd rather than polled; a ready emulator no lease holds
//! is stopped after `emulatorIdleMin`, and a stack devlab owns on a slot no lease holds after
//! `stackIdleMin`. A stack is devlab's once a lease starts it or finds it running, with or
//! without `--stack`, or once a new daemon finds it running on a slot nothing holds (#84); one
//! no lease or daemon has seen run is never stopped. A device whose lease ran
//! nothing for `deviceIdleMin` is taken from it while a waiter needs that kind of device (#57).
//! The machine config is read again on each look, so a changed timer counts from the next one.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use factory_adapters::adb::{Adb, AdbCli};
use factory_adapters::emulator::{Emulators, SystemdEmulators};
use factory_core::lab::devices::{Attached, Mode, RESTORE_POWER};
use factory_core::lab::emulator::{State, unit_name};
use factory_core::lab::events as event;
use factory_core::lab::idle::{self, IdleDevice, IdleEmulator, IdleSlot, Need, Timers};
use factory_core::lab::js_number;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use super::super::{activity, context, events, host, lease, stack};
use super::{Lab, Lease, grantable, lock_ignoring_poison, now_ms, reservations, store};
use crate::lab::wire::SlotRow;

/// The longest the keeper goes without a look: a changed config is read by then.
const LOOK: Duration = Duration::from_secs(60);

/// What the keeper has claimed to stop, so no grant hands it out until it is stopped.
#[derive(Debug, Default, Clone)]
pub struct Stopping {
    /// Emulators, by index.
    pub emulators: BTreeSet<u32>,
    /// Slots whose stack is stopping, by project and slot.
    pub slots: BTreeSet<(String, u32)>,
}

impl Lab {
    /// Starts the keeper: a watch on the owner of each lease in the store, and the idle loop.
    pub fn keep(self: &Arc<Self>) {
        if let Ok(leases) = self.write(|c, _| store::leases(c)) {
            for lease in &leases {
                self.watch_owner(lease);
            }
        }
        let lab = Arc::clone(self);
        on_thread("lab-adopt", move || lab.adopt_stacks());
        tokio::spawn(idle_loop(Arc::clone(self)));
    }

    /// Makes devlab's each stack that runs on a slot it knows while nothing holds the slot: one
    /// an earlier daemon left, or that docker started again after a reboot. The idle timer then
    /// stops it like any other (#84).
    fn adopt_stacks(self: &Arc<Self>) {
        let Ok(unheld) = self.write(|c, _| {
            let leases = store::leases(c)?;
            let mut holdings: Vec<_> = leases.iter().map(Lease::holding).collect();
            holdings.extend(reservations::all(c)?.iter().map(|r| r.holding()));
            Ok(store::slots(c)?
                .into_iter()
                .filter(|s| !s.stack_ours && !s.stopping)
                .filter(|s| {
                    !holdings.iter().any(|h| {
                        h.project.as_deref() == Some(s.project.as_str()) && h.slot == Some(s.slot)
                    })
                })
                .collect::<Vec<SlotRow>>())
        }) else {
            return;
        };
        // Docker is asked outside the store's transaction.
        let running: Vec<(SlotRow, String)> = unheld
            .into_iter()
            .filter_map(|s| {
                let id = stack::slot_stack_id(&s);
                stack::running(&id).then_some((s, id))
            })
            .collect();
        if running.is_empty() {
            return;
        }
        let now = now_ms() / 1000;
        for (row, id) in &running {
            stack::keep_down_after_reboot(id, &mut |warning| eprintln!("factory: lab: {warning}"));
            let adopted = self.write(|c, _| store::adopt_stack(c, &row.project, row.slot, id, now));
            if adopted.is_ok_and(|took| took) {
                eprintln!(
                    "factory: lab: the stack of {} slot {} runs with no lease: devlab's now, stopped once idle",
                    row.project, row.slot
                );
            }
        }
        self.notify();
    }

    /// Watches the owner of `lease`, once per owner: when it exits, its leases are dropped.
    pub(super) fn watch_owner(self: &Arc<Self>, lease: &Lease) {
        let key = lease.owner_key();
        if !lock_ignoring_poison(&self.watched).insert(key.clone()) {
            return;
        }
        let lab = Arc::clone(self);
        let (pid, start) = (lease.owner_pid, lease.owner_start.clone());
        on_thread("lab-owner", move || {
            let exited = owner_exit(pid, &start);
            lock_ignoring_poison(&lab.watched).remove(&key);
            if exited {
                let _ = lab.write(|_, _| Ok(()));
            }
        });
    }

    /// One look: claims and starts stopping what idled long enough, and says when the next thing
    /// falls due.
    fn look(self: &Arc<Self>) -> Result<Option<f64>, String> {
        let cfg = context::machine_config().map_err(|f| f.message)?;
        let timers = Timers::from_minutes(
            cfg.emulator_idle_min,
            cfg.stack_idle_min,
            cfg.device_idle_min,
        );
        let now = now_f64();
        let claimed = self
            .write(|c, _| {
                // The list adb shows as this transaction runs, as a grant plans from: none while
                // its stream restarts, when nothing is taken from anyone.
                let (attached, since) = {
                    let list = self.attached.borrow();
                    (list.live(), list.since.clone())
                };
                // Only a phone a grant could take is of use to a waiter: one locked, asleep or
                // otherwise not ready is neither free for it nor worth taking from a lease.
                let probes = self.probes(c)?;
                let probed_by = std::time::Instant::now();
                let grantable = |a: &Attached| grantable(&probes, &since, a, probed_by);
                let taken = store::taken(c)?;
                // A slot a lease or a reservation holds, or a release is stopping, isn't idle.
                let leases = store::leases(c)?;
                let mut holdings: Vec<_> = leases.iter().map(Lease::holding).collect();
                holdings.extend(reservations::all(c)?.iter().map(|r| r.holding()));
                let mut stopping = lock_ignoring_poison(&self.stopping);
                let emulators: Vec<store::EmulatorRow> = store::emulators(c)?
                    .into_iter()
                    .filter(|e| !stopping.emulators.contains(&e.idx))
                    .collect();
                let slots: Vec<SlotRow> = store::slots(c)?
                    .into_iter()
                    .filter(|s| !stopping.slots.contains(&(s.project.clone(), s.slot)))
                    .collect();
                let emulator_views: Vec<IdleEmulator> = emulators
                    .iter()
                    .map(|e| IdleEmulator {
                        idx: e.idx,
                        ready: State::from_word(&e.state) == State::Ready,
                        held: taken.contains(&e.serial),
                        idle_since: e.idle_since,
                    })
                    .collect();
                let slot_views: Vec<IdleSlot> = slots
                    .iter()
                    .map(|s| IdleSlot {
                        project: s.project.clone(),
                        slot: s.slot,
                        stack_ours: s.stack_ours,
                        held: s.stopping
                            || holdings.iter().any(|h| {
                                h.project.as_deref() == Some(s.project.as_str())
                                    && h.slot == Some(s.slot)
                            }),
                        idle_since: s.idle_since,
                    })
                    .collect();
                let device_views = device_views(c, &leases, attached.as_deref(), &grantable)?;
                let needs: Vec<Need> = activity::needs(c)?
                    .into_iter()
                    .map(|(mode, want)| Need {
                        mode: Mode::from_word(&mode),
                        want,
                    })
                    .collect();
                // What nobody holds goes to the waiters before anything is taken from a lease.
                let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                let free_phones = count(
                    attached
                        .iter()
                        .flatten()
                        .filter(|a| grantable(a) && !taken.contains(&a.serial))
                        .count(),
                );
                let online = |serial: &str| {
                    attached
                        .iter()
                        .flatten()
                        .any(|a| a.serial == serial && a.state == "device")
                };
                let free_emulators = count(
                    emulators
                        .iter()
                        .filter(|e| {
                            State::from_word(&e.state) == State::Ready
                                && !taken.contains(&e.serial)
                                && online(&e.serial)
                        })
                        .count(),
                );
                let needs = idle::unmet(&needs, free_phones, free_emulators);
                let due = idle::sweep(
                    &emulator_views,
                    &slot_views,
                    &device_views,
                    &needs,
                    timers,
                    now,
                );
                let reclaimed = reclaim(c, &leases, &due.devices, now)?;
                let emulators: Vec<store::EmulatorRow> = emulators
                    .into_iter()
                    .filter(|e| due.emulators.contains(&e.idx))
                    .collect();
                let slots: Vec<SlotRow> = slots
                    .into_iter()
                    .filter(|s| due.stacks.contains(&(s.project.clone(), s.slot)))
                    .collect();
                stopping.emulators.extend(emulators.iter().map(|e| e.idx));
                stopping
                    .slots
                    .extend(slots.iter().map(|s| (s.project.clone(), s.slot)));
                Ok((emulators, slots, reclaimed, due.next))
            })
            .map_err(|e| e.message)?;
        let (emulators, slots, reclaimed, next) = claimed;
        if !reclaimed.is_empty() {
            self.notify();
            take_back_reverses_on_thread(
                self.attached.borrow().shown.clone(),
                reclaimed,
                Power::Leave,
            );
        }
        for row in emulators {
            let lab = Arc::clone(self);
            let minutes = js_number(cfg.emulator_idle_min);
            on_thread("lab-idle-emulator", move || {
                lab.stop_emulator(&row, &minutes)
            });
        }
        for row in slots {
            let lab = Arc::clone(self);
            let minutes = js_number(cfg.stack_idle_min);
            on_thread("lab-idle-stack", move || lab.stop_stack(&row, &minutes));
        }
        Ok(next)
    }

    /// Stops a claimed emulator and forgets it.
    fn stop_emulator(&self, row: &store::EmulatorRow, minutes: &str) {
        SystemdEmulators.stop(&unit_name(row.idx), &row.serial, &AdbCli);
        let _ = self.write(|c, _| store::delete_emulator(c, row.idx, "ready"));
        lock_ignoring_poison(&self.stopping)
            .emulators
            .remove(&row.idx);
        self.notify();
        eprintln!(
            "factory: lab: stopped emulator {}, idle for {minutes} min",
            row.serial
        );
    }

    /// Stops a claimed slot's stack by its project id, and stops counting it as devlab's once
    /// that worked: one that failed to stop is still devlab's, and the next look tries again
    /// (#84).
    fn stop_stack(&self, row: &SlotRow, minutes: &str) {
        let stopped = stack::down_slot(row).map_err(|f| f.message);
        let _ = self.write(|c, _| {
            if stopped.is_ok() {
                store::disown_stack(c, &row.project, row.slot)
            } else {
                // Tried again after another idle span, not at once on every look.
                store::idle_again(c, &row.project, row.slot, now_ms() / 1000)
            }
        });
        lock_ignoring_poison(&self.stopping)
            .slots
            .remove(&(row.project.clone(), row.slot));
        self.notify();
        match stopped {
            Ok(()) => eprintln!(
                "factory: lab: stopped the stack of {} slot {}, idle for {minutes} min",
                row.project, row.slot
            ),
            Err(why) => eprintln!(
                "factory: lab: cannot stop the idle stack of {} slot {}: {why}",
                row.project, row.slot
            ),
        }
    }
}

/// Each device a lease holds that adb lists as usable, as the sweep sees it: busy while its
/// lease runs a command or waits in the queue. One unplugged or offline is no use to a waiter,
/// and its lease is told so, not that it was reclaimed; nor is a phone a grant could not take
/// (`grantable`: locked, asleep, ...), which stays with its lease. None is, before adb's first
/// list.
fn device_views(
    c: &rusqlite::Connection,
    leases: &[Lease],
    attached: Option<&[Attached]>,
    grantable: &dyn Fn(&Attached) -> bool,
) -> rusqlite::Result<Vec<IdleDevice>> {
    let usable = |serial: &str| {
        attached.into_iter().flatten().any(|a| {
            a.serial == serial && a.state == "device" && (a.kind != "phone" || grantable(a))
        })
    };
    let running = activity::running(c, &host::alive)?;
    let waiters = store::waiters(c)?;
    Ok(leases
        .iter()
        .flat_map(|l| {
            let busy = running.iter().any(|(lease, _)| *lease == l.id)
                || waiters.iter().any(|w| {
                    w.lease == Some(l.id) || (w.owner == l.owner_key() && w.worktree == l.worktree)
                });
            l.devices
                .iter()
                .filter(|d| usable(&d.serial))
                .map(move |d| IdleDevice {
                    lease: l.id,
                    serial: d.serial.clone(),
                    phone: d.kind == "phone",
                    busy,
                    touched: l.touched,
                })
        })
        .collect())
}

/// Takes each `(lease, serial)` from its lease for the waiters; the leases with just the devices
/// taken from them, whose reverses go back.
fn reclaim(
    c: &rusqlite::Connection,
    leases: &[Lease],
    due: &[(i64, String)],
    now: f64,
) -> rusqlite::Result<Vec<Lease>> {
    #[allow(clippy::cast_possible_truncation)]
    let now = now as i64;
    let mut taken: Vec<Lease> = Vec::new();
    for (id, serial) in due {
        let Some(lease) = leases.iter().find(|l| l.id == *id) else {
            continue;
        };
        let idle_min = (now - lease.touched).max(0) / 60;
        let Some(kind) = activity::reclaim(c, *id, serial, idle_min)? else {
            continue;
        };
        if kind == "emulator" {
            store::emulator_idle(c, serial, now)?;
        }
        let model = lease
            .devices
            .iter()
            .find(|d| d.serial == *serial)
            .map_or("", |d| d.model.as_str());
        let fields = event::reclaim(*id, &kind, model, idle_min);
        events::queue(c, "lab-reclaim", &fields)?;
        eprintln!(
            "factory: lab: took {kind} {serial} from lease {id}, idle for {idle_min} min, for a waiter"
        );
        let devices = lease
            .devices
            .iter()
            .filter(|d| d.serial == *serial)
            .cloned();
        match taken.iter_mut().find(|l| l.id == *id) {
            Some(l) => l.devices.extend(devices),
            None => taken.push(Lease {
                devices: devices.collect(),
                ..lease.clone()
            }),
        }
    }
    Ok(taken)
}

/// Looks, then waits for the next thing to fall due, a change to the board, or [`LOOK`].
async fn idle_loop(lab: Arc<Lab>) {
    let mut said = String::new();
    loop {
        let mut changed = lab.changed.subscribe();
        let looker = Arc::clone(&lab);
        let looked = tokio::task::spawn_blocking(move || looker.look())
            .await
            .unwrap_or_else(|e| Err(format!("the idle look failed: {e}")));
        let next = match looked {
            Ok(next) => {
                said.clear();
                next
            }
            Err(why) => {
                if why != said {
                    eprintln!("factory: lab: idle emulators and stacks are left running: {why}");
                    said = why;
                }
                None
            }
        };
        let wait = next.map_or(LOOK, |at| {
            Duration::try_from_secs_f64(at - now_f64())
                .unwrap_or(Duration::ZERO)
                .min(LOOK)
        });
        tokio::select! {
            _ = changed.changed() => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Waits until the process (pid, start) exits, on a pidfd: true then, false when it can't be
/// watched, and the gc of every write is left to free its leases.
fn owner_exit(pid: i64, start: &str) -> bool {
    let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return true;
    };
    let fd = match pidfd_open(raw, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(e) => return e == Errno::SRCH,
    };
    // Opened after the lease was granted: by now the pid may name another process.
    if !host::alive(pid, start) {
        return true;
    }
    // A pidfd reads as ready once the process exits, before its parent reaps it.
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    loop {
        match poll(&mut fds, None) {
            Ok(_) => return true,
            Err(Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

/// What a device's power gets as a lease lets go of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    /// The lease ended: what a battery-measuring run of it faked is undone ([RESTORE_POWER]).
    Restore,
    /// The device was reclaimed for a waiter and may be granted again already: its power is
    /// left alone, as the next grant's prep resets the battery itself (#84).
    Leave,
}

/// Takes back the reverses dropped leases' devices were given, beside the caller.
pub fn take_back_reverses_on_thread(
    attached: Option<Vec<Attached>>,
    leases: Vec<Lease>,
    power: Power,
) {
    if leases.iter().all(|l| l.devices.is_empty()) {
        return;
    }
    on_thread("lab-unreverse", move || {
        take_back_reverses(attached.as_deref(), &leases, power);
    });
}

/// Takes back the reverses of each lease's project on its devices that adb lists as usable
/// (all of them before adb's first list), as devlab's release did, and with [`Power::Restore`]
/// undoes what a battery-measuring run faked on them (#84).
pub fn take_back_reverses(attached: Option<&[Attached]>, leases: &[Lease], power: Power) {
    for lease in leases.iter().filter(|l| !l.devices.is_empty()) {
        let ports: Vec<u32> = match context::project_config(&lease.worktree) {
            Ok(Some(p)) => {
                let env = lease::lease_env(lease, Some(&p)).unwrap_or_default();
                lease::reverse_ports(&p, &env)
                    .into_iter()
                    .map(|(on_device, _)| on_device)
                    .filter(|port| *port > 0)
                    .collect()
            }
            _ => Vec::new(),
        };
        for d in &lease.devices {
            let usable = attached.is_none_or(|list| {
                list.iter()
                    .any(|a| a.serial == d.serial && a.state == "device")
            });
            if usable {
                for port in &ports {
                    AdbCli.unreverse(&d.serial, *port);
                }
                if power == Power::Restore {
                    restore_power(&d.serial);
                }
            }
        }
    }
}

/// Puts the device's real battery back and stops forcing doze, as a lease that may have faked
/// them for a measurement ends: a phone left on a faked "unplugged" battery sleeps.
fn restore_power(serial: &str) {
    AdbCli.shell(serial, RESTORE_POWER);
}

/// Runs `work` on a thread of its own, named `name`.
fn on_thread(name: &str, work: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(work);
}

/// Seconds since the epoch, with their fraction.
fn now_f64() -> f64 {
    now_ms() as f64 / 1000.0
}
