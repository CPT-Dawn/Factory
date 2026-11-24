//! The lease store: `lab.db`, written by the daemon alone. Every write is one short
//! `BEGIN IMMEDIATE` transaction, and no external command runs while one is open.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::wire::{
    AcquireParams, Granted, Lease, LeaseDevice, Owner, Process, Reservation, SlotRow, SlotSchema,
    Waiter,
};
use super::{activity, readiness, reservations, stack};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS leases (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project TEXT,
  worktree TEXT NOT NULL,
  owner_pid INTEGER NOT NULL,
  owner_start TEXT NOT NULL,
  owner_label TEXT NOT NULL,
  slot INTEGER,
  created INTEGER NOT NULL,
  touched INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS leases_slot ON leases (project, slot) WHERE slot IS NOT NULL;
CREATE TABLE IF NOT EXISTS slots (
  project TEXT NOT NULL,
  slot INTEGER NOT NULL,
  last_worktree TEXT,
  stack_ours INTEGER NOT NULL DEFAULT 0,
  idle_since INTEGER,
  PRIMARY KEY (project, slot)
);
CREATE TABLE IF NOT EXISTS slot_schemas (
  project TEXT NOT NULL,
  slot INTEGER NOT NULL,
  worktree TEXT NOT NULL,
  head TEXT,
  PRIMARY KEY (project, slot)
);
-- A stack an earlier daemon started and has not stopped ran its last worktree's schema. A
-- stack this daemon starts is recorded as it starts, so this adds nothing once a record exists.
INSERT OR IGNORE INTO slot_schemas (project, slot, worktree)
  SELECT project, slot, last_worktree FROM slots WHERE stack_ours = 1 AND last_worktree IS NOT NULL;
CREATE TABLE IF NOT EXISTS stoppers (
  project TEXT NOT NULL,
  slot INTEGER NOT NULL,
  pid INTEGER NOT NULL,
  start TEXT NOT NULL,
  PRIMARY KEY (project, slot, pid, start)
);
CREATE TABLE IF NOT EXISTS waiters (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  created INTEGER NOT NULL,
  label TEXT NOT NULL,
  owner TEXT NOT NULL,
  owner_label TEXT NOT NULL,
  worktree TEXT NOT NULL,
  lease INTEGER,
  project TEXT,
  slot_cap INTEGER,
  max_slots INTEGER NOT NULL,
  deadline_ms INTEGER,
  client_pid INTEGER,
  client_start TEXT,
  granted INTEGER,
  previous TEXT
);
CREATE TABLE IF NOT EXISTS lease_devices (
  serial TEXT PRIMARY KEY,
  lease_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  abi TEXT,
  model TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY,
  worktree TEXT NOT NULL,
  command TEXT NOT NULL,
  session TEXT,
  client_pid INTEGER NOT NULL,
  client_start TEXT NOT NULL,
  queued_ms INTEGER NOT NULL,
  granted_ms INTEGER,
  started_ms INTEGER,
  lease INTEGER,
  slot INTEGER
);
CREATE TABLE IF NOT EXISTS run_leases (
  run INTEGER PRIMARY KEY,
  lease INTEGER NOT NULL,
  reservation TEXT
);
CREATE TABLE IF NOT EXISTS emulators (
  idx INTEGER PRIMARY KEY,
  serial TEXT NOT NULL,
  state TEXT NOT NULL,
  idle_since INTEGER
);
";

/// An emulator devlab runs: `booting` from its start until adb says it finished booting, then
/// `ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmulatorRow {
    pub idx: u32,
    pub serial: String,
    pub state: String,
    /// Since when no lease holds it; none while one does, or before its first lease ends.
    pub idle_since: Option<i64>,
}
/// The schema's version, in `PRAGMA user_version`. Version 0 kept waiters only while the
/// daemon that queued them ran, so their table is rebuilt; `stoppers` is new. Version 2 adds
/// `runs`. `run_leases` (#53) needs no new version: `SCHEMA` makes it on every open.
const VERSION: i64 = 2;

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the store. Waiters keep their places: their callers come back to them.
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(15))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 1 {
            conn.execute_batch("DROP TABLE IF EXISTS waiters")?;
        }
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(reservations::SCHEMA)?;
        conn.execute_batch(readiness::SCHEMA)?;
        conn.execute_batch(activity::SCHEMA)?;
        if version < VERSION {
            conn.pragma_update(None, "user_version", VERSION)?;
        }
        Ok(Store { conn })
    }

    /// Runs `work` in one write transaction, committed when it returns Ok.
    pub fn write<T>(
        &mut self,
        work: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = work(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

fn lease_row(row: &Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        id: row.get("id")?,
        project: row.get("project")?,
        worktree: row.get("worktree")?,
        owner_pid: row.get("owner_pid")?,
        owner_start: row.get("owner_start")?,
        owner_label: row.get("owner_label")?,
        slot: row.get("slot")?,
        created: row.get("created")?,
        touched: row.get("touched")?,
        devices: Vec::new(),
        reclaimed: Vec::new(),
    })
}

/// The lease with the devices it holds.
fn with_devices(c: &Connection, mut lease: Lease) -> rusqlite::Result<Lease> {
    lease.devices = lease_devices(c, lease.id)?;
    lease.reclaimed = activity::reclaimed(c, lease.id)?;
    Ok(lease)
}

pub fn leases(c: &Connection) -> rusqlite::Result<Vec<Lease>> {
    let mut q = c.prepare("SELECT * FROM leases ORDER BY id")?;
    let rows: Vec<Lease> = q
        .query_map([], lease_row)?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter().map(|l| with_devices(c, l)).collect()
}

pub fn lease(c: &Connection, id: i64) -> rusqlite::Result<Option<Lease>> {
    c.query_row("SELECT * FROM leases WHERE id = ?1", [id], lease_row)
        .optional()?
        .map(|l| with_devices(c, l))
        .transpose()
}

/// The highest lease id ever granted; 0 before the first.
pub fn last_lease_id(c: &Connection) -> rusqlite::Result<i64> {
    Ok(c.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name = 'leases'",
        [],
        |row| row.get(0),
    )
    .optional()?
    .unwrap_or(0))
}

/// The devices lease `id` holds, in the order they were granted.
pub fn lease_devices(c: &Connection, id: i64) -> rusqlite::Result<Vec<LeaseDevice>> {
    let mut q = c.prepare(
        "SELECT serial, kind, abi, model FROM lease_devices WHERE lease_id = ?1 ORDER BY rowid",
    )?;
    q.query_map([id], |row| {
        Ok(LeaseDevice {
            serial: row.get(0)?,
            kind: row.get(1)?,
            abi: row.get(2)?,
            model: row.get(3)?,
            state: None,
        })
    })?
    .collect()
}

/// Every serial a lease holds.
pub fn taken(c: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut q = c.prepare("SELECT serial FROM lease_devices ORDER BY rowid")?;
    q.query_map([], |row| row.get(0))?.collect()
}

pub fn insert_lease_device(c: &Connection, lease: i64, d: &LeaseDevice) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO lease_devices (serial, lease_id, kind, abi, model) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![d.serial, lease, d.kind, d.abi, d.model],
    )?;
    Ok(())
}

/// Takes `serial` from lease `lease`; a device another lease holds now stays with it.
pub fn delete_held_device(c: &Connection, lease: i64, serial: &str) -> rusqlite::Result<()> {
    c.execute(
        "DELETE FROM lease_devices WHERE serial = ?1 AND lease_id = ?2",
        params![serial, lease],
    )?;
    Ok(())
}

pub fn delete_lease_device(c: &Connection, serial: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM lease_devices WHERE serial = ?1", [serial])?;
    Ok(())
}

pub fn set_abi(c: &Connection, serial: &str, abi: Option<&str>) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE lease_devices SET abi = ?2 WHERE serial = ?1",
        params![serial, abi],
    )?;
    Ok(())
}

pub fn emulators(c: &Connection) -> rusqlite::Result<Vec<EmulatorRow>> {
    let mut q = c.prepare("SELECT idx, serial, state, idle_since FROM emulators ORDER BY idx")?;
    q.query_map([], |row| {
        Ok(EmulatorRow {
            idx: row.get(0)?,
            serial: row.get(1)?,
            state: row.get(2)?,
            idle_since: row.get(3)?,
        })
    })?
    .collect()
}

pub fn insert_emulator(c: &Connection, idx: u32, serial: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO emulators (idx, serial, state) VALUES (?1, ?2, 'booting')",
        params![idx, serial],
    )?;
    Ok(())
}

/// Marks a booting emulator ready, idle since `idle_since` (none: a lease holds it).
pub fn set_emulator_ready(
    c: &Connection,
    idx: u32,
    idle_since: Option<i64>,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE emulators SET state = 'ready', idle_since = ?2 WHERE idx = ?1 AND state = 'booting'",
        params![idx, idle_since],
    )?;
    Ok(())
}

/// The emulator idles from `now`, unless a lease holds it.
pub fn emulator_idle(c: &Connection, serial: &str, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE emulators SET idle_since = ?2
         WHERE serial = ?1 AND serial NOT IN (SELECT serial FROM lease_devices)",
        params![serial, now],
    )?;
    Ok(())
}

/// A lease holds the emulator: it no longer idles.
pub fn emulator_held(c: &Connection, serial: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE emulators SET idle_since = NULL WHERE serial = ?1",
        [serial],
    )?;
    Ok(())
}

/// Forgets emulator `idx` if it is still in `state`: a row in another state is a newer
/// emulator at the same index.
pub fn delete_emulator(c: &Connection, idx: u32, state: &str) -> rusqlite::Result<()> {
    c.execute(
        "DELETE FROM emulators WHERE idx = ?1 AND state = ?2",
        params![idx, state],
    )?;
    Ok(())
}

pub fn slots(c: &Connection) -> rusqlite::Result<Vec<SlotRow>> {
    let mut q = c.prepare(
        "SELECT *, EXISTS (SELECT 1 FROM stoppers s WHERE s.project = slots.project
                            AND s.slot = slots.slot) AS stopping
         FROM slots ORDER BY project, slot",
    )?;
    q.query_map([], |row| {
        Ok(SlotRow {
            project: row.get("project")?,
            slot: row.get("slot")?,
            last_worktree: row.get("last_worktree")?,
            stack_ours: row.get("stack_ours")?,
            idle_since: row.get("idle_since")?,
            stopping: row.get("stopping")?,
        })
    })?
    .collect()
}

/// The requests still waiting, oldest first; not those granted and not yet answered.
pub fn waiters(c: &Connection) -> rusqlite::Result<Vec<Waiter>> {
    let mut q = c.prepare("SELECT * FROM waiters WHERE granted IS NULL ORDER BY id")?;
    q.query_map([], waiter_row)?.collect()
}

fn waiter_row(row: &Row<'_>) -> rusqlite::Result<Waiter> {
    Ok(Waiter {
        id: row.get("id")?,
        created: row.get("created")?,
        label: row.get("label")?,
        owner: row.get("owner")?,
        owner_label: row.get("owner_label")?,
        worktree: row.get("worktree")?,
        lease: row.get("lease")?,
        project: row.get("project")?,
        slot_cap: row.get("slot_cap")?,
        max_slots: row.get("max_slots")?,
        deadline_ms: row.get("deadline_ms")?,
        client_pid: row.get("client_pid")?,
        client_start: row.get("client_start")?,
    })
}

/// A request as its caller finds it again after losing the daemon.
pub struct Place {
    pub waiter: Waiter,
    /// What it was granted, when the daemon committed a grant it never got to send.
    pub granted: Option<Granted>,
}

/// The request this caller made before, by the place it names or else by the devlab call that
/// asks, if it is the same owner's in the same worktree. A grant whose lease has gone since goes
/// with it.
pub fn place(c: &Connection, p: &AcquireParams) -> rusqlite::Result<Option<Place>> {
    let row = |row: &Row<'_>| {
        Ok((
            waiter_row(row)?,
            row.get::<_, Option<i64>>("granted")?,
            row.get::<_, Option<String>>("previous")?,
        ))
    };
    let found = match (p.resume, &p.client) {
        (Some(id), _) => c
            .query_row("SELECT * FROM waiters WHERE id = ?1", [id], row)
            .optional()?,
        (None, Some(client)) => c
            .query_row(
                "SELECT * FROM waiters WHERE client_pid = ?1 AND client_start = ?2
                 ORDER BY id DESC LIMIT 1",
                params![client.pid, client.start],
                row,
            )
            .optional()?,
        (None, None) => None,
    };
    let Some((waiter, granted, previous)) =
        found.filter(|(w, _, _)| w.owner == p.owner.key() && w.worktree == p.worktree)
    else {
        return Ok(None);
    };
    let granted = match granted {
        Some(id) => match lease(c, id)? {
            Some(lease) => {
                let schema = match (&lease.project, lease.slot) {
                    (Some(project), Some(slot)) => slot_schema(c, project, slot)?,
                    _ => None,
                };
                Some(Granted {
                    lease,
                    previous,
                    schema,
                })
            }
            None => {
                delete_waiter(c, waiter.id)?;
                return Ok(None);
            }
        },
        None => None,
    };
    Ok(Some(Place { waiter, granted }))
}

/// Keeps the waiter, granted `lease`, until its answer is sent: a caller that never gets it
/// still finds it, and one that is gone has it given back.
pub fn mark_granted(
    c: &Connection,
    waiter: i64,
    lease: i64,
    previous: Option<&str>,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE waiters SET granted = ?2, previous = ?3 WHERE id = ?1",
        params![waiter, lease, previous],
    )?;
    Ok(())
}

/// The lease `forced` names when set; else this worktree's for this owner.
pub fn find_lease(
    c: &Connection,
    worktree: &str,
    owner: &Owner,
    forced: Option<i64>,
) -> rusqlite::Result<Option<Lease>> {
    if let Some(id) = forced.filter(|id| *id != 0) {
        return lease(c, id);
    }
    c.query_row(
        "SELECT * FROM leases WHERE worktree = ?1 AND owner_pid = ?2 AND owner_start = ?3",
        params![worktree, owner.pid, owner.start],
        lease_row,
    )
    .optional()?
    .map(|l| with_devices(c, l))
    .transpose()
}

/// Forgets a lease; its slot and its emulators start idling, its phones are free. A lease whose
/// worktree started the slot's stack records that worktree's newest migration as it ends, so
/// that one added during the lease counts. A run's lease made from a reservation
/// ([`end_with_run`]) gives that reservation its slot back, however the lease ends.
pub fn drop_lease(c: &Connection, lease: &Lease, now: i64) -> rusqlite::Result<()> {
    let from: Option<Option<String>> = c
        .query_row(
            "SELECT reservation FROM run_leases WHERE lease = ?1",
            [lease.id],
            |row| row.get(0),
        )
        .optional()?;
    let from = from
        .flatten()
        .map(|text| serde_json::from_str::<Reservation>(&text))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    c.execute(
        "UPDATE emulators SET idle_since = ?1
         WHERE serial IN (SELECT serial FROM lease_devices WHERE lease_id = ?2)",
        params![now, lease.id],
    )?;
    c.execute("DELETE FROM lease_devices WHERE lease_id = ?1", [lease.id])?;
    if let (Some(project), Some(slot)) = (&lease.project, lease.slot) {
        c.execute(
            "UPDATE slots SET idle_since = ?1 WHERE project = ?2 AND slot = ?3",
            params![now, project, slot],
        )?;
        let ran: bool = c.query_row(
            "SELECT EXISTS (SELECT 1 FROM slot_schemas
                            WHERE project = ?1 AND slot = ?2 AND worktree = ?3)",
            params![project, slot, lease.worktree],
            |row| row.get(0),
        )?;
        if ran {
            record_schema(c, project, slot, &lease.worktree)?;
        }
    }
    c.execute("DELETE FROM leases WHERE id = ?1", [lease.id])?;
    c.execute("DELETE FROM run_leases WHERE lease = ?1", [lease.id])?;
    activity::forget_lease(c, lease.id)?;
    if let Some(from) = &from {
        reservations::restore(c, from)?;
    }
    reservations::record_lease_end(c, lease, now)
}

/// Lease `lease` ends with run `run`: the run made it and didn't ask to keep it. `from` is the
/// reservation it was made from, which gets its slot back then.
pub fn end_with_run(
    c: &Connection,
    run: i64,
    lease: i64,
    from: Option<&Reservation>,
) -> rusqlite::Result<()> {
    let from = from
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    c.execute(
        "INSERT OR REPLACE INTO run_leases (run, lease, reservation) VALUES (?1, ?2, ?3)",
        params![run, lease, from],
    )?;
    Ok(())
}

/// Run `run` ended: the lease that ends with it ([`end_with_run`]) is dropped, and the
/// reservation it was made from, if any, holds its slot again, under the same id
/// ([`drop_lease`]). The lease dropped, if it was still there.
pub fn end_run_lease(c: &Connection, run: i64, now: i64) -> rusqlite::Result<Option<Lease>> {
    let ending: Option<i64> = c
        .query_row(
            "SELECT lease FROM run_leases WHERE run = ?1",
            [run],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = ending else {
        return Ok(None);
    };
    let Some(held) = lease(c, id)? else {
        c.execute("DELETE FROM run_leases WHERE run = ?1", [run])?;
        return Ok(None);
    };
    drop_lease(c, &held, now)?;
    Ok(Some(held))
}

/// Drops every lease and reservation whose owner exited: the leases as they were, with their
/// devices, and a line for each reservation.
pub fn gc(
    c: &Connection,
    now: i64,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<(Vec<Lease>, Vec<String>)> {
    let mut freed = Vec::new();
    for lease in leases(c)? {
        if !alive(lease.owner_pid, &lease.owner_start) {
            drop_lease(c, &lease, now)?;
            freed.push(lease);
        }
    }
    Ok((freed, reservations::gc(c, now, alive)?))
}

pub fn touch(c: &Connection, id: i64, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE leases SET touched = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    Ok(())
}

pub fn insert_waiter(c: &Connection, w: &Waiter) -> rusqlite::Result<i64> {
    c.execute(
        "INSERT INTO waiters (created, label, owner, owner_label, worktree, lease, project, slot_cap,
           max_slots, deadline_ms, client_pid, client_start)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            w.created,
            w.label,
            w.owner,
            w.owner_label,
            w.worktree,
            w.lease,
            w.project,
            w.slot_cap,
            w.max_slots,
            w.deadline_ms,
            w.client_pid,
            w.client_start
        ],
    )?;
    Ok(c.last_insert_rowid())
}

/// Drops the waiters no caller is attached to that can't come back: their devlab call exited
/// (or never said which it was), or their wait ran out `grace_ms` ago; how many it dropped. A
/// grant that never reached its caller goes once that call exits, its lease given back.
pub fn prune_waiters(
    c: &Connection,
    now_ms: i64,
    grace_ms: i64,
    attached: &dyn Fn(i64) -> bool,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<usize> {
    let mut q = c.prepare("SELECT * FROM waiters ORDER BY id")?;
    let rows: Vec<(Waiter, Option<i64>)> = q
        .query_map([], |row| Ok((waiter_row(row)?, row.get("granted")?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut pruned = 0;
    for (w, granted) in rows.iter().filter(|(w, _)| !attached(w.id)) {
        let gone = w
            .client_pid
            .is_none_or(|pid| !alive(pid, w.client_start.as_deref().unwrap_or_default()));
        let lapsed = granted.is_none()
            && w.deadline_ms
                .is_some_and(|deadline| deadline.saturating_add(grace_ms) < now_ms);
        if !(gone || lapsed) {
            continue;
        }
        if let Some(lease) = granted.map(|id| lease(c, id)).transpose()?.flatten() {
            drop_lease(c, &lease, now_ms / 1000)?;
        }
        if delete_waiter(c, w.id)? {
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Whether there was such a waiter.
pub fn delete_waiter(c: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(c.execute("DELETE FROM waiters WHERE id = ?1", [id])? > 0)
}

/// A new lease; its id is never one an earlier lease had.
pub fn insert_lease(
    c: &Connection,
    project: Option<&str>,
    worktree: &str,
    owner: &Owner,
    slot: Option<u32>,
    now: i64,
) -> rusqlite::Result<Lease> {
    c.execute(
        "INSERT INTO leases (project, worktree, owner_pid, owner_start, owner_label, slot, created, touched)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![project, worktree, owner.pid, owner.start, owner.label, slot, now],
    )?;
    Ok(Lease {
        id: c.last_insert_rowid(),
        project: project.map(str::to_owned),
        worktree: worktree.to_owned(),
        owner_pid: owner.pid,
        owner_start: owner.start.clone(),
        owner_label: owner.label.clone(),
        slot,
        created: now,
        touched: now,
        devices: Vec::new(),
        reclaimed: Vec::new(),
    })
}

/// Marks a slot held by `worktree`; returns the worktree of the lease that held it last, whether
/// or not that lease ran its stack (see [`slot_schema`] for the schema it holds).
pub fn claim_slot(
    c: &Connection,
    project: &str,
    slot: u32,
    worktree: &str,
) -> rusqlite::Result<Option<String>> {
    c.execute(
        "INSERT INTO slots (project, slot) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
        params![project, slot],
    )?;
    let previous: Option<String> = c.query_row(
        "SELECT last_worktree FROM slots WHERE project = ?1 AND slot = ?2",
        params![project, slot],
        |row| row.get(0),
    )?;
    c.execute(
        "UPDATE slots SET idle_since = NULL, last_worktree = ?3 WHERE project = ?1 AND slot = ?2",
        params![project, slot, worktree],
    )?;
    Ok(previous)
}

/// Holds the slot for `stopper` while it stops the slot's stack, beside any other stopper's
/// hold: the slot stays held until the last of them lets go.
pub fn mark_stopping(
    c: &Connection,
    project: &str,
    slot: u32,
    stopper: &Process,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO stoppers (project, slot, pid, start) VALUES (?1, ?2, ?3, ?4)",
        params![project, slot, stopper.pid, stopper.start],
    )?;
    Ok(())
}

/// Lets go of the slots `stopper` held; how many it held.
pub fn clear_stopping(c: &Connection, stopper: &Process) -> rusqlite::Result<usize> {
    c.execute(
        "DELETE FROM stoppers WHERE pid = ?1 AND start = ?2",
        params![stopper.pid, stopper.start],
    )
}

/// Lets go of the slots held by stoppers that exited; how many there were.
pub fn clear_dead_stoppers(
    c: &Connection,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<usize> {
    let mut q = c.prepare("SELECT DISTINCT pid, start FROM stoppers")?;
    let stoppers: Vec<Process> = q
        .query_map([], |row| {
            Ok(Process {
                pid: row.get(0)?,
                start: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut cleared = 0;
    for stopper in stoppers.iter().filter(|s| !alive(s.pid, &s.start)) {
        cleared += clear_stopping(c, stopper)?;
    }
    Ok(cleared)
}

/// Marks the slot's stack as devlab's to stop, or not. A stack devlab starts runs the schema
/// of the worktree whose lease holds the slot: that worktree is recorded as the slot's schema
/// (see [`slot_schema`]), and stays so when the stack stops or another lease holds the slot.
pub fn set_stack_ours(
    c: &Connection,
    project: &str,
    slot: u32,
    ours: bool,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE slots SET stack_ours = ?3 WHERE project = ?1 AND slot = ?2",
        params![project, slot, ours],
    )?;
    let holder: Option<String> = c
        .query_row(
            "SELECT worktree FROM leases WHERE project = ?1 AND slot = ?2",
            params![project, slot],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(worktree) = holder.filter(|_| ours) {
        record_schema(c, project, slot, &worktree)?;
    }
    Ok(())
}

/// The schema the slot's stack last ran: the worktree whose lease last started it, and that
/// worktree's newest migration when that lease ended. None when devlab never ran the slot's
/// stack.
pub fn slot_schema(
    c: &Connection,
    project: &str,
    slot: u32,
) -> rusqlite::Result<Option<SlotSchema>> {
    c.query_row(
        "SELECT worktree, head FROM slot_schemas WHERE project = ?1 AND slot = ?2",
        params![project, slot],
        |row| {
            Ok(SlotSchema {
                worktree: row.get(0)?,
                head: row.get(1)?,
            })
        },
    )
    .optional()
}

/// Records `worktree`'s schema, up to its newest migration as its files show it now, as the
/// slot's. Reading them is no external command: it may run in the transaction.
fn record_schema(c: &Connection, project: &str, slot: u32, worktree: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO slot_schemas (project, slot, worktree, head) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (project, slot) DO UPDATE SET worktree = excluded.worktree, head = excluded.head",
        params![project, slot, worktree, stack::migration_head(worktree)],
    )?;
    Ok(())
}

/// A `devlab run` the daemon follows, kept across its restarts: what its `lab-run` event
/// needs, and the devlab process that runs it. Its id is the waiter it first queued as; times are
/// milliseconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: i64,
    pub worktree: String,
    pub command: String,
    pub session: Option<String>,
    pub client: Process,
    pub queued_ms: i64,
    pub granted_ms: Option<i64>,
    pub started_ms: Option<i64>,
    pub lease: Option<i64>,
    pub slot: Option<u32>,
}

fn run_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: row.get("id")?,
        worktree: row.get("worktree")?,
        command: row.get("command")?,
        session: row.get("session")?,
        client: Process {
            pid: row.get("client_pid")?,
            start: row.get("client_start")?,
        },
        queued_ms: row.get("queued_ms")?,
        granted_ms: row.get("granted_ms")?,
        started_ms: row.get("started_ms")?,
        lease: row.get("lease")?,
        slot: row.get("slot")?,
    })
}

/// The run `fresh` describes, as it was kept: a devlab process runs one run, so one it queued
/// for before (under another waiter, if its place was lost) is the same run, now under
/// `fresh.id`. Else `fresh`, kept from now on.
pub fn begin_run(c: &Connection, fresh: &RunRow) -> rusqlite::Result<RunRow> {
    let kept = c
        .query_row(
            "SELECT * FROM runs WHERE id = ?1 OR (client_pid = ?2 AND client_start = ?3)
             ORDER BY id = ?1 DESC, id DESC LIMIT 1",
            params![fresh.id, fresh.client.pid, fresh.client.start],
            run_row,
        )
        .optional()?;
    if let Some(mut kept) = kept {
        if kept.id != fresh.id {
            c.execute(
                "UPDATE runs SET id = ?1 WHERE id = ?2",
                params![fresh.id, kept.id],
            )?;
            c.execute(
                "UPDATE run_leases SET run = ?1 WHERE run = ?2",
                params![fresh.id, kept.id],
            )?;
            kept.id = fresh.id;
        }
        return Ok(kept);
    }
    c.execute(
        "INSERT INTO runs (id, worktree, command, session, client_pid, client_start, queued_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            fresh.id,
            fresh.worktree,
            fresh.command,
            fresh.session,
            fresh.client.pid,
            fresh.client.start,
            fresh.queued_ms
        ],
    )?;
    Ok(fresh.clone())
}

/// The run devlab process `client` runs, if the daemon keeps one.
pub fn run_of(c: &Connection, client: &Process) -> rusqlite::Result<Option<RunRow>> {
    c.query_row(
        "SELECT * FROM runs WHERE client_pid = ?1 AND client_start = ?2",
        params![client.pid, client.start],
        run_row,
    )
    .optional()
}

/// Marks the run granted `lease` at `ms`, unless it was granted before.
pub fn run_granted(
    c: &Connection,
    id: i64,
    ms: i64,
    lease: i64,
    slot: Option<u32>,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE runs SET granted_ms = COALESCE(granted_ms, ?2), lease = ?3, slot = ?4 WHERE id = ?1",
        params![id, ms, lease, slot],
    )?;
    Ok(())
}

/// Marks the run's command started at `ms`, unless it started before.
pub fn run_started(c: &Connection, id: i64, ms: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE runs SET started_ms = COALESCE(started_ms, ?2) WHERE id = ?1",
        params![id, ms],
    )?;
    Ok(())
}

/// Takes the run out of the store, to record its end: none when it was taken already.
pub fn take_run(c: &Connection, id: i64) -> rusqlite::Result<Option<RunRow>> {
    let run = c
        .query_row("SELECT * FROM runs WHERE id = ?1", [id], run_row)
        .optional()?;
    if run.is_some() {
        c.execute("DELETE FROM runs WHERE id = ?1", [id])?;
    }
    Ok(run)
}

/// Takes out the runs no daemon can follow any more: no connection follows them (`attached`)
/// and their devlab process is gone, so it never comes back to say how they ended.
pub fn take_orphan_runs(
    c: &Connection,
    attached: &dyn Fn(i64) -> bool,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<Vec<RunRow>> {
    let mut q = c.prepare("SELECT * FROM runs ORDER BY id")?;
    let rows: Vec<RunRow> = q.query_map([], run_row)?.collect::<rusqlite::Result<_>>()?;
    let mut orphans = Vec::new();
    for run in rows {
        if attached(run.id) || alive(run.client.pid, &run.client.start) {
            continue;
        }
        c.execute("DELETE FROM runs WHERE id = ?1", [run.id])?;
        orphans.push(run);
    }
    Ok(orphans)
}
