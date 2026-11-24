//! The lab's events on their way to the stats ledger (#62). Each is queued in `lab.db` by the
//! transaction that makes it true, so one rolled back is never told, and the ledger's thread
//! appends the queue once it commits ([`drain`]). And each lease's clock: when
//! it began, and how long commands have run on it, for its `idle_seconds`.

use std::sync::LazyLock;

use factory_core::event::{Event, Fields};
use factory_core::lab::base_name;
use factory_core::lab::events::{self as event, LeaseEnd, LeaseFacts, spans};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use super::service::now_ms;
use super::store;
use super::wire::Lease;
use crate::ledger::Ledger;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lab_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  fields TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS wait_reasons (
  waiter INTEGER PRIMARY KEY,
  why TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS lease_clocks (
  lease INTEGER PRIMARY KEY,
  start_ms INTEGER NOT NULL,
  busy_ms INTEGER NOT NULL DEFAULT 0,
  busy_since INTEGER,
  reservation INTEGER
);
";

/// Queues event `kind` with `fields` for the ledger.
pub fn queue(c: &Connection, kind: &str, fields: &Fields) -> rusqlite::Result<()> {
    let text = Value::Object(fields.clone()).to_string();
    c.execute(
        "INSERT INTO lab_events (kind, fields) VALUES (?1, ?2)",
        params![kind, text],
    )?;
    Ok(())
}

/// Appends the queued events to `ledger`, oldest first, and then forgets them: a daemon that
/// stops before forgetting them appends them again (once more, at worst) from its next start,
/// and none is lost. Run on the ledger's own thread and connection, outside any lab call. One
/// the ledger won't take costs a log line ([`append`]) and is forgotten.
pub fn drain(c: &Connection, ledger: &Ledger) -> rusqlite::Result<()> {
    let mut q = c.prepare("SELECT id, kind, fields FROM lab_events ORDER BY id")?;
    let rows: Vec<(i64, String, String)> = q
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let Some(last) = rows.last().map(|(id, _, _)| *id) else {
        return Ok(());
    };
    for (_, kind, text) in rows {
        if let Ok(Value::Object(fields)) = serde_json::from_str(&text) {
            append(ledger, &kind, fields);
        }
    }
    c.execute("DELETE FROM lab_events WHERE id <= ?1", [last])?;
    Ok(())
}

/// Appends one lab event to the ledger. One that can't be written costs a log line and
/// nothing else: no lab command fails with the ledger.
fn append(ledger: &Ledger, kind: &str, fields: Fields) {
    let landed = Event::new(kind, fields).map(|event| ledger.append(&event));
    if !matches!(landed, Ok(Ok(_))) {
        eprintln!("factory: lab: cannot write a {kind} event to the ledger");
    }
}

/// The names a worktree may not be shown by: the user's, and their home dir's, from the
/// environment and from `/etc/passwd`, so a daemon started without `HOME` hides them too.
static PRIVATE: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut names: Vec<String> = ["USER", "LOGNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .collect();
    let mut homes: Vec<String> = std::env::var("HOME").ok().into_iter().collect();
    let uid = rustix::process::getuid().as_raw().to_string();
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for entry in passwd
            .lines()
            .map(|line| line.split(':').collect::<Vec<_>>())
        {
            if entry.get(2) == Some(&uid.as_str()) {
                names.extend(entry.first().map(|name| (*name).to_owned()));
                homes.extend(entry.get(5).map(|home| (*home).to_owned()));
            }
        }
    }
    names.extend(homes.iter().map(|home| base_name(home).to_owned()));
    names.retain(|name| !name.is_empty() && name != "/");
    names.sort();
    names.dedup();
    names
});

/// What the ledger says of lease `lease` as the store holds it now, with `booting` more
/// emulators on their way to it.
fn facts(c: &Connection, lease: &Lease, booting: u32) -> rusqlite::Result<LeaseFacts> {
    let reservation: Option<i64> = c
        .query_row(
            "SELECT reservation FROM lease_clocks WHERE lease = ?1",
            [lease.id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let mut devices: Vec<(String, String)> = store::lease_devices(c, lease.id)?
        .into_iter()
        .map(|d| (d.kind, d.model))
        .collect();
    // An emulator still booting is held as the daemon's emulators are, once it is up.
    devices.extend((0..booting).map(|_| ("emulator".to_owned(), "emulator".to_owned())));
    Ok(LeaseFacts {
        id: lease.id,
        project: lease.project.clone(),
        owner_label: lease.owner_label.clone(),
        worktree: lease.worktree.clone(),
        private: PRIVATE.clone(),
        devices,
        slot: lease.slot,
        reservation,
    })
}

/// Lease `id` was granted, made from reservation `reservation` if one, with `booting`
/// emulators still to boot: its clock starts and its `lab-lease start` is queued.
pub fn lease_began(
    c: &Connection,
    id: i64,
    reservation: Option<i64>,
    booting: u32,
) -> rusqlite::Result<()> {
    let Some(lease) = store::lease(c, id)? else {
        return Ok(());
    };
    c.execute(
        "INSERT OR REPLACE INTO lease_clocks (lease, start_ms, reservation) VALUES (?1, ?2, ?3)",
        params![id, now_ms(), reservation],
    )?;
    queue(
        c,
        "lab-lease",
        &event::lease_start(&facts(c, &lease, booting)?),
    )
}

/// Lease `lease` ends for `reason`: its `lab-lease end` is queued and its clock forgotten.
/// Called before its devices are let go.
pub fn lease_ended(c: &Connection, lease: &Lease, reason: LeaseEnd) -> rusqlite::Result<()> {
    let now = now_ms();
    let clock: Option<(i64, i64, Option<i64>)> = c
        .query_row(
            "SELECT start_ms, busy_ms, busy_since FROM lease_clocks WHERE lease = ?1",
            [lease.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    // A lease granted before its clock was kept began when it was made, and ran nothing known.
    let (start, busy, since) = clock.unwrap_or((lease.created.saturating_mul(1000), 0, None));
    let (seconds, idle) = spans(start, busy, since, now);
    let fields = event::lease_end(&facts(c, lease, 0)?, seconds, idle, reason);
    queue(c, "lab-lease", &fields)?;
    c.execute("DELETE FROM lease_clocks WHERE lease = ?1", [lease.id])?;
    Ok(())
}

/// Brings lease `lease`'s clock up to date with the commands running on it now: its execs and
/// its started run. Called after each starts or ends.
pub fn commands_changed(c: &Connection, lease: i64) -> rusqlite::Result<()> {
    // A lease granted before clocks were kept gets one from when it was made.
    c.execute(
        "INSERT OR IGNORE INTO lease_clocks (lease, start_ms)
         SELECT id, created * 1000 FROM leases WHERE id = ?1",
        [lease],
    )?;
    let running: i64 = c.query_row(
        "SELECT (SELECT COUNT(*) FROM execs WHERE lease = ?1)
              + (SELECT COUNT(*) FROM runs WHERE lease = ?1 AND started_ms IS NOT NULL)",
        [lease],
        |row| row.get(0),
    )?;
    let now = now_ms();
    if running > 0 {
        c.execute(
            "UPDATE lease_clocks SET busy_since = COALESCE(busy_since, ?2) WHERE lease = ?1",
            params![lease, now],
        )?;
    } else {
        c.execute(
            "UPDATE lease_clocks SET busy_ms = busy_ms + MAX(0, ?2 - busy_since), busy_since = NULL
             WHERE lease = ?1 AND busy_since IS NOT NULL",
            params![lease, now],
        )?;
    }
    Ok(())
}

/// Keeps `why` as what waiter `waiter` waits for, so its `lab-wait` can say it, even from a
/// daemon that comes back after a restart.
pub fn waiting(c: &Connection, waiter: i64, why: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO wait_reasons (waiter, why) VALUES (?1, ?2)",
        params![waiter, why],
    )?;
    c.execute(
        "DELETE FROM wait_reasons WHERE waiter NOT IN (SELECT id FROM waiters)",
        [],
    )?;
    Ok(())
}

/// The `lab-wait` of waiter `waiter`, which asked for `asked` and ends its wait now (`now`,
/// in seconds): granted, or given up. Its reason is `why`, else the last it waited for
/// ([`waiting`]), which is forgotten.
pub fn wait_event(
    c: &Connection,
    waiter: i64,
    asked: &event::Asked,
    granted: bool,
    why: Option<&str>,
    now: i64,
) -> rusqlite::Result<Fields> {
    let created: Option<i64> = c
        .query_row(
            "SELECT created FROM waiters WHERE id = ?1",
            [waiter],
            |row| row.get(0),
        )
        .optional()?;
    let kept: Option<String> = c
        .query_row(
            "SELECT why FROM wait_reasons WHERE waiter = ?1",
            [waiter],
            |row| row.get(0),
        )
        .optional()?;
    c.execute("DELETE FROM wait_reasons WHERE waiter = ?1", [waiter])?;
    let why = why.map(str::to_owned).or(kept).unwrap_or_default();
    let seconds = u64::try_from(now.saturating_sub(created.unwrap_or(now))).unwrap_or(0);
    Ok(if granted {
        event::wait_granted(
            waiter,
            asked,
            seconds,
            Some(why.as_str()).filter(|w| !w.is_empty()),
        )
    } else {
        event::wait_gave_up(waiter, asked, seconds, &why)
    })
}

/// The oldest waiter short of a device `kind` can serve: who a reclaimed device goes to.
pub fn waiter_for(c: &Connection, kind: &str) -> rusqlite::Result<Option<i64>> {
    c.query_row(
        "SELECT waiter FROM waiter_devices WHERE mode = ?1 OR mode = 'any' ORDER BY waiter LIMIT 1",
        [kind],
        |row| row.get(0),
    )
    .optional()
}
