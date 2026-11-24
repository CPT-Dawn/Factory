//! What each lease does, for the idle reclaim (#57): the device asks of the waiters short of
//! devices, the `devlab exec` calls running on a lease, and the devices waiters took from leases
//! that ran nothing. And what each waiter asks of devices as its last look found it, for the
//! queue (#58). Written by the daemon alone, in the lab's short transactions.

use std::collections::BTreeMap;

use factory_core::lab::devices::Mode;
use factory_core::lab::queue::DeviceNeed;
use rusqlite::{Connection, OptionalExtension, params};

use super::events;
use super::wire::{Process, ReclaimedDevice};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS waiter_devices (
  waiter INTEGER PRIMARY KEY,
  mode TEXT NOT NULL,
  want INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS waiter_asks (
  waiter INTEGER PRIMARY KEY,
  mode TEXT NOT NULL,
  want INTEGER NOT NULL,
  lacks INTEGER NOT NULL,
  cap INTEGER NOT NULL,
  short INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS execs (
  pid INTEGER NOT NULL,
  start TEXT NOT NULL,
  lease INTEGER NOT NULL,
  command TEXT NOT NULL,
  PRIMARY KEY (pid, start)
);
CREATE TABLE IF NOT EXISTS reclaimed (
  lease INTEGER NOT NULL,
  serial TEXT NOT NULL,
  kind TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  idle_min INTEGER NOT NULL,
  PRIMARY KEY (lease, serial)
);
";

/// Records what waiter `waiter` is short of, `(mode, want)`, or that it is short of no device;
/// true when it wasn't short of any before.
pub fn set_need(c: &Connection, waiter: i64, need: Option<(&str, u32)>) -> rusqlite::Result<bool> {
    let had: bool = c.query_row(
        "SELECT EXISTS (SELECT 1 FROM waiter_devices WHERE waiter = ?1)",
        [waiter],
        |row| row.get(0),
    )?;
    match need {
        Some((mode, want)) => {
            c.execute(
                "INSERT OR REPLACE INTO waiter_devices (waiter, mode, want) VALUES (?1, ?2, ?3)",
                params![waiter, mode, want],
            )?;
            Ok(!had)
        }
        None => {
            c.execute("DELETE FROM waiter_devices WHERE waiter = ?1", [waiter])?;
            Ok(false)
        }
    }
}

/// What the waiters still waiting are short of, as `(mode, want)`. The asks of waiters that
/// are gone or were granted are forgotten.
pub fn needs(c: &Connection) -> rusqlite::Result<Vec<(String, u32)>> {
    c.execute(
        "DELETE FROM waiter_devices
         WHERE waiter NOT IN (SELECT id FROM waiters WHERE granted IS NULL)",
        [],
    )?;
    let mut q = c.prepare("SELECT mode, want FROM waiter_devices ORDER BY waiter")?;
    q.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// Forgets all that was recorded of waiter `waiter`: a new waiter may get the id of one gone.
pub fn forget_waiter(c: &Connection, waiter: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM waiter_devices WHERE waiter = ?1", [waiter])?;
    c.execute("DELETE FROM waiter_asks WHERE waiter = ?1", [waiter])?;
    Ok(())
}

/// Records what waiter `waiter` asks of devices as its look found it (none: no device); true
/// when that changed, so the waiters behind it should look again.
pub fn set_ask(c: &Connection, waiter: i64, ask: Option<DeviceNeed>) -> rusqlite::Result<bool> {
    let had = c
        .query_row(
            "SELECT * FROM waiter_asks WHERE waiter = ?1",
            [waiter],
            ask_row,
        )
        .optional()?;
    if had == ask {
        return Ok(false);
    }
    match ask {
        Some(d) => c.execute(
            "INSERT OR REPLACE INTO waiter_asks (waiter, mode, want, lacks, cap, short)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![waiter, d.mode.as_str(), d.want, d.lacks, d.cap, d.short],
        )?,
        None => c.execute("DELETE FROM waiter_asks WHERE waiter = ?1", [waiter])?,
    };
    Ok(true)
}

/// What each waiter still waiting asks of devices, by waiter. The asks of waiters that are gone
/// or were granted are forgotten.
pub fn asks(c: &Connection) -> rusqlite::Result<BTreeMap<i64, DeviceNeed>> {
    c.execute(
        "DELETE FROM waiter_asks
         WHERE waiter NOT IN (SELECT id FROM waiters WHERE granted IS NULL)",
        [],
    )?;
    let mut q = c.prepare("SELECT * FROM waiter_asks")?;
    q.query_map([], |row| Ok((row.get("waiter")?, ask_row(row)?)))?
        .collect()
}

fn ask_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceNeed> {
    let mode: String = row.get("mode")?;
    Ok(DeviceNeed {
        mode: Mode::from_word(&mode),
        want: row.get("want")?,
        lacks: row.get("lacks")?,
        cap: row.get("cap")?,
        short: row.get("short")?,
    })
}

/// One `devlab exec` running on a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub client: Process,
    pub lease: i64,
    pub command: String,
}

pub fn begin_exec(c: &Connection, e: &Exec) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO execs (pid, start, lease, command) VALUES (?1, ?2, ?3, ?4)",
        params![e.client.pid, e.client.start, e.lease, e.command],
    )?;
    events::commands_changed(c, e.lease)
}

/// Ends the exec of `client`; the lease it ran on, if it was counted.
pub fn end_exec(c: &Connection, client: &Process) -> rusqlite::Result<Option<i64>> {
    let mut q = c.prepare("DELETE FROM execs WHERE pid = ?1 AND start = ?2 RETURNING lease")?;
    let lease: Option<i64> = q
        .query_map(params![client.pid, client.start], |row| row.get(0))?
        .next()
        .transpose()?;
    if let Some(lease) = lease {
        events::commands_changed(c, lease)?;
    }
    Ok(lease)
}

/// The execs still running, oldest first; those whose process exited are forgotten.
pub fn execs(c: &Connection, alive: &dyn Fn(i64, &str) -> bool) -> rusqlite::Result<Vec<Exec>> {
    let mut q = c.prepare("SELECT pid, start, lease, command FROM execs ORDER BY rowid")?;
    let all: Vec<Exec> = q
        .query_map([], |row| {
            Ok(Exec {
                client: Process {
                    pid: row.get(0)?,
                    start: row.get(1)?,
                },
                lease: row.get(2)?,
                command: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let (live, dead): (Vec<Exec>, Vec<Exec>) = all
        .into_iter()
        .partition(|e| alive(e.client.pid, &e.client.start));
    for e in &dead {
        end_exec(c, &e.client)?;
    }
    Ok(live)
}

/// The command each lease runs: its open `devlab run`, else its oldest exec.
pub fn running(
    c: &Connection,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut q = c.prepare("SELECT lease, command FROM runs WHERE lease IS NOT NULL ORDER BY id")?;
    let mut running: Vec<(i64, String)> = q
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for e in execs(c, alive)? {
        if !running.iter().any(|(lease, _)| *lease == e.lease) {
            running.push((e.lease, e.command));
        }
    }
    running.sort_by_key(|(lease, _)| *lease);
    running.dedup_by_key(|(lease, _)| *lease);
    Ok(running)
}

/// Takes `serial` from lease `lease`, which ran nothing for `idle_min` minutes, and keeps why;
/// the device's kind, or none when the lease no longer held it.
pub fn reclaim(
    c: &Connection,
    lease: i64,
    serial: &str,
    idle_min: i64,
) -> rusqlite::Result<Option<String>> {
    let taken = c.execute(
        "INSERT OR REPLACE INTO reclaimed (lease, serial, kind, model, idle_min)
         SELECT lease_id, serial, kind, model, ?3 FROM lease_devices
         WHERE lease_id = ?1 AND serial = ?2",
        params![lease, serial, idle_min],
    )?;
    if taken == 0 {
        return Ok(None);
    }
    let kind: String = c.query_row(
        "SELECT kind FROM reclaimed WHERE lease = ?1 AND serial = ?2",
        params![lease, serial],
        |row| row.get(0),
    )?;
    c.execute(
        "DELETE FROM lease_devices WHERE lease_id = ?1 AND serial = ?2",
        params![lease, serial],
    )?;
    Ok(Some(kind))
}

/// The devices waiters took from lease `lease`, in the order they were taken.
pub fn reclaimed(c: &Connection, lease: i64) -> rusqlite::Result<Vec<ReclaimedDevice>> {
    let mut q = c.prepare(
        "SELECT serial, kind, model, idle_min FROM reclaimed WHERE lease = ?1 ORDER BY rowid",
    )?;
    q.query_map([lease], |row| {
        Ok(ReclaimedDevice {
            serial: row.get(0)?,
            kind: row.get(1)?,
            model: row.get(2)?,
            idle_min: row.get(3)?,
        })
    })?
    .collect()
}

/// Lease `lease` was granted devices again: what was taken from it is no longer why it fails.
pub fn clear_reclaimed(c: &Connection, lease: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM reclaimed WHERE lease = ?1", [lease])?;
    Ok(())
}

/// Lease `lease` ended: its execs and what was taken from it go with it.
pub fn forget_lease(c: &Connection, lease: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM execs WHERE lease = ?1", [lease])?;
    clear_reclaimed(c, lease)
}
