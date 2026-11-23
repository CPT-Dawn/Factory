//! The daemon's SQLite database, `<state>/factory.db`, in WAL mode: what a daemon restart must
//! not lose: each review backend's quota, and each wait that outlives its CLI (`--notify`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use factory_core::quota::Quota;
use factory_core::timestamp::Timestamp;
use rusqlite::{Connection, params};

use crate::output::Failure;
use crate::protocol::{NotifyOn, SessionParam};

/// The schema version this daemon writes, kept in `PRAGMA user_version`.
const VERSION: i64 = 2;

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, Failure> {
        let cannot = |e: rusqlite::Error| {
            Failure::precondition(format!(
                "cannot open the database {}: {e}; check FACTORY_STATE, or move the file aside",
                path.display()
            ))
        };
        let db = Connection::open(path).map_err(cannot)?;
        // It holds the inbox tokens of the sessions that wait with --notify; SQLite gives its WAL
        // files the database's mode.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| {
            Failure::precondition(format!(
                "cannot make the database {} private: {e}; check its owner",
                path.display()
            ))
        })?;
        db.pragma_update(None, "journal_mode", "wal")
            .map_err(cannot)?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(cannot)?;
        if version > VERSION {
            return Err(Failure::precondition(format!(
                "the database {} is from a newer factory (schema {version}); upgrade factory",
                path.display()
            )));
        }
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS quota (
                 backend TEXT PRIMARY KEY,
                 state TEXT NOT NULL CHECK (state IN ('available', 'cooling')),
                 until_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS notify_wait (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL CHECK (kind IN ('ci', 'lead')),
                 target TEXT NOT NULL,
                 cwd TEXT,
                 poll_secs INTEGER,
                 waiter TEXT,
                 socket TEXT NOT NULL,
                 token TEXT NOT NULL,
                 pid INTEGER,
                 start TEXT,
                 deadline_ms INTEGER NOT NULL,
                 limit_secs INTEGER NOT NULL
             );
             PRAGMA user_version = 2;",
        )
        .map_err(cannot)?;
        Ok(Store { db })
    }

    /// Every backend's recorded quota; one never recorded is unknown.
    pub fn quotas(&self) -> rusqlite::Result<Vec<(String, Quota)>> {
        let mut rows = self
            .db
            .prepare("SELECT backend, state, until_ms FROM quota ORDER BY backend")?;
        rows.query_map([], |row| {
            let state: String = row.get(1)?;
            let until: Option<i64> = row.get(2)?;
            Ok((row.get(0)?, quota_of(&state, until)))
        })?
        .collect()
    }

    /// Records `backend`'s quota.
    pub fn set_quota(&self, backend: &str, quota: Quota) -> rusqlite::Result<()> {
        let (state, until) = match quota {
            Quota::Unknown => {
                self.db
                    .execute("DELETE FROM quota WHERE backend = ?1", params![backend])?;
                return Ok(());
            }
            Quota::Available => ("available", None),
            Quota::CoolingDown { until } => ("cooling", Some(until.unix_millis())),
        };
        self.db.execute(
            "INSERT INTO quota (backend, state, until_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (backend) DO UPDATE SET state = ?2, until_ms = ?3",
            params![backend, state, until],
        )?;
        Ok(())
    }
}

/// A wait that outlives its CLI, as the database keeps it.
#[derive(Clone, Debug)]
pub struct NotifyRow {
    pub id: i64,
    pub on: NotifyOn,
    pub waiter: Option<String>,
    pub session: SessionParam,
    /// When it gives up, in milliseconds since the epoch.
    pub deadline_ms: i64,
    /// How long it was given, for the line that says it ran out.
    pub limit_secs: u64,
}

impl Store {
    /// Keeps a wait that outlives its CLI; its `id` is the database's, never reused.
    pub fn add_notify(&self, row: &NotifyRow) -> rusqlite::Result<i64> {
        let (kind, target, cwd, poll) = match &row.on {
            NotifyOn::Ci { pr, cwd, poll_secs } => ("ci", pr, Some(cwd), Some(*poll_secs)),
            NotifyOn::Lead { lead } => ("lead", lead, None, None),
        };
        self.db.execute(
            "INSERT INTO notify_wait (kind, target, cwd, poll_secs, waiter, socket, token, pid,
                                      start, deadline_ms, limit_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                kind,
                target,
                cwd,
                poll.map(sql_int),
                row.waiter,
                row.session.socket,
                row.session.token,
                row.session.pid,
                row.session.start,
                row.deadline_ms,
                sql_int(row.limit_secs),
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Every wait that outlives its CLI, oldest first.
    pub fn notifies(&self) -> rusqlite::Result<Vec<NotifyRow>> {
        let mut rows = self.db.prepare(
            "SELECT id, kind, target, cwd, poll_secs, waiter, socket, token, pid, start,
                    deadline_ms, limit_secs
             FROM notify_wait ORDER BY id",
        )?;
        rows.query_map([], |row| {
            let kind: String = row.get(1)?;
            let target: String = row.get(2)?;
            let on = if kind == "ci" {
                NotifyOn::Ci {
                    pr: target,
                    cwd: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    poll_secs: row.get::<_, Option<i64>>(4)?.map_or(0, unsigned),
                }
            } else {
                NotifyOn::Lead { lead: target }
            };
            Ok(NotifyRow {
                id: row.get(0)?,
                on,
                waiter: row.get(5)?,
                session: SessionParam {
                    socket: row.get(6)?,
                    token: row.get(7)?,
                    pid: row.get(8)?,
                    start: row.get(9)?,
                },
                deadline_ms: row.get(10)?,
                limit_secs: unsigned(row.get(11)?),
            })
        })?
        .collect()
    }

    /// Forgets a wait that outlives its CLI: it ended, or its session went away.
    pub fn remove_notify(&self, id: i64) -> rusqlite::Result<()> {
        self.db
            .execute("DELETE FROM notify_wait WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn sql_int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn unsigned(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn quota_of(state: &str, until: Option<i64>) -> Quota {
    match (state, until) {
        ("cooling", Some(until)) => Quota::CoolingDown {
            until: Timestamp::from_unix_millis(until),
        },
        ("available", _) => Quota::Available,
        _ => Quota::Unknown,
    }
}
