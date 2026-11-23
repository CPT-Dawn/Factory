//! The lab inside the daemon: the lease store, and acquire's wait in the queue.
//!
//! An acquire gathers what it needs from outside (listening ports, whether each slot's stack
//! runs) first, then decides and writes in one short transaction. It waits on a change to the
//! board (a grant, a release, a waiter leaving) or the re-check tick, never in a sleep loop of
//! the CLI's.
//!
//! The keeper ([`keeper`]) does what devlab's watcher did: it frees an owner's leases the moment
//! the owner exits, and stops idle emulators and the idle stacks devlab started.

mod keeper;

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use factory_adapters::adb::{Adb, AdbCli, AdbTrack};
use factory_adapters::docker::{Containers, Docker};
use factory_adapters::emulator::{EmulatorSpec, Emulators, SystemdEmulators};
use factory_core::event::{Event, Fields};
use factory_core::exit::Exit;
use factory_core::lab::devices::{
    Attached, EmulatorView, HeldBy, Mode, Plan, PlanInput, device_rows, plan_devices,
};
use factory_core::lab::emulator::{
    Boot, Settle, State, after_restart, boot_step, booting_note, free_indexes, serial_of, unit_name,
};
use factory_core::lab::grant::{self, Ask, Decision};
use factory_core::lab::js_number;
use factory_core::lab::run::{End, Phase};
use factory_core::lab::slots::{SlotView, slot_host_ports, slot_project_id};
use rusqlite::Connection;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, Lines};
use tokio::sync::watch;
use tokio::time::Instant;

use super::lease::base_name;
use super::store::{self, Store};
use super::wire::{
    AcquireParams, Board, DeviceAsk, DeviceLine, FindParams, GC, Granted, LEASE, Lease,
    LeaseDevice, RELEASE, RUN_END, RUN_START, RunAsk, RunEnd, SLOT, STATUS, SlotParams, Waiter,
};
use super::{android, host};
use crate::ledger::Ledger;
use crate::protocol::{ErrorBody, Outcome, Request, Response};

/// How often a waiting acquire looks again without a change to the board: a port that freed
/// tells no one. An owner that exits does: the keeper watches each one.
const RECHECK: Duration = Duration::from_secs(3);
/// How long a call that needs the device list waits for adb's first one.
const FIRST_LIST: Duration = Duration::from_secs(15);
/// How often a booting emulator is looked at without a change to the device list.
const BOOT_LOOK: Duration = Duration::from_secs(2);

pub struct Lab {
    path: PathBuf,
    store: Mutex<Store>,
    /// Bumped on every change a waiter could be waiting for.
    changed: watch::Sender<u64>,
    /// What `adb track-devices` showed last; none until its first list.
    attached: watch::Sender<Option<Vec<Attached>>>,
    /// Starts the one device tracker, on the first call that needs the device list.
    tracking: Once,
    /// The owners the keeper watches, by [`Lease::owner_key`].
    watched: Mutex<HashSet<String>>,
    /// What the keeper is stopping because it idled; no grant hands it out meanwhile.
    stopping: Mutex<keeper::Stopping>,
    /// A line for each lease dropped because its owner exited, until `devlab gc` tells it: the
    /// keeper usually drops it first. The newest [`UNREPORTED`].
    unreported: Mutex<VecDeque<String>>,
}

/// How many dropped leases wait for `devlab gc` to tell them.
const UNREPORTED: usize = 64;

type Answer<T> = Result<T, ErrorBody>;

impl Lab {
    pub fn open(path: &Path) -> Result<Lab, String> {
        let store =
            Store::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Ok(Lab {
            path: path.to_owned(),
            store: Mutex::new(store),
            changed: watch::Sender::new(0),
            attached: watch::Sender::new(None),
            tracking: Once::new(),
            watched: Mutex::new(HashSet::new()),
            stopping: Mutex::new(keeper::Stopping::default()),
            unreported: Mutex::new(VecDeque::new()),
        })
    }

    /// Settles the emulators an earlier daemon left booting: their booter died with it, so
    /// they are stopped and forgotten, never marked ready. Runs beside the daemon.
    pub fn recover(self: &Arc<Self>) {
        let Ok(rows) = self.write(|c, _| store::emulators(c)) else {
            return;
        };
        let orphans: Vec<store::EmulatorRow> = rows
            .into_iter()
            .filter(|r| after_restart(State::from_word(&r.state)) == Settle::StopAndForget)
            .collect();
        if orphans.is_empty() {
            return;
        }
        let lab = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("lab-recover".to_owned())
            .spawn(move || {
                for row in orphans {
                    SystemdEmulators.stop(&unit_name(row.idx), &row.serial, &AdbCli);
                    let _ = lab.write(|c, _| store::delete_emulator(c, row.idx, "booting"));
                }
                lab.notify();
            });
    }

    /// The device list, once adb has shown one; empty when adb shows none in [`FIRST_LIST`].
    async fn attached(self: &Arc<Self>) -> Vec<Attached> {
        self.track();
        let mut seen = self.attached.subscribe();
        let _ = tokio::time::timeout(FIRST_LIST, seen.wait_for(Option::is_some)).await;
        self.attached.borrow().clone().unwrap_or_default()
    }

    /// Starts following `adb track-devices`, once: each list it shows is told to waiters.
    fn track(self: &Arc<Self>) {
        self.tracking.call_once(|| {
            let lab = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("adb-track".to_owned())
                .spawn(move || {
                    android::track_forever(
                        &AdbTrack,
                        &|list| {
                            lab.attached.send_replace(Some(list));
                            lab.notify();
                        },
                        &|| {
                            lab.attached.send_if_modified(|list| {
                                let unknown = list.is_none();
                                if unknown {
                                    *list = Some(Vec::new());
                                }
                                unknown
                            });
                        },
                    )
                });
            if spawned.is_err() {
                self.attached.send_replace(Some(Vec::new()));
            }
        });
    }

    /// One write transaction; dead owners' leases are dropped first, telling waiters and
    /// `devlab gc`. The reverses their devices were given are taken back beside it, after the
    /// transaction.
    fn write<T>(&self, work: impl FnOnce(&Connection, i64) -> rusqlite::Result<T>) -> Answer<T> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now();
        let (freed, out) = store
            .write(|c| {
                let freed = store::gc(c, now, &host::alive)?;
                Ok((freed, work(c, now)?))
            })
            .map_err(|e| ErrorBody {
                code: Exit::Precondition.code(),
                message: format!("lab store {}: {e}; retry", self.path.display()),
            })?;
        drop(store);
        if !freed.is_empty() {
            let mut unreported = keeper::held(&self.unreported);
            for lease in &freed {
                if unreported.len() == UNREPORTED {
                    unreported.pop_front();
                }
                unreported.push_back(format!(
                    "lease {} ({}, owner exited)",
                    lease.id, lease.owner_label
                ));
            }
            drop(unreported);
            self.notify();
            keeper::take_back_reverses_beside(self.attached.borrow().clone(), freed);
        }
        Ok(out)
    }

    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn drop_unanswered(&self, lease: &Lease) {
        if self
            .write(|c, now| store::drop_lease(c, lease, now))
            .is_ok()
        {
            self.notify();
        }
    }

    fn remove_waiter(&self, id: i64) {
        if let Ok(true) = self.write(|c, _| store::delete_waiter(c, id)) {
            self.notify();
        }
    }
}

/// Answers a lab method that doesn't wait; none for a method that isn't the lab's.
pub async fn respond(lab: &Arc<Lab>, method: &str, params: Value) -> Option<Outcome> {
    let work: fn(&Lab, Value, &[Attached]) -> Answer<Value> = match method {
        LEASE => |lab, p, seen| reply(lab_lease(lab, parse(p)?, seen)),
        RELEASE => |lab, p, _| reply(release(lab, parse(p)?)),
        GC => |lab, _, _| {
            reply(
                lab.write(|_, _| Ok(()))
                    .map(|()| keeper::held(&lab.unreported).drain(..).collect::<Vec<_>>()),
            )
        },
        STATUS => |lab, _, seen| reply(status(lab, seen)),
        SLOT => |lab, p, _| reply(slot(lab, parse(p)?)),
        _ => return None,
    };
    // A lease's devices and the status table are told as adb lists them now.
    let seen = if method == LEASE || method == STATUS {
        lab.attached().await
    } else {
        Vec::new()
    };
    let lab = Arc::clone(lab);
    let answer = tokio::task::spawn_blocking(move || work(&lab, params, &seen))
        .await
        .unwrap_or_else(|e| Err(failed(&e)));
    Some(match answer {
        Ok(value) => Outcome::Result(value),
        Err(body) => Outcome::Error(body),
    })
}

fn parse<T: DeserializeOwned>(params: Value) -> Answer<T> {
    serde_json::from_value(params).map_err(|e| ErrorBody {
        code: Exit::Usage.code(),
        message: format!(
            "bad lab params: {e}; the daemon may be older than this factory: restart it"
        ),
    })
}

fn reply<T: Serialize>(answer: Answer<T>) -> Answer<Value> {
    answer.map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
}

fn failed(e: &dyn std::fmt::Display) -> ErrorBody {
    ErrorBody {
        code: Exit::Precondition.code(),
        message: format!("the lab failed in the daemon: {e}; retry"),
    }
}

fn lab_lease(lab: &Lab, p: FindParams, seen: &[Attached]) -> Answer<Option<Lease>> {
    let lease = lab.write(|c, now| {
        let lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        if let Some(lease) = &lease {
            store::touch(c, lease.id, now)?;
        }
        Ok(lease)
    })?;
    Ok(lease.map(|lease| with_states(lease, seen)))
}

/// The lease with what adb says of each of its devices.
fn with_states(mut lease: Lease, seen: &[Attached]) -> Lease {
    for d in &mut lease.devices {
        d.state = state_of(seen, &d.serial).map(str::to_owned);
    }
    lease
}

fn state_of<'a>(seen: &'a [Attached], serial: &str) -> Option<&'a str> {
    seen.iter()
        .find(|a| a.serial == serial)
        .map(|a| a.state.as_str())
}

fn release(lab: &Lab, p: FindParams) -> Answer<Option<Lease>> {
    let dropped = lab.write(|c, now| {
        let lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        if let Some(lease) = &lease {
            store::drop_lease(c, lease, now)?;
        }
        Ok(lease)
    })?;
    if let Some(lease) = &dropped {
        lab.notify();
        keeper::take_back_reverses(
            lab.attached.borrow().clone().as_deref(),
            std::slice::from_ref(lease),
        );
    }
    Ok(dropped)
}

fn status(lab: &Lab, seen: &[Attached]) -> Answer<Board> {
    lab.write(|c, now| {
        let leases = store::leases(c)?;
        let held: Vec<HeldBy> = leases
            .iter()
            .flat_map(|l| {
                l.devices.iter().map(|d| HeldBy {
                    serial: d.serial.clone(),
                    kind: d.kind.clone(),
                    model: d.model.clone(),
                    holder: format!(
                        "lease {} {} ({})",
                        l.id,
                        base_name(&l.worktree),
                        l.owner_label
                    ),
                })
            })
            .collect();
        let emulators: Vec<EmulatorView> = store::emulators(c)?
            .into_iter()
            .map(|e| EmulatorView {
                booting: State::from_word(&e.state) == State::Booting,
                serial: e.serial,
                idle_since: e.idle_since,
            })
            .collect();
        let devices = device_rows(seen, &held, &emulators, now)
            .into_iter()
            .map(|r| DeviceLine {
                serial: r.serial,
                kind: r.kind,
                model: r.model,
                state: r.state,
                holder: r.holder,
            })
            .collect();
        Ok(Board {
            now,
            leases,
            slots: store::slots(c)?,
            waiters: store::waiters(c)?,
            devices,
        })
    })
}

fn slot(lab: &Lab, p: SlotParams) -> Answer<()> {
    lab.write(|c, _| store::set_stack_ours(c, &p.project, p.slot, p.stack_ours))
}

/// What a grant needs from outside the store, gathered before its transaction.
struct Facts {
    listening: BTreeSet<u32>,
    /// Whether each slot's stack runs, by slot.
    running: Vec<bool>,
    /// What adb lists; empty when the request asks for no device.
    attached: Vec<Attached>,
    mem_available_mb: i64,
}

fn gather(lab: &Lab, p: &AcquireParams, attached: Vec<Attached>) -> Facts {
    let mem_available_mb = if p.devices.is_some() {
        forget_dead_emulators(lab, &attached);
        host::mem_available_mb()
    } else {
        0
    };
    let running = match &p.project {
        Some(project) => match &project.stack {
            Some(stack) => (0..project.max_slots)
                .map(|slot| {
                    Docker.running(&format!(
                        "supabase_db_{}",
                        slot_project_id(&stack.project_id, slot)
                    ))
                })
                .collect(),
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    Facts {
        listening: host::listening_ports(),
        running,
        attached,
        mem_available_mb,
    }
}

/// Forgets the ready emulators whose unit ended: adb no longer lists them, and they hold a
/// place under the emulator cap they no longer use. A lease that held one finds it unplugged.
fn forget_dead_emulators(lab: &Lab, attached: &[Attached]) {
    let Ok(rows) = lab.write(|c, _| store::emulators(c)) else {
        return;
    };
    for row in rows {
        if State::from_word(&row.state) == State::Ready
            && state_of(attached, &row.serial) != Some("device")
            && !SystemdEmulators.unit_active(&unit_name(row.idx))
        {
            let _ = lab.write(|c, _| store::delete_emulator(c, row.idx, "ready"));
        }
    }
}

/// A grant, with the device work it leaves for after its transaction.
struct Taken {
    granted: Granted,
    /// The lease is new to this request.
    fresh: bool,
    /// The emulators to boot, by index; each is in the store as booting.
    boots: Vec<u32>,
    /// The phones just granted, whose ABI is still to be read.
    phones: Vec<String>,
    /// The idle emulators just granted.
    reused: Vec<String>,
}

/// What the request's device ask plans, or why it waits: the held devices adb no longer lists
/// as `device` are dropped from the lease first, and only the rest are planned (a top-up).
fn plan(
    c: &Connection,
    ask: &DeviceAsk,
    lease: Option<&Lease>,
    seen: &[Attached],
    mem_available_mb: i64,
    stopping: &keeper::Stopping,
) -> rusqlite::Result<Result<Option<Planned>, String>> {
    let mut have = 0u32;
    for held in lease.map_or(&[][..], |l| l.devices.as_slice()) {
        if state_of(seen, &held.serial) == Some("device") {
            have += 1;
        } else {
            store::delete_lease_device(c, &held.serial)?;
        }
    }
    let want = ask.want.saturating_sub(have);
    if want == 0 {
        return Ok(Ok(None));
    }
    let taken = store::taken(c)?;
    let is_taken = |serial: &str| taken.iter().any(|t| t == serial);
    let emus = store::emulators(c)?;
    let phones = |state: &'static str| {
        seen.iter()
            .filter(move |a| a.kind == "phone" && a.state == state)
    };
    let mut free_phones: Vec<String> = phones("device")
        .filter(|a| !is_taken(&a.serial))
        .map(|a| a.serial.clone())
        .collect();
    free_phones.sort();
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let input = PlanInput {
        want,
        mode: Mode::from_word(&ask.mode),
        free_phones,
        idle_emulators: emus
            .iter()
            .filter(|e| {
                State::from_word(&e.state) == State::Ready
                    && !is_taken(&e.serial)
                    && !stopping.emulators.contains(&e.idx)
                    && state_of(seen, &e.serial) == Some("device")
            })
            .map(|e| e.serial.clone())
            .collect(),
        emulators_running: count(emus.len()),
        emulators_booting: count(
            emus.iter()
                .filter(|e| State::from_word(&e.state) == State::Booting)
                .count(),
        ),
        max_emulators: ask.max_emulators,
        mem_available_mb,
        host_floor_mb: ask.host_floor_mb,
        emulator_ram_mb: ask.emulator_ram_mb,
        unauthorized: count(phones("unauthorized").count()),
        busy_phones: count(phones("device").filter(|a| is_taken(&a.serial)).count()),
    };
    Ok(plan_devices(&input).map(|plan| {
        Some(Planned {
            plan,
            used: emus.iter().map(|e| e.idx).collect(),
        })
    }))
}

/// A device plan, and the emulator indexes in use when it was made.
struct Planned {
    plan: Plan,
    used: Vec<u32>,
}

fn enqueue(lab: &Lab, p: &AcquireParams) -> Answer<i64> {
    lab.write(|c, now| {
        let existing = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        let ask = project_ask(p);
        let name = base_name(&p.worktree);
        store::insert_waiter(
            c,
            &Waiter {
                id: 0,
                created: now,
                label: format!("{} @ {name}", p.owner.label),
                owner: p.owner.key(),
                owner_label: p.owner.label.clone(),
                project: p.project.as_ref().map(|project| project.name.clone()),
                slot_cap: grant::request_cap(ask.as_ref(), existing.is_some()),
                max_slots: p.project.as_ref().map_or(0, |project| project.max_slots),
            },
        )
    })
}

fn project_ask(p: &AcquireParams) -> Option<grant::ProjectAsk> {
    p.project.as_ref().map(|project| grant::ProjectAsk {
        name: project.name.clone(),
        max_slots: project.max_slots,
        cap: project.cap,
        has_stack: project.stack.is_some(),
    })
}

/// The lease granted, or why the request waits.
fn try_grant(
    lab: &Lab,
    p: &AcquireParams,
    waiter: i64,
    facts: &Facts,
) -> Answer<Result<Taken, String>> {
    lab.write(|c, now| {
        let lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        let stopping = keeper::held(&lab.stopping).clone();
        let planned = match &p.devices {
            Some(ask) => plan(
                c,
                ask,
                lease.as_ref(),
                &facts.attached,
                facts.mem_available_mb,
                &stopping,
            )?,
            None => Ok(None),
        };
        let (planned, devices_wait) = match planned {
            Ok(planned) => (planned, None),
            Err(why) => (None, Some(why)),
        };
        let leases = store::leases(c)?;
        let rows = store::slots(c)?;
        let board = grant::Board {
            leases: leases.iter().map(Lease::holding).collect(),
            queue: store::waiters(c)?.iter().map(Waiter::entry).collect(),
            slots: p.project.as_ref().map_or_else(Vec::new, |project| {
                (0..project.max_slots)
                    .map(|slot| {
                        let (own_ports, stack_ports) = slot_host_ports(
                            &project.ports,
                            project.stride,
                            slot,
                            project.stack.as_ref().map(|s| s.ports.as_slice()),
                        );
                        SlotView {
                            held: false,
                            last_is_mine: rows.iter().any(|r| {
                                r.project == project.name
                                    && r.slot == slot
                                    && r.last_worktree.as_deref() == Some(p.worktree.as_str())
                            }),
                            stack_running: facts
                                .running
                                .get(slot as usize)
                                .copied()
                                .unwrap_or(false),
                            own_ports,
                            stack_ports,
                            stopping: stopping.slots.contains(&(project.name.clone(), slot)),
                        }
                    })
                    .collect()
            }),
            listening: facts.listening.clone(),
        };
        let ask = Ask {
            waiter,
            project: project_ask(p),
            existing: lease.as_ref().map(|l| l.slot),
            devices_wait,
        };
        let (slot, new) = match grant::decide(&ask, &board, now) {
            Decision::Wait(why) => return Ok(Err(why)),
            Decision::Grant { slot, new } => (slot, new),
        };
        let fresh = new || lease.is_none();
        let lease = match lease {
            Some(lease) if !new => lease,
            _ => store::insert_lease(
                c,
                p.project.as_ref().map(|project| project.name.as_str()),
                &p.worktree,
                &p.owner,
                slot,
                now,
            )?,
        };
        let previous = match (&p.project, lease.slot) {
            (Some(project), Some(slot)) => store::claim_slot(c, &project.name, slot, &p.worktree)?,
            _ => None,
        };
        let (mut boots, mut phones, mut reused) = (Vec::new(), Vec::new(), Vec::new());
        if let (Some(Planned { plan, used }), Some(ask)) = (planned, &p.devices) {
            for serial in &plan.phones {
                let model = facts
                    .attached
                    .iter()
                    .find(|a| a.serial == *serial)
                    .map_or_else(String::new, |a| a.model.clone());
                store::insert_lease_device(
                    c,
                    lease.id,
                    &LeaseDevice {
                        serial: serial.clone(),
                        kind: "phone".to_owned(),
                        abi: None,
                        model,
                        state: None,
                    },
                )?;
                phones.push(serial.clone());
            }
            for serial in &plan.reuse {
                store::insert_lease_device(c, lease.id, &emulator_device(serial))?;
                store::emulator_held(c, serial)?;
                reused.push(serial.clone());
            }
            for idx in free_indexes(&used, plan.boot) {
                store::insert_emulator(c, idx, &serial_of(idx, ask.port_base))?;
                boots.push(idx);
            }
        }
        store::touch(c, lease.id, now)?;
        store::delete_waiter(c, waiter)?;
        let lease = store::lease(c, lease.id)?.unwrap_or(lease);
        Ok(Ok(Taken {
            granted: Granted { lease, previous },
            fresh,
            boots,
            phones,
            reused,
        }))
    })
}

fn emulator_device(serial: &str) -> LeaseDevice {
    LeaseDevice {
        serial: serial.to_owned(),
        kind: "emulator".to_owned(),
        abi: Some("x86_64".to_owned()),
        model: "emulator".to_owned(),
        state: None,
    }
}

/// The device work after a grant, none of it in a transaction: the new phones' ABIs, the
/// emulators to boot, then each device readied. Notes go to the caller as it goes. An emulator
/// that won't boot is exit 77, and the request gets none of its devices: the lease keeps only
/// what it held before. A device the lease holds that is gone by the end is exit 76. Else the
/// lease as it is then, with its devices' states.
async fn settle<W: AsyncWrite + Unpin>(
    lab: &Arc<Lab>,
    p: &AcquireParams,
    taken: Taken,
    writer: &mut W,
    id: Option<u64>,
) -> Answer<Lease> {
    let lease_id = taken.granted.lease.id;
    let Some(ask) = p.devices.clone() else {
        return Ok(taken.granted.lease);
    };
    if !taken.phones.is_empty() {
        let (lab, phones) = (Arc::clone(lab), taken.phones.clone());
        blocking(move || {
            for serial in &phones {
                let abi = android::abi(&AdbCli, serial);
                lab.write(|c, _| store::set_abi(c, serial, abi.as_deref()))?;
            }
            Ok(())
        })
        .await?;
    }
    if !taken.boots.is_empty() {
        let note = Outcome::Note(booting_note(taken.boots.len()));
        send(writer, Response { id, outcome: note }).await;
        if let Err(why) = boot(lab, &ask, lease_id, &taken.boots).await {
            let lab = Arc::clone(lab);
            let (phones, reused) = (taken.phones.clone(), taken.reused.clone());
            blocking(move || {
                lab.write(|c, now| {
                    for serial in phones.iter().chain(&reused) {
                        store::delete_held_device(c, lease_id, serial)?;
                    }
                    for serial in &reused {
                        store::emulator_idle(c, serial, now)?;
                    }
                    Ok(())
                })?;
                lab.notify();
                Ok(())
            })
            .await?;
            return Err(ErrorBody {
                code: Exit::Emulator.code(),
                message: why,
            });
        }
    }
    let warnings = {
        let lab = Arc::clone(lab);
        blocking(move || {
            let held = lab.write(|c, _| store::lease_devices(c, lease_id))?;
            Ok(held
                .iter()
                .filter_map(|d| {
                    android::prep(&AdbCli, &d.serial, ask.stay_awake, ask.animations_off)
                        .map(|warn| format!("{}: {warn}", d.serial))
                })
                .collect::<Vec<String>>())
        })
        .await?
    };
    for warning in warnings {
        send(
            writer,
            Response {
                id,
                outcome: Outcome::Note(warning),
            },
        )
        .await;
    }
    let seen = lab.attached().await;
    let lease = lab.write(|c, _| store::lease(c, lease_id))?;
    let lease = with_states(lease.unwrap_or(taken.granted.lease), &seen);
    match lease.lost() {
        Some(why) => Err(ErrorBody {
            code: Exit::DeviceLost.code(),
            message: why,
        }),
        None => Ok(lease),
    }
}

/// Boots the emulators at `idxs` for lease `lease`. They stay booting until every one has
/// finished, then join the lease together. One whose unit won't start or ends, or that doesn't
/// boot in time, fails them all: those still booting are stopped and forgotten, never marked
/// ready, and those that finished idle.
async fn boot(lab: &Arc<Lab>, ask: &DeviceAsk, lease: i64, idxs: &[u32]) -> Result<(), String> {
    let specs: Vec<EmulatorSpec> = idxs.iter().map(|idx| android::spec(*idx, ask)).collect();
    let started = {
        let specs = specs.clone();
        tokio::task::spawn_blocking(move || {
            specs.iter().try_for_each(|spec| {
                SystemdEmulators.ensure_avd(spec)?;
                SystemdEmulators.start(spec)
            })
        })
        .await
        .unwrap_or_else(|e| Err(daemon_failed(&e)))
    };
    if let Err(why) = started {
        lab.forget(specs).await;
        return Err(why);
    }
    let begun = Instant::now();
    let (mut pending, mut finished) = (specs, Vec::new());
    loop {
        let mut changed = lab.attached.subscribe();
        let seen = lab.attached().await;
        let elapsed = begun.elapsed().as_secs();
        let steps = {
            let pending = pending.clone();
            tokio::task::spawn_blocking(move || {
                pending
                    .iter()
                    .map(|spec| {
                        let look = android::look(&SystemdEmulators, &AdbCli, spec, &seen, elapsed);
                        boot_step(spec.idx, &spec.serial, look)
                    })
                    .collect::<Vec<Boot>>()
            })
            .await
            .map_err(|e| daemon_failed(&e))
        };
        let steps = match steps {
            Ok(steps) => steps,
            Err(why) => {
                lab.forget(pending).await;
                let _ = lab.hand_over(finished, None).await;
                return Err(why);
            }
        };
        let failed = steps.iter().find_map(|step| match step {
            Boot::Failed(why) => Some(why.clone()),
            _ => None,
        });
        let mut waiting = Vec::new();
        for (spec, step) in pending.into_iter().zip(steps) {
            if step == Boot::Ready {
                finished.push(spec);
            } else {
                waiting.push(spec);
            }
        }
        if let Some(why) = failed {
            lab.forget(waiting).await;
            let _ = lab.hand_over(finished, None).await;
            return Err(why);
        }
        if waiting.is_empty() {
            return lab.hand_over(finished, Some(lease)).await;
        }
        pending = waiting;
        tokio::select! {
            _ = changed.changed() => {}
            () = tokio::time::sleep(BOOT_LOOK) => {}
        }
    }
}

fn daemon_failed(e: &dyn std::fmt::Display) -> String {
    format!("the emulator boot failed in the daemon ({e}); run devlab acquire again")
}

impl Lab {
    /// Marks emulators that finished booting ready, all or none, and hands them to lease
    /// `lease`; with no lease, or when the lease ended while they booted, they idle. Then their
    /// screens are unlocked. An error when one of them is no longer booting in the store.
    async fn hand_over(
        self: &Arc<Self>,
        specs: Vec<EmulatorSpec>,
        lease: Option<i64>,
    ) -> Result<(), String> {
        if specs.is_empty() {
            return Ok(());
        }
        let lab = Arc::clone(self);
        let done = blocking(move || {
            let handed = lab.write(|c, now| {
                let rows = store::emulators(c)?;
                if let Some(gone) = specs.iter().find(|spec| {
                    !rows.iter().any(|r| {
                        r.idx == spec.idx
                            && r.serial == spec.serial
                            && State::from_word(&r.state) == State::Booting
                    })
                }) {
                    return Ok(Err(format!(
                        "{} was forgotten while it booted; run devlab acquire again",
                        gone.serial
                    )));
                }
                let holder = match lease {
                    Some(id) => store::lease(c, id)?.map(|l| l.id),
                    None => None,
                };
                for spec in &specs {
                    store::set_emulator_ready(c, spec.idx, holder.is_none().then_some(now))?;
                    if let Some(id) = holder {
                        store::insert_lease_device(c, id, &emulator_device(&spec.serial))?;
                    }
                }
                Ok(Ok(()))
            })?;
            if handed.is_ok() {
                for spec in &specs {
                    let _ = AdbCli.shell(&spec.serial, "input keyevent 82");
                }
            }
            lab.notify();
            Ok(handed)
        })
        .await;
        match done {
            Ok(handed) => handed,
            Err(body) => Err(body.message),
        }
    }

    /// Stops emulators that won't boot and forgets them.
    async fn forget(self: &Arc<Self>, specs: Vec<EmulatorSpec>) {
        if specs.is_empty() {
            return;
        }
        let lab = Arc::clone(self);
        let _ = blocking(move || {
            for spec in &specs {
                SystemdEmulators.stop(&spec.unit, &spec.serial, &AdbCli);
                lab.write(|c, _| store::delete_emulator(c, spec.idx, "booting"))?;
            }
            lab.notify();
            Ok(())
        })
        .await;
    }
}

/// Serves one `lab.acquire`: queues it, writes a note each time the reason it waits changes,
/// then the grant or the error. A client that hangs up leaves the queue. A run's acquire is then
/// followed to its end, which lands in the ledger.
pub async fn acquire<W, R, S>(
    lab: Arc<Lab>,
    ledger: Arc<Ledger>,
    request: Request,
    writer: &mut W,
    lines: &mut Lines<R>,
    stopping: S,
) where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    S: Future<Output = ()>,
{
    let id = Some(request.id);
    let p: AcquireParams = match parse(request.params) {
        Ok(p) => p,
        Err(body) => {
            send(
                writer,
                Response {
                    id,
                    outcome: Outcome::Error(body),
                },
            )
            .await;
            return;
        }
    };
    if !p.owner.found {
        let _ = writeln!(
            std::io::stderr(),
            "factory: lab: warning: the devlab call in {} has no claude or codex ancestor; {} owns its lease",
            p.worktree,
            p.owner.label
        );
    }
    let p = Arc::new(p);
    let mut run = p.run.clone().map(|ask| Run {
        ask,
        worktree: p.worktree.clone(),
        ledger,
        queued: Instant::now(),
        granted: None,
        started: None,
        lease: None,
    });
    let queued = {
        let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
        blocking(move || enqueue(&lab, &p)).await
    };
    let waiter = match queued {
        Ok(waiter) => waiter,
        Err(body) => {
            if let Some(run) = &run {
                run.record(End::Exit(body.code).outcome(), Phase::Queued.word())
                    .await;
            }
            send(
                writer,
                Response {
                    id,
                    outcome: Outcome::Error(body),
                },
            )
            .await;
            return;
        }
    };
    lab.notify();
    let year = Duration::from_secs(365 * 86400);
    let wait = Duration::try_from_secs_f64(p.wait_sec).unwrap_or(year);
    let now = Instant::now();
    let deadline = now
        .checked_add(wait)
        .or_else(|| now.checked_add(year))
        .unwrap_or(now);
    let mut said = String::new();
    loop {
        let mut changed = lab.changed.subscribe();
        let seen = if p.devices.is_some() {
            lab.attached().await
        } else {
            Vec::new()
        };
        let tried = {
            let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
            blocking(move || {
                let facts = gather(&lab, &p, seen);
                try_grant(&lab, &p, waiter, &facts)
            })
            .await
        };
        let why = match tried {
            Ok(Ok(taken)) => {
                lab.notify();
                lab.watch_owner(&taken.granted.lease);
                let (fresh, previous) = (taken.fresh, taken.granted.previous.clone());
                let lease = taken.granted.lease.clone();
                let outcome = match settle(&lab, &p, taken, writer, id).await {
                    Ok(lease) => Outcome::Result(
                        serde_json::to_value(Granted { lease, previous }).unwrap_or(Value::Null),
                    ),
                    Err(body) => Outcome::Error(body),
                };
                let answered = send(writer, Response { id, outcome }).await;
                // A caller gone before its answer never learns of the lease: give it back.
                if !answered && fresh {
                    lab.drop_unanswered(&lease);
                }
                if let Some(mut run) = run {
                    run.granted = Some(Instant::now());
                    if answered {
                        run.lease = Some(lease);
                        run.follow(lines, stopping).await;
                    } else {
                        run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                    }
                }
                return;
            }
            Ok(Err(why)) => why,
            Err(body) => {
                lab.remove_waiter(waiter);
                if let Some(run) = &run {
                    run.record(End::Exit(body.code).outcome(), Phase::Queued.word())
                        .await;
                }
                send(
                    writer,
                    Response {
                        id,
                        outcome: Outcome::Error(body),
                    },
                )
                .await;
                return;
            }
        };
        if Instant::now() >= deadline {
            lab.remove_waiter(waiter);
            let body = ErrorBody {
                code: Exit::Busy.code(),
                message: format!("gave up after {} s: {why}", js_number(p.wait_sec)),
            };
            if let Some(run) = &run {
                run.record(End::Exit(body.code).outcome(), Phase::Queued.word())
                    .await;
            }
            send(
                writer,
                Response {
                    id,
                    outcome: Outcome::Error(body),
                },
            )
            .await;
            return;
        }
        if why != said {
            let note = Response {
                id,
                outcome: Outcome::Note(format!("waiting: {why}")),
            };
            if !send(writer, note).await {
                lab.remove_waiter(waiter);
                if let Some(run) = &run {
                    run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                }
                return;
            }
            said = why;
        }
        tokio::select! {
            _ = changed.changed() => {}
            () = tokio::time::sleep(RECHECK) => {}
            () = tokio::time::sleep_until(deadline) => {}
            line = lines.next_line() => {
                let Ok(Some(line)) = line else {
                    lab.remove_waiter(waiter);
                    if let Some(run) = &run {
                        run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                    }
                    return;
                };
                if let Some(run) = &mut run
                    && let Some(end) = run_end(&line)
                {
                    lab.remove_waiter(waiter);
                    run.record(end.outcome, &end.phase).await;
                    let body = ErrorBody {
                        code: Exit::Precondition.code(),
                        message: "the run was stopped while queued".to_owned(),
                    };
                    send(
                        writer,
                        Response {
                            id,
                            outcome: Outcome::Error(body),
                        },
                    )
                    .await;
                    return;
                }
            }
        }
    }
}

/// A run's end, if `line` is one.
fn run_end(line: &str) -> Option<RunEnd> {
    let request: Request = serde_json::from_str(line).ok()?;
    (request.method == RUN_END)
        .then(|| serde_json::from_value(request.params).ok())
        .flatten()
}

/// A `devlab run` the daemon follows, from its acquire to its end.
struct Run {
    ask: RunAsk,
    worktree: String,
    ledger: Arc<Ledger>,
    queued: Instant,
    granted: Option<Instant>,
    started: Option<Instant>,
    lease: Option<Lease>,
}

impl Run {
    /// Reads the run's connection after the grant: its start, then its end, or the connection
    /// closing without one, which is a lost run. A daemon stopping under a run loses it too: the
    /// run may outlast any drain, and its end would reach no one.
    async fn follow<R, S>(&mut self, lines: &mut Lines<R>, stopping: S)
    where
        R: AsyncBufRead + Unpin,
        S: Future<Output = ()>,
    {
        tokio::pin!(stopping);
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line,
                () = &mut stopping => Ok(None),
            };
            let Ok(Some(line)) = line else {
                let phase = if self.started.is_some() {
                    Phase::Running
                } else {
                    Phase::Queued
                };
                self.record(End::Lost.outcome(), phase.word()).await;
                return;
            };
            if let Some(end) = run_end(&line) {
                self.record(end.outcome, &end.phase).await;
                return;
            }
            if serde_json::from_str::<Request>(&line).is_ok_and(|r| r.method == RUN_START) {
                self.started.get_or_insert_with(Instant::now);
            }
        }
    }

    /// Writes the run's `lab-run` event.
    async fn record(&self, outcome: String, phase: &str) {
        let now = Instant::now();
        let queued_until = self.granted.unwrap_or(now);
        let mut fields = Fields::new();
        fields.insert("worktree".to_owned(), self.worktree.clone().into());
        if let Some(lease) = &self.lease {
            fields.insert("lease".to_owned(), lease.id.into());
            if let Some(slot) = lease.slot {
                fields.insert("slot".to_owned(), slot.into());
            }
        }
        fields.insert("command".to_owned(), self.ask.command.clone().into());
        fields.insert(
            "queue_s".to_owned(),
            queued_until.duration_since(self.queued).as_secs().into(),
        );
        fields.insert(
            "run_s".to_owned(),
            self.started
                .map_or(0, |started| now.duration_since(started).as_secs())
                .into(),
        );
        fields.insert("outcome".to_owned(), outcome.into());
        fields.insert("phase".to_owned(), phase.into());
        if let Some(session) = &self.ask.session {
            fields.insert("session".to_owned(), session.clone().into());
        }
        let Ok(event) = Event::new("lab-run", fields) else {
            return;
        };
        let ledger = Arc::clone(&self.ledger);
        let landed = tokio::task::spawn_blocking(move || ledger.append(&event)).await;
        if !matches!(landed, Ok(Ok(_))) {
            let _ = writeln!(
                std::io::stderr(),
                "factory: lab: cannot record a run of {} in the ledger",
                self.worktree
            );
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Answer<T> + Send + 'static,
) -> Answer<T> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(failed(&e)))
}

/// Writes one response line; false when the client is gone.
async fn send<W: AsyncWrite + Unpin>(writer: &mut W, response: Response) -> bool {
    let Ok(mut line) = serde_json::to_string(&response) else {
        return false;
    };
    line.push('\n');
    writer.write_all(line.as_bytes()).await.is_ok()
}

/// Seconds since the epoch.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
