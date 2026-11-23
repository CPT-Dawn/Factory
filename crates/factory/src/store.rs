//! The daemon's SQLite database, `<state>/factory.db`, in WAL mode: what a daemon restart must
//! not lose: each review backend's quota, the Claude plan's quota, the leads spawned whose end is not yet in the
//! ledger, whether the context steward runs and what it has done to each session, each wait
//! that outlives its CLI (`--notify`), and each blocking `factory wait` the daemon holds.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use factory_core::claude::{ClaudeQuota, Limit, Seen, Window};
use factory_core::quota::Quota;
use factory_core::timestamp::Timestamp;
use factory_core::wait::WaitOn;
use rusqlite::{Connection, OptionalExtension, params};

use crate::output::Failure;
use crate::protocol::{NotifyOn, SessionParam};

/// The schema version this daemon writes, kept in `PRAGMA user_version`.
const VERSION: i64 = 5;

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
             CREATE TABLE IF NOT EXISTS claude_quota (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 used_percent REAL,
                 resets_ms INTEGER,
                 seen_ms INTEGER,
                 limit_until_ms INTEGER,
                 limit_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS wait (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL,
                 target TEXT NOT NULL,
                 waiter TEXT,
                 deadline_ms INTEGER NOT NULL,
                 limit_secs INTEGER NOT NULL,
                 since_ms INTEGER,
                 lead_pane TEXT,
                 lead_report TEXT
             );
             PRAGMA user_version = 5;",
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
    /// The Claude plan's quota as last recorded; nothing recorded is nothing known.
    pub fn claude_quota(&self) -> rusqlite::Result<ClaudeQuota> {
        let row = self
            .db
            .query_row(
                "SELECT used_percent, resets_ms, seen_ms, limit_until_ms, limit_at_ms
                 FROM claude_quota WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<f64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((used, resets, seen, until, at)) = row else {
            return Ok(ClaudeQuota::default());
        };
        let stamp = Timestamp::from_unix_millis;
        Ok(ClaudeQuota {
            window: match (used, resets, seen) {
                (Some(used_percent), Some(resets), Some(seen)) => Some(Seen {
                    window: Window {
                        used_percent,
                        resets: stamp(resets),
                    },
                    at: stamp(seen),
                }),
                _ => None,
            },
            limit: until.zip(at).map(|(until, at)| Limit {
                until: stamp(until),
                at: stamp(at),
            }),
        })
    }

    /// Records the Claude plan's quota.
    pub fn set_claude_quota(&self, quota: &ClaudeQuota) -> rusqlite::Result<()> {
        let window = quota.window;
        self.db.execute(
            "INSERT INTO claude_quota (id, used_percent, resets_ms, seen_ms, limit_until_ms, limit_at_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET used_percent = ?1, resets_ms = ?2, seen_ms = ?3,
                 limit_until_ms = ?4, limit_at_ms = ?5",
            params![
                window.map(|seen| seen.window.used_percent),
                window.map(|seen| seen.window.resets.unix_millis()),
                window.map(|seen| seen.at.unix_millis()),
                quota.limit.map(|limit| limit.until.unix_millis()),
                quota.limit.map(|limit| limit.at.unix_millis()),
            ],
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

/// A blocking `factory wait` the daemon holds, as the database keeps it, so a CLI that lost
/// its daemon to a restart picks it up again from the next one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitRow {
    pub id: i64,
    pub on: WaitOn,
    pub waiter: Option<String>,
    /// When it gives up, in milliseconds since the epoch.
    pub deadline_ms: i64,
    /// How long it was given, for the line that says it ran out.
    pub limit_secs: u64,
    /// For a review: when the run it waits for began, so an older run's end does not answer it.
    pub since: Option<Timestamp>,
    /// For a lead: its pane and report, so a lead gone while no daemon ran still answers.
    pub lead_pane: Option<String>,
    pub lead_report: Option<String>,
}

impl Store {
    /// Keeps a blocking wait; the id it returns is the database's, never reused.
    pub fn add_wait(&self, row: &WaitRow) -> rusqlite::Result<i64> {
        let (kind, target) = row.on.parts();
        self.db.execute(
            "INSERT INTO wait (kind, target, waiter, deadline_ms, limit_secs, since_ms,
                               lead_pane, lead_report)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                kind,
                target,
                row.waiter,
                row.deadline_ms,
                sql_int(row.limit_secs),
                row.since.map(Timestamp::unix_millis),
                row.lead_pane,
                row.lead_report,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Every blocking wait kept, oldest first; a row this daemon cannot read is left out.
    pub fn waits(&self) -> rusqlite::Result<Vec<WaitRow>> {
        let mut rows = self.db.prepare(
            "SELECT id, kind, target, waiter, deadline_ms, limit_secs, since_ms,
                    lead_pane, lead_report
             FROM wait ORDER BY id",
        )?;
        let read = rows.query_map([], |row| {
            let kind: String = row.get(1)?;
            let target: String = row.get(2)?;
            Ok(
                WaitOn::from_parts(&kind, &target).map(|on| -> rusqlite::Result<WaitRow> {
                    Ok(WaitRow {
                        id: row.get(0)?,
                        on,
                        waiter: row.get(3)?,
                        deadline_ms: row.get(4)?,
                        limit_secs: unsigned(row.get(5)?),
                        since: row
                            .get::<_, Option<i64>>(6)?
                            .map(Timestamp::from_unix_millis),
                        lead_pane: row.get(7)?,
                        lead_report: row.get(8)?,
                    })
                }),
            )
        })?;
        let mut waits = Vec::new();
        for row in read {
            if let Some(row) = row? {
                waits.push(row?);
            }
        }
        Ok(waits)
    }

    /// Points a kept wait at `on` instead.
    pub fn retarget_wait(&self, id: i64, on: &WaitOn) -> rusqlite::Result<()> {
        let (kind, target) = on.parts();
        self.db.execute(
            "UPDATE wait SET kind = ?2, target = ?3 WHERE id = ?1",
            params![id, kind, target],
        )?;
        Ok(())
    }

    /// Forgets a kept wait: it was answered, its CLI went away, or no CLI came back for it.
    pub fn remove_wait(&self, id: i64) -> rusqlite::Result<()> {
        self.db
            .execute("DELETE FROM wait WHERE id = ?1", params![id])?;
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
