//! The readiness table of `lab.db` (#55): each attached phone's readiness as the daemon's last
//! probe found it, and when. A row lives from the phone's plug-in to its unplug. Written by the
//! daemon alone, in the lab's short transactions.

use factory_core::lab::ready::{Known, Readiness};
use rusqlite::{Connection, OptionalExtension, Row, params};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS readiness (
  serial TEXT PRIMARY KEY,
  model TEXT NOT NULL DEFAULT '',
  readiness TEXT NOT NULL,
  ever_ready INTEGER NOT NULL DEFAULT 0,
  warning TEXT,
  probed_ms INTEGER NOT NULL
);
";

/// One phone's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyRow {
    pub serial: String,
    pub model: String,
    pub known: Known,
    /// What `devlab check` warns of a ready phone.
    pub warning: Option<String>,
    /// When it was probed, in ms since the epoch.
    pub probed_ms: i64,
}

fn row(row: &Row<'_>) -> rusqlite::Result<ReadyRow> {
    let word: String = row.get("readiness")?;
    Ok(ReadyRow {
        serial: row.get("serial")?,
        model: row.get("model")?,
        known: Known {
            readiness: Readiness::from_word(&word).unwrap_or(Readiness::Offline),
            ever_ready: row.get("ever_ready")?,
        },
        warning: row.get("warning")?,
        probed_ms: row.get("probed_ms")?,
    })
}

pub fn all(c: &Connection) -> rusqlite::Result<Vec<ReadyRow>> {
    let mut q = c.prepare("SELECT * FROM readiness ORDER BY serial")?;
    q.query_map([], row)?.collect()
}

pub fn get(c: &Connection, serial: &str) -> rusqlite::Result<Option<ReadyRow>> {
    c.query_row(
        "SELECT * FROM readiness WHERE serial = ?1",
        params![serial],
        row,
    )
    .optional()
}

pub fn put(c: &Connection, r: &ReadyRow) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO readiness (serial, model, readiness, ever_ready, warning, probed_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            r.serial,
            r.model,
            r.known.readiness.word(),
            r.known.ever_ready,
            r.warning,
            r.probed_ms
        ],
    )?;
    Ok(())
}

pub fn drop_row(c: &Connection, serial: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM readiness WHERE serial = ?1", params![serial])?;
    Ok(())
}

/// The lease that holds phone `serial`, as a notification names it: `lease 3 (demo-a)`.
pub fn holder(c: &Connection, serial: &str) -> rusqlite::Result<Option<String>> {
    c.query_row(
        "SELECT l.id, l.worktree FROM lease_devices d JOIN leases l ON l.id = d.lease_id
         WHERE d.serial = ?1",
        params![serial],
        |r| {
            let id: i64 = r.get(0)?;
            let worktree: String = r.get(1)?;
            Ok(format!(
                "lease {id} ({})",
                factory_core::lab::base_name(&worktree)
            ))
        },
    )
    .optional()
}
