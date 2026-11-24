//! The reservations table of `lab.db`, and the history of how long leases ran, which an
//! over-cap owner's ETA is read from. Written by the daemon alone, in the lab's short transactions.

use rusqlite::{Connection, Row, params};

use factory_core::lab::reserve::{UNUSED_SECS, expired};

use super::store;
use super::wire::{Lease, Owner, Reservation};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reservations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project TEXT NOT NULL,
  slot INTEGER NOT NULL,
  worktree TEXT NOT NULL,
  owner_pid INTEGER NOT NULL,
  owner_start TEXT NOT NULL,
  owner_label TEXT NOT NULL,
  created INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS reservations_slot ON reservations (project, slot);
CREATE TABLE IF NOT EXISTS reservation_uses (
  reservation INTEGER PRIMARY KEY,
  used INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS lease_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project TEXT NOT NULL,
  seconds INTEGER NOT NULL
);
";

/// How many of a project's latest leases its ETA is read from.
const HISTORY: i64 = 20;

fn row(row: &Row<'_>) -> rusqlite::Result<Reservation> {
    Ok(Reservation {
        id: row.get("id")?,
        project: row.get("project")?,
        slot: row.get("slot")?,
        worktree: row.get("worktree")?,
        owner_pid: row.get("owner_pid")?,
        owner_start: row.get("owner_start")?,
        owner_label: row.get("owner_label")?,
        created: row.get("created")?,
        used: row.get("used")?,
    })
}

/// Every reservation, with when a run last gave its slot back.
pub fn all(c: &Connection) -> rusqlite::Result<Vec<Reservation>> {
    let mut q = c.prepare(
        "SELECT r.*, u.used FROM reservations r
         LEFT JOIN reservation_uses u ON u.reservation = r.id ORDER BY r.id",
    )?;
    q.query_map([], row)?.collect()
}

/// A new reservation of `slot`; its id is never one an earlier reservation had.
pub fn insert(
    c: &Connection,
    project: &str,
    slot: u32,
    worktree: &str,
    owner: &Owner,
    now: i64,
) -> rusqlite::Result<Reservation> {
    c.execute(
        "INSERT INTO reservations (project, slot, worktree, owner_pid, owner_start, owner_label, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![project, slot, worktree, owner.pid, owner.start, owner.label, now],
    )?;
    // The slot is spoken for: it does not idle.
    c.execute(
        "UPDATE slots SET idle_since = NULL WHERE project = ?1 AND slot = ?2",
        params![project, slot],
    )?;
    Ok(Reservation {
        id: c.last_insert_rowid(),
        project: project.to_owned(),
        slot,
        worktree: worktree.to_owned(),
        owner_pid: owner.pid,
        owner_start: owner.start.clone(),
        owner_label: owner.label.clone(),
        created: now,
        used: None,
    })
}

/// Forgets a reservation; its slot starts idling.
fn drop_reservation(c: &Connection, r: &Reservation, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE slots SET idle_since = ?1 WHERE project = ?2 AND slot = ?3",
        params![now, r.project, r.slot],
    )?;
    forget(c, r.id)
}

/// Deletes reservation `id` and when it was last used.
fn forget(c: &Connection, id: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM reservations WHERE id = ?1", [id])?;
    c.execute("DELETE FROM reservation_uses WHERE reservation = ?1", [id])?;
    Ok(())
}

/// Gives the owner's reservations of `project` back; how many there were.
pub fn release(c: &Connection, owner: &Owner, project: &str, now: i64) -> rusqlite::Result<usize> {
    let mine: Vec<Reservation> = all(c)?
        .into_iter()
        .filter(|r| r.project == project && r.owner_key() == owner.key())
        .collect();
    for r in &mine {
        drop_reservation(c, r, now)?;
    }
    Ok(mine.len())
}

/// Gives one reservation back, when it is still there.
pub fn drop_by_id(c: &Connection, id: i64, now: i64) -> rusqlite::Result<()> {
    if let Some(r) = all(c)?.into_iter().find(|r| r.id == id) {
        drop_reservation(c, &r, now)?;
    }
    Ok(())
}

/// Drops every reservation whose owner exited, or that nobody used for two hours since it was
/// made or last used ([`expired`]); a line per reservation dropped.
pub fn gc(
    c: &Connection,
    now: i64,
    alive: &dyn Fn(i64, &str) -> bool,
) -> rusqlite::Result<Vec<String>> {
    let mut freed = Vec::new();
    for r in all(c)? {
        let why = if !alive(r.owner_pid, &r.owner_start) {
            "owner exited".to_owned()
        } else if expired(r.created, r.used, now) {
            format!("unused for {} h", UNUSED_SECS / 3600)
        } else {
            continue;
        };
        drop_reservation(c, &r, now)?;
        freed.push(format!("reservation {} ({}, {why})", r.id, r.owner_label));
    }
    Ok(freed)
}

/// Turns reservation `id` of `project` into a lease of `worktree` for `owner`, when there is
/// such a reservation: the slot it held, held by the new lease now, and the reservation as it
/// was. The caller claims the slot.
pub fn claim(
    c: &Connection,
    id: i64,
    project: &str,
    worktree: &str,
    owner: &Owner,
    now: i64,
) -> rusqlite::Result<Option<(Lease, Reservation)>> {
    let Some(r) = all(c)?
        .into_iter()
        .find(|r| r.id == id && r.project == project)
    else {
        return Ok(None);
    };
    forget(c, r.id)?;
    let lease = store::insert_lease(c, Some(project), worktree, owner, Some(r.slot), now)?;
    Ok(Some((lease, r)))
}

/// Puts back reservation `r` that a lease was made from, under its own id, once that lease
/// ended: its slot is spoken for again, and it was last used `now`. A slot a lease or another
/// reservation holds by then stays as it is.
pub fn restore(c: &Connection, r: &Reservation, now: i64) -> rusqlite::Result<()> {
    let put = c.execute(
        "INSERT OR IGNORE INTO reservations
           (id, project, slot, worktree, owner_pid, owner_start, owner_label, created)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
         WHERE NOT EXISTS (SELECT 1 FROM leases WHERE project = ?2 AND slot = ?3)",
        params![
            r.id,
            r.project,
            r.slot,
            r.worktree,
            r.owner_pid,
            r.owner_start,
            r.owner_label,
            r.created
        ],
    )?;
    if put > 0 {
        c.execute(
            "UPDATE slots SET idle_since = NULL WHERE project = ?1 AND slot = ?2",
            params![r.project, r.slot],
        )?;
        c.execute(
            "INSERT OR REPLACE INTO reservation_uses (reservation, used) VALUES (?1, ?2)",
            params![r.id, now],
        )?;
    }
    Ok(())
}

/// Records how long a lease ran, for the ETAs of its project.
pub fn record_lease_end(c: &Connection, lease: &Lease, now: i64) -> rusqlite::Result<()> {
    if let (Some(project), Some(_)) = (&lease.project, lease.slot) {
        c.execute(
            "INSERT INTO lease_history (project, seconds) VALUES (?1, ?2)",
            params![project, (now - lease.created).max(0)],
        )?;
    }
    Ok(())
}

/// How long the project's latest leases ran, in seconds.
pub fn durations(c: &Connection, project: &str) -> rusqlite::Result<Vec<i64>> {
    let mut q = c.prepare(
        "SELECT seconds FROM lease_history WHERE project = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    q.query_map(params![project, HISTORY], |row| row.get(0))?
        .collect()
}
