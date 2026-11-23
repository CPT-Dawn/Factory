//! The daemon's SQLite database, `<state>/factory.db`, in WAL mode: what a daemon restart must
//! not lose: each review backend's quota, the leads spawned whose end is not yet in the
//! ledger, and whether the context steward runs and what it has done to each session.

use std::path::Path;

use factory_core::quota::Quota;
use factory_core::timestamp::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};

use crate::output::Failure;

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
             CREATE TABLE IF NOT EXISTS lead (
                 name TEXT PRIMARY KEY,
                 repo TEXT NOT NULL,
                 report TEXT NOT NULL,
                 ended INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS monitor (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 running INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS context (
                 log TEXT PRIMARY KEY,
                 lead TEXT NOT NULL,
                 agent TEXT,
                 tokens INTEGER,
                 asked INTEGER NOT NULL DEFAULT 0,
                 told INTEGER NOT NULL DEFAULT 0
             );
             PRAGMA user_version = 2;",
        )
        .map_err(cannot)?;
        Ok(Store { db })
    }

    /// Records that the lead `name` was spawned in `repo` and will leave its final report at
    /// `report`; its end is not in the ledger yet.
    pub fn lead_started(&self, name: &str, repo: &str, report: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO lead (name, repo, report, ended) VALUES (?1, ?2, ?3, 0)
             ON CONFLICT (name) DO UPDATE SET repo = ?2, report = ?3, ended = 0",
            params![name, repo, report],
        )?;
        Ok(())
    }

    /// Records that the end of the lead `name` is in the ledger.
    pub fn lead_ended(&self, name: &str) -> rusqlite::Result<()> {
        self.db
            .execute("UPDATE lead SET ended = 1 WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// The leads spawned whose end is not in the ledger yet, as (name, repo, report).
    pub fn leads_running(&self) -> rusqlite::Result<Vec<(String, String, String)>> {
        let mut rows = self
            .db
            .prepare("SELECT name, repo, report FROM lead WHERE ended = 0 ORDER BY name")?;
        rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect()
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

impl Store {
    /// Whether the context steward was left running.
    pub fn monitor_running(&self) -> rusqlite::Result<bool> {
        let running: Option<bool> = self
            .db
            .query_row("SELECT running FROM monitor WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(running.unwrap_or(false))
    }

    pub fn set_monitor_running(&self, running: bool) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO monitor (id, running) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET running = ?1",
            params![running],
        )?;
        Ok(())
    }

    /// The context table's row for the session logged at `log`, if it has one.
    pub fn context(&self, log: &str) -> rusqlite::Result<Option<Context>> {
        self.db
            .query_row(
                "SELECT lead, agent, tokens, asked, told FROM context WHERE log = ?1",
                params![log],
                |row| {
                    Ok(Context {
                        log: log.to_owned(),
                        lead: row.get(0)?,
                        agent: row.get(1)?,
                        tokens: row
                            .get::<_, Option<i64>>(2)?
                            .and_then(|tokens| u64::try_from(tokens).ok()),
                        asked: row.get(3)?,
                        told: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    pub fn set_context(&self, context: &Context) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO context (log, lead, agent, tokens, asked, told)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (log) DO UPDATE
             SET lead = ?2, agent = ?3, tokens = ?4, asked = ?5, told = ?6",
            params![
                context.log,
                context.lead,
                context.agent,
                context.tokens.and_then(|tokens| i64::try_from(tokens).ok()),
                context.asked,
                context.told
            ],
        )?;
        Ok(())
    }
}

/// One session in the context table: a lead's own (no agent) or one of its subagents'.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// The session's log, which names it.
    pub log: String,
    pub lead: String,
    /// The subagent's `agent-<id>`; none for the lead's own session.
    pub agent: Option<String>,
    pub tokens: Option<u64>,
    /// Whether the lead has been asked to stop and not compacted since.
    pub asked: bool,
    /// Whether the subagent's lead has been told it passed the threshold.
    pub told: bool,
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
