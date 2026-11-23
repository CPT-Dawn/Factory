//! The lease store: `lab.db`, written by the daemon alone. Every write is one short
//! `BEGIN IMMEDIATE` transaction, and no external command runs while one is open.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::reservations;
use super::wire::{Lease, LeaseDevice, Owner, SlotRow, Waiter};

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
CREATE TABLE IF NOT EXISTS waiters (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  created INTEGER NOT NULL,
  label TEXT NOT NULL,
  owner TEXT NOT NULL,
  owner_label TEXT NOT NULL,
  project TEXT,
  slot_cap INTEGER,
  max_slots INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS lease_devices (
  serial TEXT PRIMARY KEY,
  lease_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  abi TEXT,
  model TEXT NOT NULL DEFAULT ''
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

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the store. Waiters are forgotten: their connections ended with the
    /// daemon that queued them.
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(15))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(reservations::SCHEMA)?;
        conn.execute("DELETE FROM waiters", [])?;
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
    })
}

/// The lease with the devices it holds.
fn with_devices(c: &Connection, mut lease: Lease) -> rusqlite::Result<Lease> {
    lease.devices = lease_devices(c, lease.id)?;
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
    let mut q = c.prepare("SELECT * FROM slots ORDER BY project, slot")?;
    q.query_map([], |row| {
        Ok(SlotRow {
            project: row.get("project")?,
            slot: row.get("slot")?,
            last_worktree: row.get("last_worktree")?,
            stack_ours: row.get("stack_ours")?,
            idle_since: row.get("idle_since")?,
        })
    })?
    .collect()
}

pub fn waiters(c: &Connection) -> rusqlite::Result<Vec<Waiter>> {
    let mut q = c.prepare("SELECT * FROM waiters ORDER BY id")?;
    q.query_map([], |row| {
        Ok(Waiter {
            id: row.get("id")?,
            created: row.get("created")?,
            label: row.get("label")?,
            owner: row.get("owner")?,
            owner_label: row.get("owner_label")?,
            project: row.get("project")?,
            slot_cap: row.get("slot_cap")?,
            max_slots: row.get("max_slots")?,
        })
    })?
    .collect()
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

/// Forgets a lease; its slot and its emulators start idling, its phones are free.
pub fn drop_lease(c: &Connection, lease: &Lease, now: i64) -> rusqlite::Result<()> {
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
    }
    c.execute("DELETE FROM leases WHERE id = ?1", [lease.id])?;
    reservations::record_lease_end(c, lease, now)
}

/// Drops every lease whose owner exited; a line per lease dropped.
pub fn gc(
    c: &Connection,
    now: i64,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<Vec<String>> {
    let mut freed = Vec::new();
    for lease in leases(c)? {
        if !alive(lease.owner_pid, &lease.owner_start) {
            drop_lease(c, &lease, now)?;
            freed.push(format!(
                "lease {} ({}, owner exited)",
                lease.id, lease.owner_label
            ));
        }
    }
    freed.extend(reservations::gc(c, now, alive)?);
    Ok(freed)
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
        "INSERT INTO waiters (created, label, owner, owner_label, project, slot_cap, max_slots)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            w.created,
            w.label,
            w.owner,
            w.owner_label,
            w.project,
            w.slot_cap,
            w.max_slots
        ],
    )?;
    Ok(c.last_insert_rowid())
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
    })
}

/// Marks a slot held by `worktree`; returns the worktree it ran before.
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
    Ok(())
}
