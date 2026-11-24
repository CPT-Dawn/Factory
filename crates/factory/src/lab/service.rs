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

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use factory_adapters::adb::{Adb, AdbCli, AdbTrack};
use factory_adapters::docker::{Containers, Docker};
use factory_adapters::emulator::{EmulatorSpec, Emulators, SystemdEmulators};
use factory_core::event::{Event, Fields};
use factory_core::exit::Exit;
use factory_core::lab::args::{LONGEST_WAIT, wait_span};
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

use super::reservations;
use super::store::{self, RunRow, Store};
use super::wire::{
    AcquireParams, Board, DeviceAsk, DeviceLine, FindParams, GC, Granted, LEASE, Lease,
    LeaseDevice, Owner, Process, ProjectAsk, RECEIVED, RELEASE, RUN_END, RUN_END_ID, RUN_START,
    RUN_STOPPING, ReleaseParams, Released, Reservation, RunAsk, RunEnd, RunResume, SLOT, STATUS,
    STOPPING, SlotParams, StopParams, UNRESERVE, Waiter,
};
use super::{android, host};
use crate::ledger::Ledger;
use crate::output::Failure;
use crate::protocol::{ErrorBody, Outcome, Request, Response, WaitLeaseParams, WaitLeaseResult};
use factory_core::lab::base_name;

/// How often a waiting acquire looks again without a change to the board: a port that freed
/// tells no one. An owner that exits does: the keeper watches each one.
const RECHECK: Duration = Duration::from_secs(3);
/// How long a call that needs the device list waits for adb's first one.
const FIRST_LIST: Duration = Duration::from_secs(15);
/// How often a booting emulator is looked at without a change to the device list.
const BOOT_LOOK: Duration = Duration::from_secs(2);
/// How long past its deadline a waiter whose caller is away keeps its place, so a caller that
/// lost the daemon as its wait ran out still hears that it gave up.
const AWAY_GRACE_MS: i64 = 30_000;
/// How long a caller may take to say it has its grant before the grant is given back.
const RECEIVED_TIMEOUT: Duration = Duration::from_secs(10);

mod reserve;
pub use reserve::reserve;

pub struct Lab {
    path: PathBuf,
    store: Mutex<Store>,
    /// Bumped on every change a waiter could be waiting for.
    changed: watch::Sender<u64>,
    /// What `adb track-devices` showed last; none until its first list.
    attached: watch::Sender<Option<Vec<Attached>>>,
    /// Starts the one device tracker, on the first call that needs the device list.
    tracking: Once,
    /// The waiters whose callers are connected to this daemon; the rest are kept for callers
    /// that come back after a restart, until they can't.
    callers: Mutex<HashSet<i64>>,
    /// The owners the keeper watches, by [`Lease::owner_key`].
    watched: Mutex<HashSet<String>>,
    /// What the keeper is stopping because it idled; no grant hands it out meanwhile.
    stopping: Mutex<keeper::Stopping>,
    /// Where runs no daemon can follow any more are recorded.
    ledger: Arc<Ledger>,
}

/// The lock, whether or not a thread panicked while it held it.
fn lock_ignoring_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

mod overview;

type Answer<T> = Result<T, ErrorBody>;

impl Lab {
    pub fn open(path: &Path, ledger: Arc<Ledger>) -> Result<Lab, String> {
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
            callers: Mutex::new(HashSet::new()),
            ledger,
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
                    let tracker = AdbTrack::from_env();
                    android::track_forever(
                        &tracker,
                        tracker.pace,
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
                        &|line| eprintln!("factory: lab: {line}"),
                    )
                });
            if spawned.is_err() {
                self.attached.send_replace(Some(Vec::new()));
            }
        });
    }

    /// One write transaction; dead owners' leases, dead stoppers' holds and waiters that can't
    /// come back are dropped first, telling waiters, and runs whose devlab went while no daemon
    /// followed them are recorded as `daemon-stopped`. The reverses the dropped leases' devices
    /// were given are taken back beside it, after the transaction.
    fn write<T>(&self, work: impl FnOnce(&Connection, i64) -> rusqlite::Result<T>) -> Answer<T> {
        self.write_telling(work).map(|(out, _)| out)
    }

    /// [`Lab::write`], and a line for each lease and reservation this very write dropped
    /// because its owner exited: what `devlab gc` reports. One the keeper dropped earlier is
    /// not told again.
    fn write_telling<T>(
        &self,
        work: impl FnOnce(&Connection, i64) -> rusqlite::Result<T>,
    ) -> Answer<(T, Vec<String>)> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now();
        let (freed, unreserved, unstopped, (orphans, ended), out) = store
            .write(|c| {
                let (freed, unreserved) = store::gc(c, now, &host::alive)?;
                let unstopped = store::clear_dead_stoppers(c, &host::alive)?;
                let attached = self.callers().clone();
                let pruned = store::prune_waiters(
                    c,
                    now_ms(),
                    AWAY_GRACE_MS,
                    &|id| attached.contains(&id),
                    &host::alive,
                )?;
                let orphans =
                    store::take_orphan_runs(c, &|id| attached.contains(&id), &host::alive)?;
                let mut ended = Vec::new();
                for run in &orphans {
                    ended.extend(store::end_run_lease(c, run.id, now)?);
                }
                Ok((
                    freed,
                    unreserved,
                    unstopped + pruned,
                    (orphans, ended),
                    work(c, now)?,
                ))
            })
            .map_err(|e| ErrorBody {
                code: Exit::Precondition.code(),
                message: format!("lab store {}: {e}; retry", self.path.display()),
            })?;
        drop(store);
        for run in &orphans {
            let phase = if run.started_ms.is_some() {
                Phase::Running
            } else {
                Phase::Queued
            };
            append_run(
                &self.ledger,
                run,
                End::DaemonStopped.outcome(),
                phase.word(),
            );
        }
        let lines: Vec<String> = freed
            .iter()
            .map(|l| format!("lease {} ({}, owner exited)", l.id, l.owner_label))
            .chain(unreserved)
            .collect();
        if !lines.is_empty() || unstopped > 0 || !ended.is_empty() {
            self.notify();
        }
        let freed: Vec<Lease> = freed.into_iter().chain(ended).collect();
        if !freed.is_empty() {
            keeper::take_back_reverses_on_thread(self.attached.borrow().clone(), freed);
        }
        Ok((out, lines))
    }

    fn callers(&self) -> std::sync::MutexGuard<'_, HashSet<i64>> {
        self.callers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Gives lease `lease` back whole, its devices as they are now, and ends request `waiter`:
    /// a request that failed for good. The reverses its devices were given are taken back, and
    /// `from`, the reservation the lease was made from, is put back.
    fn give_back(&self, waiter: i64, lease: i64, from: Option<&Reservation>) -> Answer<()> {
        let dropped = self.write(|c, now| {
            let held = store::lease(c, lease)?;
            if let Some(held) = &held {
                store::drop_lease(c, held, now)?;
                if let Some(from) = from {
                    reservations::restore(c, from, now)?;
                }
            }
            store::delete_waiter(c, waiter)?;
            Ok(held)
        })?;
        if let Some(held) = dropped {
            self.notify();
            keeper::take_back_reverses_on_thread(self.attached.borrow().clone(), vec![held]);
        }
        Ok(())
    }

    /// Ends a granted request once its caller said it has the answer, or can't. `to_drop` is a
    /// lease to give back with it: a fresh one its caller never learned of.
    fn settle(&self, waiter: i64, to_drop: Option<&Lease>) -> Answer<()> {
        self.write(|c, now| {
            if let Some(lease) = to_drop {
                store::drop_lease(c, lease, now)?;
            }
            store::delete_waiter(c, waiter)
        })?;
        if to_drop.is_some() {
            self.notify();
        }
        Ok(())
    }

    fn remove_waiter(&self, id: i64) {
        if let Ok(true) = self.write(|c, _| store::delete_waiter(c, id)) {
            self.notify();
        }
    }
}

/// Answers once lease `p.lease` is held no more: released, its owner exited (each look frees
/// a dead owner's leases first), or expired. Fails with exit 4 for an id never granted, and
/// with exit 124 when it is still held at `deadline`, `limit` after the wait began.
pub async fn wait_lease(
    lab: &Arc<Lab>,
    p: &WaitLeaseParams,
    deadline: Instant,
    limit: Duration,
) -> Result<WaitLeaseResult, Failure> {
    let id = p.lease;
    loop {
        // Subscribed before the look, so a release during it is not missed.
        let mut changed = lab.changed.subscribe();
        let looked = {
            let lab = Arc::clone(lab);
            blocking(move || {
                lab.write(|c, _| Ok((store::lease(c, id)?.is_some(), store::last_lease_id(c)?)))
            })
            .await
        };
        match looked.map_err(|body| {
            Failure::new(
                Exit::from_code(body.code).unwrap_or(Exit::Precondition),
                body.message,
            )
        })? {
            (false, last) if id > last => {
                return Err(Failure::new(
                    Exit::NoLease,
                    format!("no lease {id} was ever granted; see factory lab status"),
                ));
            }
            (false, _) => {
                return Ok(WaitLeaseResult {
                    lease: id,
                    held: false,
                });
            }
            (true, _) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Failure::new(
                Exit::Timeout,
                format!(
                    "lease {id} still held after {}; wait again",
                    factory_core::duration::show(limit)
                ),
            ));
        }
        // A change, or a look again in a while: an owner can exit unnoticed between changes.
        let next = (now + RECHECK).min(deadline);
        tokio::select! {
            _ = changed.changed() => {}
            () = tokio::time::sleep_until(next) => {}
        }
    }
}

/// Answers a lab method that doesn't wait; none for a method that isn't the lab's.
pub async fn respond(lab: &Arc<Lab>, method: &str, params: Value) -> Option<Outcome> {
    let work: fn(&Lab, Value, &[Attached]) -> Answer<Value> = match method {
        LEASE => |lab, p, seen| reply(lab_lease(lab, parse(p)?, seen)),
        RELEASE => |lab, p, _| reply(release(lab, parse(p)?)),
        STOPPING => |lab, p, _| reply(stopping(lab, parse(p)?)),
        GC => |lab, _, _| reply(lab.write_telling(|_, _| Ok(())).map(|((), freed)| freed)),
        STATUS => |lab, _, seen| reply(status(lab, seen)),
        SLOT => |lab, p, _| reply(slot(lab, parse(p)?)),
        UNRESERVE => |lab, p, _| reply(reserve::unreserve(lab, parse(p)?)),
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

fn release(lab: &Lab, p: ReleaseParams) -> Answer<Released> {
    let released = lab.write(|c, now| {
        let f = &p.find;
        let mine = |l: &Lease| held_by(l, &f.owner);
        let dropped = if p.all {
            store::leases(c)?.into_iter().filter(mine).collect()
        } else {
            match own_lease(c, f)? {
                Ok(found) => found.into_iter().collect(),
                Err(refused) => return Ok(Err(refused)),
            }
        };
        for lease in &dropped {
            store::drop_lease(c, lease, now)?;
        }
        let unstopped = match &p.stopper {
            Some(stopper) => store::clear_stopping(c, stopper)?,
            None => 0,
        };
        let still_held = store::leases(c)?.into_iter().filter(mine).collect();
        Ok(Ok((
            Released {
                dropped,
                still_held,
            },
            unstopped,
        )))
    })?;
    let (released, unstopped) = released?;
    if !released.dropped.is_empty() || unstopped > 0 {
        lab.notify();
    }
    keeper::take_back_reverses(lab.attached.borrow().clone().as_deref(), &released.dropped);
    Ok(released)
}

fn held_by(lease: &Lease, owner: &Owner) -> bool {
    lease.owner_pid == owner.pid && lease.owner_start == owner.start
}

/// The caller's lease, found as `find_lease` does; a lease named by id that another owner holds
/// is refused instead.
fn own_lease(c: &Connection, f: &FindParams) -> rusqlite::Result<Result<Option<Lease>, ErrorBody>> {
    let lease = store::find_lease(c, &f.worktree, &f.owner, f.lease)?;
    Ok(match lease {
        Some(lease) if f.lease.is_some() && !held_by(&lease, &f.owner) => Err(not_yours(&lease)),
        found => Ok(found),
    })
}

/// The refusal of a lease named by id that another owner holds.
fn not_yours(lease: &Lease) -> ErrorBody {
    ErrorBody {
        code: Exit::NoLease.code(),
        message: format!(
            "lease {} belongs to {} ({}), not to you: it stays held; ask its owner to release it",
            lease.id, lease.owner_label, lease.worktree
        ),
    }
}

/// The caller's lease, its slot marked as stopping for `p.stopper`.
fn stopping(lab: &Lab, p: StopParams) -> Answer<Option<Lease>> {
    lab.write(|c, _| {
        let f = &p.find;
        let lease = match own_lease(c, f)? {
            Ok(lease) => lease,
            Err(refused) => return Ok(Err(refused)),
        };
        if let Some(Lease {
            project: Some(project),
            slot: Some(slot),
            ..
        }) = &lease
        {
            store::mark_stopping(c, project, *slot, &p.stopper)?;
        }
        Ok(Ok(lease))
    })?
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
            reservations: reservations::all(c)?,
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
    Facts {
        attached,
        mem_available_mb,
        ..gather_stack(p.project.as_ref())
    }
}

/// The facts about a project's stacks and the host's ports, for a request that asks for no device.
fn gather_stack(project: Option<&ProjectAsk>) -> Facts {
    let running = match project {
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
        attached: Vec::new(),
        mem_available_mb: 0,
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
    /// The reservation a fresh lease was made from, put back if the lease is given back.
    from: Option<Reservation>,
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

/// Leaves the waiter to callers that come back, once its caller's connection ends.
struct Present(Arc<Lab>, i64);

impl Drop for Present {
    fn drop(&mut self) {
        self.0.callers().remove(&self.1);
    }
}

/// Where a request stands once queued.
enum Queued {
    /// In the queue at `id`, giving up at `deadline_ms`; `resumed`: a place it had before.
    Waiting {
        id: i64,
        deadline_ms: Option<i64>,
        resumed: bool,
    },
    /// Granted before the daemon could answer it.
    Granted { id: i64, granted: Granted },
}

/// Queues the request, or takes back the place it had if it is still its own: the one it names,
/// else the one its devlab call holds, so a caller that never heard of its place keeps it too.
/// The caller counts as attached from here.
fn enqueue(lab: &Lab, p: &AcquireParams) -> Answer<(Queued, Option<RunRow>)> {
    lab.write(|c, now| {
        let kept = store::place(c, p)?;
        let queued = match kept {
            Some(store::Place {
                waiter,
                granted: Some(granted),
            }) => Queued::Granted {
                id: waiter.id,
                granted,
            },
            Some(store::Place { waiter, .. }) => Queued::Waiting {
                id: waiter.id,
                deadline_ms: waiter.deadline_ms,
                resumed: true,
            },
            None => {
                let (id, deadline_ms) = insert(c, p, now)?;
                Queued::Waiting {
                    id,
                    deadline_ms,
                    resumed: false,
                }
            }
        };
        let id = match &queued {
            Queued::Waiting { id, .. } | Queued::Granted { id, .. } => *id,
        };
        lab.callers().insert(id);
        let run = match (&p.run, &p.client) {
            (Some(_), Some(_)) => Some(store::begin_run(c, &fresh_run(p, id))?),
            _ => None,
        };
        Ok((queued, run))
    })
}

/// The run `p` asks for, as queued now at waiter `id`.
fn fresh_run(p: &AcquireParams, id: i64) -> RunRow {
    let ask = p.run.clone().unwrap_or(RunAsk {
        command: String::new(),
        session: None,
        keep: false,
    });
    RunRow {
        id,
        worktree: p.worktree.clone(),
        command: ask.command,
        session: ask.session,
        client: p.client.clone().unwrap_or(Process {
            pid: 0,
            start: String::new(),
        }),
        queued_ms: now_ms(),
        granted_ms: None,
        started_ms: None,
        lease: None,
        slot: None,
    }
}

fn insert(c: &Connection, p: &AcquireParams, now: i64) -> rusqlite::Result<(i64, Option<i64>)> {
    let deadline_ms = i64::try_from(wait_span(p.wait_sec).as_millis())
        .ok()
        .and_then(|wait| now_ms().checked_add(wait));
    let name = base_name(&p.worktree);
    let id = store::insert_waiter(
        c,
        &Waiter {
            id: 0,
            created: now,
            label: format!("{} @ {name}", p.owner.label),
            owner: p.owner.key(),
            owner_label: p.owner.label.clone(),
            worktree: p.worktree.clone(),
            lease: p.lease,
            project: p.project.as_ref().map(|project| project.name.clone()),
            slot_cap: p.project.as_ref().map(|project| project.cap),
            max_slots: p.project.as_ref().map_or(0, |project| project.max_slots),
            deadline_ms,
            client_pid: p.client.as_ref().map(|client| client.pid),
            client_start: p.client.as_ref().map(|client| client.start.clone()),
        },
    )?;
    Ok((id, deadline_ms))
}

fn project_ask(p: &AcquireParams) -> Option<grant::ProjectAsk> {
    p.project.as_ref().map(grant_project)
}

fn grant_project(project: &ProjectAsk) -> grant::ProjectAsk {
    grant::ProjectAsk {
        name: project.name.clone(),
        max_slots: project.max_slots,
        cap: project.cap,
        has_stack: project.stack.is_some(),
    }
}

/// The board a request is decided on: the leases and the reservations (both hold a slot of their
/// owner's), the queue, and the asker's project's slots.
fn grant_board(
    c: &Connection,
    project: Option<&ProjectAsk>,
    worktree: &str,
    facts: &Facts,
    stopping: &keeper::Stopping,
) -> rusqlite::Result<grant::Board> {
    let mut leases: Vec<_> = store::leases(c)?.iter().map(Lease::holding).collect();
    leases.extend(reservations::all(c)?.iter().map(|r| r.holding()));
    let rows = store::slots(c)?;
    Ok(grant::Board {
        leases,
        queue: store::waiters(c)?.iter().map(Waiter::entry).collect(),
        slots: project.map_or_else(Vec::new, |project| {
            (0..project.max_slots)
                .map(|slot| {
                    let (own_ports, stack_ports) = slot_host_ports(
                        &project.ports,
                        project.stride,
                        slot,
                        project.stack.as_ref().map(|s| s.ports.as_slice()),
                    );
                    SlotView {
                        held: rows
                            .iter()
                            .any(|r| r.project == project.name && r.slot == slot && r.stopping),
                        last_is_mine: rows.iter().any(|r| {
                            r.project == project.name
                                && r.slot == slot
                                && r.last_worktree.as_deref() == Some(worktree)
                        }),
                        stack_running: facts.running.get(slot as usize).copied().unwrap_or(false),
                        own_ports,
                        stack_ports,
                        stopping: stopping.slots.contains(&(project.name.clone(), slot)),
                    }
                })
                .collect()
        }),
        listening: facts.listening.clone(),
    })
}

/// A run's request (its run kept under the waiter's id) gets lease `lease`, new to it: the lease
/// ends with the run unless the run keeps it (`--keep`). `from`: the reservation it was made from.
fn ends_with_run(
    c: &Connection,
    p: &AcquireParams,
    waiter: i64,
    lease: &Lease,
    from: Option<&Reservation>,
) -> rusqlite::Result<()> {
    match &p.run {
        Some(run) if !run.keep => store::end_with_run(c, waiter, lease.id, from),
        _ => Ok(()),
    }
}

/// The lease granted, or why the request waits.
fn try_grant(
    lab: &Lab,
    p: &AcquireParams,
    waiter: i64,
    facts: &Facts,
) -> Answer<Result<Taken, String>> {
    lab.write(|c, now| {
        let mut lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        // A reservation of the caller's project is used once: it becomes the caller's lease,
        // without a turn in the queue. One that is gone (used, released or its lead exited)
        // asks for a slot the normal way. The claim is undone when the request then waits: its
        // lease is made for good only in a grant, so a request that waits, or gives up, leaves
        // the reservation as it was.
        c.execute_batch("SAVEPOINT claim")?;
        let mut claimed = None;
        if lease.is_none()
            && let (Some(id), Some(project)) = (p.reservation, &p.project)
            && let Some((made, from)) =
                reservations::claim(c, id, &project.name, &p.worktree, &p.owner, now)?
        {
            lease = Some(made);
            claimed = Some(from);
        }
        if claimed.is_some()
            && p.devices.is_none()
            && let (Some(lease), Some(project)) = (&lease, &p.project)
        {
            c.execute_batch("RELEASE claim")?;
            let previous = match lease.slot {
                Some(slot) => store::claim_slot(c, &project.name, slot, &p.worktree)?,
                None => None,
            };
            store::touch(c, lease.id, now)?;
            store::delete_waiter(c, waiter)?;
            ends_with_run(c, p, waiter, lease, claimed.as_ref())?;
            return Ok(Ok(Taken {
                granted: Granted {
                    lease: lease.clone(),
                    previous,
                },
                fresh: true,
                boots: Vec::new(),
                phones: Vec::new(),
                reused: Vec::new(),
                from: claimed,
            }));
        }
        let stopping = lock_ignoring_poison(&lab.stopping).clone();
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
        let board = grant_board(c, p.project.as_ref(), &p.worktree, facts, &stopping)?;
        let ask = Ask {
            waiter,
            project: project_ask(p),
            existing: lease.as_ref().map(|l| l.slot),
            devices_wait,
        };
        let (slot, new) = match grant::decide(&ask, &board, now) {
            Decision::Wait(why) => {
                if claimed.is_some() {
                    c.execute_batch("ROLLBACK TO claim")?;
                }
                c.execute_batch("RELEASE claim")?;
                return Ok(Err(why));
            }
            Decision::Grant { slot, new } => (slot, new),
        };
        c.execute_batch("RELEASE claim")?;
        let fresh = new || lease.is_none() || claimed.is_some();
        // The slot goes back to the reservation only from the lease it was made into.
        let from = claimed.filter(|_| !new);
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
        if fresh {
            ends_with_run(c, p, waiter, &lease, from.as_ref())?;
        }
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
        // A grant that boots emulators can't be handed to a caller again after a restart: the
        // daemon that comes back stops them, so the place goes now, as it did before.
        if fresh && boots.is_empty() {
            store::mark_granted(c, waiter, lease.id, previous.as_deref())?;
        } else {
            store::delete_waiter(c, waiter)?;
        }
        let lease = store::lease(c, lease.id)?.unwrap_or(lease);
        Ok(Ok(Taken {
            granted: Granted { lease, previous },
            fresh,
            boots,
            phones,
            reused,
            from,
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
/// that won't boot is exit 77, and the request gets none of its devices: the caller of this
/// function gives the whole lease back (`Lab::give_back`). A device the lease holds that is gone by the end is exit 76. Else the
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
    tokio::pin!(stopping);
    let queued = {
        let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
        blocking(move || enqueue(&lab, &p)).await
    };
    let mut run = p.run.as_ref().map(|_| {
        let (row, kept) = match &queued {
            Ok((_, Some(row))) => (row.clone(), true),
            _ => (fresh_run(&p, 0), false),
        };
        Run {
            lab: Arc::clone(&lab),
            row,
            kept,
        }
    });
    let (waiter, deadline_ms, resumed) = match queued.map(|(queued, _)| queued) {
        Ok(Queued::Waiting {
            id,
            deadline_ms,
            resumed,
        }) => (id, deadline_ms, resumed),
        Ok(Queued::Granted {
            id: waiter,
            granted,
        }) => {
            let _attached = Present(Arc::clone(&lab), waiter);
            let place = (waiter, granted, true);
            deliver(&lab, writer, lines, id, place, run, stopping.as_mut()).await;
            return;
        }
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
    let _attached = Present(Arc::clone(&lab), waiter);
    lab.notify();
    let place = Response {
        id,
        outcome: Outcome::Queued(waiter),
    };
    if !send(writer, place).await {
        lab.remove_waiter(waiter);
        if let Some(run) = &run {
            run.record(End::Lost.outcome(), Phase::Queued.word()).await;
        }
        return;
    }
    // A wait that ran out while the daemon was away takes nothing that came free meanwhile.
    if resumed && deadline_ms.is_some_and(|ms| ms <= now_ms()) {
        lab.remove_waiter(waiter);
        let body = ErrorBody {
            code: Exit::Busy.code(),
            message: format!(
                "gave up after {} s: the wait ran out while the daemon restarted",
                js_number(p.wait_sec)
            ),
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
    let now = Instant::now();
    let left = deadline_ms.map_or(LONGEST_WAIT, |ms| {
        Duration::from_millis(u64::try_from(ms.saturating_sub(now_ms())).unwrap_or(0))
    });
    let deadline = now
        .checked_add(left)
        .or_else(|| now.checked_add(LONGEST_WAIT))
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
                let from = taken.from.clone();
                match settle(&lab, &p, taken, writer, id).await {
                    Ok(lease) => {
                        let place = (waiter, Granted { lease, previous }, fresh);
                        deliver(&lab, writer, lines, id, place, run, stopping.as_mut()).await;
                    }
                    Err(mut body) => {
                        // An emulator that would not boot (exit 77), or a device of a fresh lease
                        // gone before its prep (exit 76), gives the lease back before its caller
                        // hears so: it can ask `devlab env` at once and find none. Any other
                        // failure keeps the lease, and only a caller that never hears of a fresh
                        // one loses it.
                        let mut given_back = false;
                        if body.code == Exit::Emulator.code()
                            || (fresh && body.code == Exit::DeviceLost.code())
                        {
                            match lab.give_back(waiter, lease.id, from.as_ref()) {
                                Ok(()) => given_back = true,
                                // Not the exit it failed with: the lease is still held.
                                Err(why) => {
                                    body = ErrorBody {
                                        code: why.code,
                                        message: format!(
                                            "{}; lease {} is still held ({}): run devlab release",
                                            body.message, lease.id, why.message
                                        ),
                                    };
                                }
                            }
                        }
                        let outcome = Outcome::Error(body);
                        let answered = send(writer, Response { id, outcome }).await;
                        if !given_back {
                            let _ = lab.settle(waiter, (!answered && fresh).then_some(&lease));
                        }
                        if let Some(mut run) = run {
                            if answered {
                                run.granted((!given_back).then_some(&lease)).await;
                                run.follow(writer, lines, None, stopping.as_mut()).await;
                            } else {
                                run.granted(None).await;
                                run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                            }
                        }
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
            // The daemon is stopping: the place, and a run's, stay for its caller to ask the
            // next daemon for.
            () = stopping.as_mut() => return,
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

/// How a grant's caller took it.
enum Receipt {
    /// The caller has it; `then`, the line of a run that said so by starting or ending.
    Received { then: Option<String> },
    /// The caller never had it, and a fresh lease went back; `end`, a run's, if it said one.
    Gone { end: Option<RunEnd> },
    /// The daemon stopped first: the grant, and a run's, stay for the caller to ask for again.
    Stopping,
}

/// Answers a grant and, for a run, follows the run to its end.
async fn deliver<W, R, S>(
    lab: &Lab,
    writer: &mut W,
    lines: &mut Lines<R>,
    id: Option<u64>,
    (waiter, granted, fresh): (i64, Granted, bool),
    run: Option<Run>,
    mut stopping: Pin<&mut S>,
) where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    S: Future<Output = ()>,
{
    let lease = granted.lease.clone();
    let receipt = answer(
        lab,
        writer,
        lines,
        id,
        (waiter, granted, fresh),
        &mut stopping,
    )
    .await;
    let Some(mut run) = run else { return };
    match receipt {
        Receipt::Received { then } => {
            run.granted(Some(&lease)).await;
            run.follow(writer, lines, then, stopping).await;
        }
        Receipt::Gone { end: Some(end) } => {
            run.granted(None).await;
            run.record(end.outcome, &end.phase).await;
        }
        Receipt::Gone { end: None } => {
            run.granted(None).await;
            run.record(End::Lost.outcome(), Phase::Queued.word()).await;
        }
        Receipt::Stopping => {}
    }
}

/// Sends the grant, then lets the waiter go once the caller says it has it ([`RECEIVED`]), and
/// confirms. A run says so by starting or ending too, unless it ended while still queued. A
/// caller gone before it says so never learns of a `fresh` lease: it is given back. Until the
/// waiter goes, a daemon that restarts hands its caller the same grant.
async fn answer<W, R, S>(
    lab: &Lab,
    writer: &mut W,
    lines: &mut Lines<R>,
    id: Option<u64>,
    (waiter, granted, fresh): (i64, Granted, bool),
    stopping: &mut Pin<&mut S>,
) -> Receipt
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    S: Future<Output = ()>,
{
    let lease = granted.lease.clone();
    let give_back = |end| {
        let _ = lab.settle(waiter, fresh.then_some(&lease));
        Receipt::Gone { end }
    };
    let value = serde_json::to_value(granted).unwrap_or(Value::Null);
    let sent = send(
        writer,
        Response {
            id,
            outcome: Outcome::Result(value),
        },
    )
    .await;
    if !sent {
        return give_back(None);
    }
    let next = tokio::select! {
        next = tokio::time::timeout(RECEIVED_TIMEOUT, lines.next_line()) => next,
        () = stopping.as_mut() => return Receipt::Stopping,
    };
    let Ok(Ok(Some(line))) = next else {
        return give_back(None);
    };
    match serde_json::from_str::<Request>(&line) {
        Ok(said) if said.method == RECEIVED => {
            let outcome = match lab.settle(waiter, None) {
                Ok(()) => Outcome::Result(Value::Bool(true)),
                Err(body) => Outcome::Error(body),
            };
            send(
                writer,
                Response {
                    id: Some(said.id),
                    outcome,
                },
            )
            .await;
            Receipt::Received { then: None }
        }
        _ => match run_end(&line) {
            Some(end) if end.phase == Phase::Queued.word() => give_back(Some(end)),
            _ => {
                let _ = lab.settle(waiter, None);
                Receipt::Received { then: Some(line) }
            }
        },
    }
}

/// A run's end, if `line` is one.
fn run_end(line: &str) -> Option<RunEnd> {
    let request: Request = serde_json::from_str(line).ok()?;
    (request.method == RUN_END)
        .then(|| serde_json::from_value(request.params).ok())
        .flatten()
}

/// A `devlab run` the daemon follows, from its acquire to its end, across daemon restarts:
/// the store keeps it (`kept`) until its end is recorded.
struct Run {
    lab: Arc<Lab>,
    row: RunRow,
    kept: bool,
}

impl Run {
    /// The run has had its grant: `lease`, or none when it was given back.
    async fn granted(&mut self, lease: Option<&Lease>) {
        self.row.granted_ms.get_or_insert_with(now_ms);
        self.row.lease = lease.map(|lease| lease.id);
        self.row.slot = lease.and_then(|lease| lease.slot);
        if let (true, Some(lease)) = (self.kept, self.row.lease) {
            let (lab, id, ms, slot) = (
                Arc::clone(&self.lab),
                self.row.id,
                self.row.granted_ms.unwrap_or_default(),
                self.row.slot,
            );
            let _ = blocking(move || lab.write(|c, _| store::run_granted(c, id, ms, lease, slot)))
                .await;
        }
    }

    /// Reads the run's connection after the grant, from its line `first` if it was read
    /// already: its start, then its end, which is answered once recorded, or the connection
    /// closing without one, which is a lost run. A daemon that stops leaves the run to the next
    /// one, which the run asks to follow it ([`RUN_RESUME`](super::wire::RUN_RESUME)).
    async fn follow<W, R, S>(
        &mut self,
        writer: &mut W,
        lines: &mut Lines<R>,
        first: Option<String>,
        stopping: S,
    ) where
        W: AsyncWrite + Unpin,
        R: AsyncBufRead + Unpin,
        S: Future<Output = ()>,
    {
        tokio::pin!(stopping);
        let mut first = first;
        // A signal stopped devlab while its command runs: the run's end, should the connection
        // close before devlab says the command is gone.
        let mut stopped: Option<RunEnd> = None;
        loop {
            let line = match first.take() {
                Some(line) => line,
                None => tokio::select! {
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => line,
                        _ => {
                            if let Some(end) = stopped {
                                self.record(end.outcome, &end.phase).await;
                                return;
                            }
                            let phase = if self.row.started_ms.is_some() {
                                Phase::Running
                            } else {
                                Phase::Queued
                            };
                            self.record(End::Lost.outcome(), phase.word()).await;
                            return;
                        }
                    },
                    () = &mut stopping => return,
                },
            };
            let Ok(request) = serde_json::from_str::<Request>(&line) else {
                continue;
            };
            if let Some(end) = run_end(&line) {
                self.record(end.outcome, &end.phase).await;
                if request.id == RUN_END_ID {
                    let done = Outcome::Result(Value::Bool(true));
                    send(
                        writer,
                        Response {
                            id: Some(request.id),
                            outcome: done,
                        },
                    )
                    .await;
                }
                return;
            }
            if request.method == RUN_STOPPING {
                stopped = serde_json::from_value(request.params).ok().or(stopped);
                continue;
            }
            if request.method == RUN_START && self.row.started_ms.is_none() {
                let ms = now_ms();
                self.row.started_ms = Some(ms);
                if self.kept {
                    let (lab, id) = (Arc::clone(&self.lab), self.row.id);
                    let _ = blocking(move || lab.write(|c, _| store::run_started(c, id, ms))).await;
                }
            }
        }
    }

    /// Writes the run's `lab-run` event, unless another took the run out of the store first.
    async fn record(&self, outcome: String, phase: &str) {
        if self.kept {
            let (lab, id) = (Arc::clone(&self.lab), self.row.id);
            let taken = blocking(move || {
                lab.write(|c, now| {
                    let Some(run) = store::take_run(c, id)? else {
                        return Ok(None);
                    };
                    Ok(Some((run, store::end_run_lease(c, id, now)?)))
                })
            })
            .await;
            match taken {
                Ok(None) => return,
                Ok(Some((_, Some(ended)))) => {
                    self.lab.notify();
                    keeper::take_back_reverses_on_thread(
                        self.lab.attached.borrow().clone(),
                        vec![ended],
                    );
                }
                _ => {}
            }
        }
        let (ledger, row, phase) = (
            Arc::clone(&self.lab.ledger),
            self.row.clone(),
            phase.to_owned(),
        );
        let _ =
            tokio::task::spawn_blocking(move || append_run(&ledger, &row, outcome, &phase)).await;
    }
}

/// Picks up a run ([`RUN_RESUME`](super::wire::RUN_RESUME)) whose daemon went away, and follows it to its end.
pub async fn resume_run<W, R, S>(
    lab: Arc<Lab>,
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
    let found = match parse::<RunResume>(request.params) {
        Ok(p) => {
            let lab = Arc::clone(&lab);
            blocking(move || {
                lab.write(|c, _| {
                    let run = store::run_of(c, &p.client)?;
                    Ok(run.filter(|run| lab.callers().insert(run.id)))
                })
            })
            .await
        }
        Err(body) => Err(body),
    };
    let found = found.and_then(|row| {
        row.ok_or_else(|| ErrorBody {
            code: Exit::Precondition.code(),
            message: "no run of this devlab for the daemon to follow".to_owned(),
        })
    });
    let row = match found {
        Ok(row) => row,
        Err(body) => {
            let outcome = Outcome::Error(body);
            send(writer, Response { id, outcome }).await;
            return;
        }
    };
    let _attached = Present(Arc::clone(&lab), row.id);
    let followed = Outcome::Result(Value::Bool(true));
    if !send(
        writer,
        Response {
            id,
            outcome: followed,
        },
    )
    .await
    {
        return;
    }
    let mut run = Run {
        lab,
        row,
        kept: true,
    };
    run.follow(writer, lines, None, stopping).await;
}

/// Appends the `lab-run` event of `run`, ended with `outcome` in `phase`.
fn append_run(ledger: &Ledger, run: &RunRow, outcome: String, phase: &str) {
    let now = now_ms();
    let secs = |since: i64| u64::try_from(now.saturating_sub(since) / 1000).unwrap_or(0);
    let mut fields = Fields::new();
    fields.insert("worktree".to_owned(), run.worktree.clone().into());
    if let Some(lease) = run.lease {
        fields.insert("lease".to_owned(), lease.into());
        if let Some(slot) = run.slot {
            fields.insert("slot".to_owned(), slot.into());
        }
    }
    fields.insert("command".to_owned(), run.command.clone().into());
    let queued_until = run.granted_ms.unwrap_or(now);
    fields.insert(
        "queue_s".to_owned(),
        u64::try_from(queued_until.saturating_sub(run.queued_ms) / 1000)
            .unwrap_or(0)
            .into(),
    );
    fields.insert("run_s".to_owned(), run.started_ms.map_or(0, secs).into());
    fields.insert("outcome".to_owned(), outcome.into());
    fields.insert("phase".to_owned(), phase.into());
    if let Some(session) = &run.session {
        fields.insert("session".to_owned(), session.clone().into());
    }
    let landed = Event::new("lab-run", fields).map(|event| ledger.append(&event));
    if !matches!(landed, Ok(Ok(_))) {
        let _ = writeln!(
            std::io::stderr(),
            "factory: lab: cannot record a run of {} in the ledger",
            run.worktree
        );
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

/// Milliseconds since the epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Seconds since the epoch.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
