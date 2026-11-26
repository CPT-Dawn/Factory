//! The lab as the bar shows it (#72): each phone's last readiness, the emulators, every stack
//! slot and the queue, from the lease store and the daemon's own checks. Like the overview it
//! runs no command: the phone watcher and the stack sweep do the asking, on their own time.

use std::collections::BTreeMap;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use factory_core::bar::lab::{Emulator, Lab as BarLab, Phone, Stack};
use factory_core::lab::base_name;
use factory_core::lab::emulator::State;
use factory_core::lab::ready::Readiness;

use super::super::wire::{SlotRow, slot_inventory};
use super::super::{context, host, readiness, stack};
use super::{Answer, Lab, lock_ignoring_poison, now_ms, reservations, store};

impl Lab {
    /// The lab for the bar: its error when the store can't be read.
    pub async fn bar(self: &Arc<Self>) -> BarLab {
        let lab = Arc::clone(self);
        let read = tokio::task::spawn_blocking(move || lab.bar_now())
            .await
            .unwrap_or_else(|e| Err(super::failed(&e)));
        read.unwrap_or_else(|failure| BarLab {
            now: now_ms(),
            error: Some(failure.message),
            ..BarLab::default()
        })
    }

    fn bar_now(&self) -> Answer<BarLab> {
        let cfg = context::machine_config().unwrap_or_default();
        let shown = self.attached.borrow().shown.clone();
        let stacks_seen = lock_ignoring_poison(&self.stacks).clone();
        let (swept, hidden) = self.hidden_phones(shown.as_deref().unwrap_or_default());
        self.read(|c| {
            let leases = store::leases(c)?;
            let holder = |serial: &str| {
                leases
                    .iter()
                    .find(|lease| lease.devices.iter().any(|d| d.serial == serial))
                    .map(|lease| base_name(&lease.worktree).to_owned())
            };
            let rows = readiness::all(c)?;
            // The phones adb shows, each with its last probe; the probed ones while no list
            // has come yet.
            let serials: Vec<(String, String)> = match &shown {
                Some(list) => list
                    .iter()
                    .filter(|a| a.kind == "phone")
                    .map(|a| (a.serial.clone(), a.model.clone()))
                    .collect(),
                None => rows
                    .iter()
                    .map(|r| (r.serial.clone(), r.model.clone()))
                    .collect(),
            };
            let mut phones: Vec<Phone> = serials
                .into_iter()
                .map(|(serial, model)| {
                    let row = rows.iter().find(|r| r.serial == serial);
                    Phone {
                        model: row
                            .map(|r| r.model.clone())
                            .filter(|m| !m.is_empty())
                            .unwrap_or(model),
                        readiness: row.map(|r| r.known.readiness),
                        probed: row.map(|r| r.probed_ms.0),
                        holder: holder(&serial),
                        serial,
                    }
                })
                .collect();
            // A phone adb doesn't list, but the USB bus shows: its USB debugging is off.
            phones.extend(hidden.into_iter().map(|h| Phone {
                model: h.vendor.to_owned(),
                readiness: Some(Readiness::UsbDebuggingOff),
                probed: Some(swept),
                // A leased phone is its holder's, and says so whatever adb shows of it.
                holder: holder(&h.serial),
                serial: h.serial,
            }));
            let emulators = store::emulators(c)?
                .into_iter()
                .map(|e| Emulator {
                    ready: State::from_word(&e.state) == State::Ready,
                    holder: holder(&e.serial),
                    serial: e.serial,
                })
                .collect();
            let held = reservations::all(c)?;
            // The slots the store knows, and those only the sweep found a stack on.
            let mut rows = slot_inventory(&store::slots(c)?, &held);
            for (project, slot) in stacks_seen.keys() {
                if !rows
                    .iter()
                    .any(|r| &r.project == project && r.slot == *slot)
                {
                    rows.push(unrecorded(project, *slot));
                }
            }
            rows.sort_by(|a, b| (&a.project, a.slot).cmp(&(&b.project, b.slot)));
            let stacks = rows
                .into_iter()
                .map(|row| {
                    let lease = leases.iter().find(|lease| {
                        lease.project.as_deref() == Some(row.project.as_str())
                            && lease.slot == Some(row.slot)
                    });
                    let reserved = held
                        .iter()
                        .find(|r| r.project == row.project && r.slot == row.slot);
                    Stack {
                        up: stacks_seen.get(&(row.project.clone(), row.slot)).copied(),
                        ours: row.stack_ours,
                        holder: lease
                            .map(|lease| base_name(&lease.worktree).to_owned())
                            .or_else(|| reserved.map(|r| format!("reserved by {}", r.owner_label))),
                        project: row.project,
                        slot: row.slot,
                    }
                })
                .collect();
            Ok(BarLab {
                now: now_ms(),
                phones,
                emulators,
                stacks,
                queue: store::waiters(c)?.len(),
                max_emulators: cfg.max_emulators,
                mem_available_mb: host::mem_available_mb(),
                error: None,
            })
        })
    }

    /// Starts the stack sweep: whether each slot's stack runs, kept for the bar, looked at again
    /// every `readinessCheckSec` and on each change to the lab, so a slot just leased is known
    /// at once. It asks docker on its own thread, so no bar call waits on it.
    pub fn sweep_stacks(self: &Arc<Self>) {
        let (wake, woken) = mpsc::channel();
        if self.sweep.set(wake).is_err() {
            return;
        }
        let lab = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("lab-stacks".to_owned())
            .spawn(move || {
                loop {
                    let cfg = context::machine_config().unwrap_or_default();
                    if let Ok(slots) = lab.read(every_slot) {
                        let seen: BTreeMap<(String, u32), bool> = slots
                            .into_iter()
                            .map(|row| {
                                let id = stack::slot_stack_id(
                                    &row.project,
                                    row.slot,
                                    row.last_worktree.as_deref(),
                                );
                                ((row.project, row.slot), stack::running(&id))
                            })
                            .collect();
                        *lock_ignoring_poison(&lab.stacks) = seen;
                    }
                    let every = Duration::try_from_secs_f64(cfg.readiness_check_sec)
                        .unwrap_or(Duration::from_secs(120));
                    match woken.recv_timeout(every) {
                        // One look answers every change queued before it.
                        Ok(()) => while woken.try_recv().is_ok() {},
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            });
    }
}

/// Every slot the sweep looks at: each one the store knows or a reservation holds, and every
/// other slot of each project the lab has seen, by the `devlab.json` of a worktree that held or
/// reserved one of its slots: a stack the owner started on a slot no lease took is theirs.
/// Read while the store is: the configs are read after.
fn every_slot(c: &rusqlite::Connection) -> rusqlite::Result<Vec<SlotRow>> {
    let held = reservations::all(c)?;
    let mut rows = slot_inventory(&store::slots(c)?, &held);
    let mut worktrees: BTreeMap<String, String> = BTreeMap::new();
    let seen = rows
        .iter()
        .filter_map(|r| Some((r.project.clone(), r.last_worktree.clone()?)))
        .chain(
            store::leases(c)?
                .into_iter()
                .filter_map(|l| Some((l.project?, l.worktree))),
        )
        .chain(held.into_iter().map(|r| (r.project, r.worktree)));
    for (project, worktree) in seen {
        worktrees.entry(project).or_insert(worktree);
    }
    for (project, worktree) in worktrees {
        let Some(p) = context::project_config(&worktree).ok().flatten() else {
            continue;
        };
        for slot in 0..p.max_slots {
            if !rows.iter().any(|r| r.project == project && r.slot == slot) {
                rows.push(unrecorded(&project, slot));
            }
        }
    }
    rows.sort_by(|a, b| (&a.project, a.slot).cmp(&(&b.project, b.slot)));
    Ok(rows)
}

/// Slot `slot` of `project`, which no lease has held.
fn unrecorded(project: &str, slot: u32) -> SlotRow {
    SlotRow {
        project: project.to_owned(),
        slot,
        last_worktree: None,
        stack_ours: false,
        idle_since: None,
        stopping: false,
    }
}
