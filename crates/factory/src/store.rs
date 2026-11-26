//! The daemon's SQLite database, `<state>/factory.db`, in WAL mode: what a daemon restart must
//! not lose: each review backend's quota, the Claude and Codex plans' quotas, the leads spawned whose end is not yet in the
//! ledger, whether the context steward runs and what it has done to each session, each wait
//! that outlives its CLI (`--notify`), each blocking `factory wait` the daemon holds, and the
//! watched repos' CI as GitHub last answered it, with every job's queued, started and finished
//! times.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use factory_core::claude::{ClaudeQuota, Limit};
use factory_core::codex::CodexQuota;
use factory_core::fleet::{Job, Pull, PullState, RepoCi, Runner, RunnerState};
use factory_core::quota::Quota;
use factory_core::timestamp::Timestamp;
use factory_core::usage::{UsageWindow, WindowKind};
use factory_core::wait::WaitOn;
use rusqlite::{Connection, OptionalExtension, params};

use crate::output::Failure;
use crate::protocol::{NotifyOn, SessionParam};

/// The schema version this daemon writes, kept in `PRAGMA user_version`.
const VERSION: i64 = 6;

/// What schema 6 adds to a table an older schema made: Claude's weekly window.
const WEEKLY_COLUMNS: [(&str, &str); 3] = [
    ("weekly_used_percent", "REAL"),
    ("weekly_resets_ms", "INTEGER"),
    ("weekly_seen_ms", "INTEGER"),
];

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
        let mut db = Connection::open(path).map_err(cannot)?;
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
        // One transaction: a store is at one schema or the next, never halfway.
        let migration = db.transaction().map_err(cannot)?;
        migration
            .execute_batch(
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
                 limit_at_ms INTEGER,
                 weekly_used_percent REAL,
                 weekly_resets_ms INTEGER,
                 weekly_seen_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS codex_quota (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 used_percent REAL,
                 resets_ms INTEGER,
                 seen_ms INTEGER,
                 weekly_used_percent REAL,
                 weekly_resets_ms INTEGER,
                 weekly_seen_ms INTEGER,
                 plan TEXT,
                 reset_credits INTEGER NOT NULL DEFAULT 0,
                 asked_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS usage_history (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 harness TEXT NOT NULL,
                 quota_group TEXT,
                 window TEXT NOT NULL,
                 used_percent REAL,
                 resets_ms INTEGER,
                 seen_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS usage_history_seen ON usage_history (seen_ms);
             CREATE TABLE IF NOT EXISTS ci_runner (
                 repo TEXT NOT NULL,
                 id INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 status TEXT NOT NULL,
                 busy INTEGER NOT NULL DEFAULT 0,
                 seen_ms INTEGER NOT NULL,
                 PRIMARY KEY (repo, id)
             );
             CREATE TABLE IF NOT EXISTS ci_job (
                 repo TEXT NOT NULL,
                 id INTEGER NOT NULL,
                 run_id INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 status TEXT NOT NULL,
                 conclusion TEXT,
                 runner TEXT,
                 queued_ms INTEGER,
                 started_ms INTEGER,
                 finished_ms INTEGER,
                 PRIMARY KEY (repo, id)
             );
             CREATE TABLE IF NOT EXISTS ci_pull (
                 repo TEXT NOT NULL,
                 number INTEGER NOT NULL,
                 branch TEXT NOT NULL,
                 url TEXT NOT NULL,
                 state TEXT NOT NULL,
                 seen_ms INTEGER NOT NULL,
                 PRIMARY KEY (repo, number)
             );
             CREATE TABLE IF NOT EXISTS ci_seen (
                 repo TEXT PRIMARY KEY,
                 seen_ms INTEGER NOT NULL
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
             ",
            )
            .map_err(cannot)?;
        add_codex_asked_column(&migration).map_err(cannot)?;
        if version < VERSION {
            add_weekly_columns(&migration).map_err(cannot)?;
            migration
                .pragma_update(None, "user_version", VERSION)
                .map_err(cannot)?;
        }
        migration.commit().map_err(cannot)?;
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

    /// Records that the end of the lead `name` of `repo` is in the ledger; a lead of that name
    /// spawned since in another repo keeps its record.
    pub fn lead_ended(&self, name: &str, repo: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE lead SET ended = 1 WHERE name = ?1 AND repo = ?2",
            params![name, repo],
        )?;
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
        type Reading = (Option<f64>, Option<i64>, Option<i64>);
        let row = self
            .db
            .query_row(
                "SELECT used_percent, resets_ms, seen_ms, limit_until_ms, limit_at_ms,
                     weekly_used_percent, weekly_resets_ms, weekly_seen_ms
                 FROM claude_quota WHERE id = 1",
                [],
                |row| {
                    let five: Reading = (row.get(0)?, row.get(1)?, row.get(2)?);
                    let weekly: Reading = (row.get(5)?, row.get(6)?, row.get(7)?);
                    let limit = (row.get::<_, Option<i64>>(3)?, row.get::<_, Option<i64>>(4)?);
                    Ok((five, weekly, limit))
                },
            )
            .optional()?;
        let Some((five, weekly, (until, at))) = row else {
            return Ok(ClaudeQuota::default());
        };
        let stamp = Timestamp::from_unix_millis;
        let window = |kind, (used, resets, seen): Reading| match (used, resets, seen) {
            (Some(used_percent), Some(resets), Some(seen)) => Some(UsageWindow {
                kind,
                used_percent,
                resets: stamp(resets),
                seen: stamp(seen),
            }),
            _ => None,
        };
        Ok(ClaudeQuota {
            five_hour: window(WindowKind::FiveHour, five),
            weekly: window(WindowKind::Weekly, weekly),
            limit: until.zip(at).map(|(until, at)| Limit {
                until: stamp(until),
                at: stamp(at),
            }),
        })
    }

    /// Records the Claude plan's quota.
    pub fn set_claude_quota(&self, quota: &ClaudeQuota) -> rusqlite::Result<()> {
        let (five, weekly) = (quota.five_hour, quota.weekly);
        self.db.execute(
            "INSERT INTO claude_quota (id, used_percent, resets_ms, seen_ms, limit_until_ms,
                 limit_at_ms, weekly_used_percent, weekly_resets_ms, weekly_seen_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (id) DO UPDATE SET used_percent = ?1, resets_ms = ?2, seen_ms = ?3,
                 limit_until_ms = ?4, limit_at_ms = ?5, weekly_used_percent = ?6,
                 weekly_resets_ms = ?7, weekly_seen_ms = ?8",
            params![
                five.map(|window| window.used_percent),
                five.map(|window| window.resets.unix_millis()),
                five.map(|window| window.seen.unix_millis()),
                quota.limit.map(|limit| limit.until.unix_millis()),
                quota.limit.map(|limit| limit.at.unix_millis()),
                weekly.map(|window| window.used_percent),
                weekly.map(|window| window.resets.unix_millis()),
                weekly.map(|window| window.seen.unix_millis()),
            ],
        )?;
        Ok(())
    }
}

impl Store {
    /// The Codex plan's windows, plan and reset credits as last read; nothing recorded is
    /// nothing known.
    pub fn codex_quota(&self) -> rusqlite::Result<CodexQuota> {
        type Reading = (Option<f64>, Option<i64>, Option<i64>);
        let row = self
            .db
            .query_row(
                "SELECT used_percent, resets_ms, seen_ms, weekly_used_percent, weekly_resets_ms,
                     weekly_seen_ms, plan, reset_credits, asked_ms
                 FROM codex_quota WHERE id = 1",
                [],
                |row| {
                    let five: Reading = (row.get(0)?, row.get(1)?, row.get(2)?);
                    let weekly: Reading = (row.get(3)?, row.get(4)?, row.get(5)?);
                    let credits: i64 = row.get(7)?;
                    let asked: Option<i64> = row.get(8)?;
                    Ok((
                        five,
                        weekly,
                        row.get::<_, Option<String>>(6)?,
                        credits,
                        asked,
                    ))
                },
            )
            .optional()?;
        let Some((five, weekly, plan, credits, asked)) = row else {
            return Ok(CodexQuota::default());
        };
        let stamp = Timestamp::from_unix_millis;
        let window = |kind, (used, resets, seen): Reading| match (used, resets, seen) {
            (Some(used_percent), Some(resets), Some(seen)) => Some(UsageWindow {
                kind,
                used_percent,
                resets: stamp(resets),
                seen: stamp(seen),
            }),
            _ => None,
        };
        let (five_hour, weekly) = (
            window(WindowKind::FiveHour, five),
            window(WindowKind::Weekly, weekly),
        );
        Ok(CodexQuota {
            // A restart within 5 minutes of an ask, read or not, does not ask again.
            asked: asked.map(stamp),
            five_hour,
            weekly,
            plan,
            reset_credits: u32::try_from(credits).unwrap_or(0),
            ..CodexQuota::default()
        })
    }

    /// Records the Codex plan's windows, plan, reset credits and when it was last asked. Whether
    /// the last ask could be read is not kept: a daemon that starts within 5 minutes of the ask
    /// does not press a Codex that was down.
    pub fn set_codex_quota(&self, quota: &CodexQuota) -> rusqlite::Result<()> {
        let (five, weekly) = (quota.five_hour, quota.weekly);
        self.db.execute(
            "INSERT INTO codex_quota (id, used_percent, resets_ms, seen_ms, weekly_used_percent,
                 weekly_resets_ms, weekly_seen_ms, plan, reset_credits, asked_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (id) DO UPDATE SET used_percent = ?1, resets_ms = ?2, seen_ms = ?3,
                 weekly_used_percent = ?4, weekly_resets_ms = ?5, weekly_seen_ms = ?6,
                 plan = ?7, reset_credits = ?8, asked_ms = ?9",
            params![
                five.map(|window| window.used_percent),
                five.map(|window| window.resets.unix_millis()),
                five.map(|window| window.seen.unix_millis()),
                weekly.map(|window| window.used_percent),
                weekly.map(|window| window.resets.unix_millis()),
                weekly.map(|window| window.seen.unix_millis()),
                quota.plan,
                i64::from(quota.reset_credits),
                quota.asked.map(Timestamp::unix_millis),
            ],
        )?;
        Ok(())
    }
    /// Records a repo's CI as GitHub just answered it: its runners and open factory pull
    /// requests replace the last ones, and every job read keeps the times it had.
    pub fn record_ci(&mut self, ci: &RepoCi, read: &[Job]) -> rusqlite::Result<()> {
        let seen = ci.seen.unwrap_or_default();
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM ci_runner WHERE repo = ?1", params![ci.repo])?;
        for runner in &ci.runners {
            let (status, busy) = runner.state.github();
            tx.execute(
                "INSERT INTO ci_runner (repo, id, name, status, busy, seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![ci.repo, sql(runner.id), runner.name, status, busy, seen],
            )?;
        }
        for job in read {
            tx.execute(
                "INSERT INTO ci_job (repo, id, run_id, name, status, conclusion, runner,
                     queued_ms, started_ms, finished_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (repo, id) DO UPDATE SET run_id = ?3, name = ?4, status = ?5,
                     conclusion = ?6, runner = COALESCE(?7, runner),
                     queued_ms = COALESCE(?8, queued_ms), started_ms = COALESCE(?9, started_ms),
                     finished_ms = COALESCE(?10, finished_ms)",
                params![
                    ci.repo,
                    sql(job.id),
                    sql(job.run_id),
                    job.name,
                    job.status,
                    job.conclusion,
                    job.runner,
                    job.queued,
                    job.started,
                    job.finished,
                ],
            )?;
        }
        tx.execute("DELETE FROM ci_pull WHERE repo = ?1", params![ci.repo])?;
        for pull in &ci.pulls {
            tx.execute(
                "INSERT INTO ci_pull (repo, number, branch, url, state, seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    ci.repo,
                    sql(pull.number),
                    pull.branch,
                    pull.url,
                    pull.state.word(),
                    seen
                ],
            )?;
        }
        tx.execute(
            "INSERT INTO ci_seen (repo, seen_ms) VALUES (?1, ?2)
             ON CONFLICT (repo) DO UPDATE SET seen_ms = ?2",
            params![ci.repo, seen],
        )?;
        tx.commit()
    }

    /// A repo's CI as last recorded: its runners, its jobs not yet completed, its open factory
    /// pull requests and its latest completed jobs; nothing, and never seen, before the first
    /// answer.
    pub fn ci(&self, repo: &str) -> rusqlite::Result<RepoCi> {
        let seen = self
            .db
            .query_row(
                "SELECT seen_ms FROM ci_seen WHERE repo = ?1",
                params![repo],
                |row| row.get(0),
            )
            .optional()?;
        let mut runners = self
            .db
            .prepare("SELECT id, name, status, busy FROM ci_runner WHERE repo = ?1 ORDER BY id")?;
        let runners = runners
            .query_map(params![repo], |row| {
                let status: String = row.get(2)?;
                Ok(Runner {
                    id: unsql(row.get(0)?),
                    name: row.get(1)?,
                    state: RunnerState::of(&status, row.get(3)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let jobs = self.ci_jobs(repo, "status != 'completed' ORDER BY id", u32::MAX)?;
        let finished = self.ci_jobs(
            repo,
            "status = 'completed' ORDER BY finished_ms DESC, id DESC",
            FINISHED_KEPT,
        )?;
        let mut pulls = self.db.prepare(
            "SELECT number, branch, url, state FROM ci_pull WHERE repo = ?1 ORDER BY number",
        )?;
        let pulls = pulls
            .query_map(params![repo], |row| {
                let state: String = row.get(3)?;
                Ok(Pull {
                    number: unsql(row.get(0)?),
                    branch: row.get(1)?,
                    url: row.get(2)?,
                    state: PullState::parse(&state),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(RepoCi {
            repo: repo.to_owned(),
            seen,
            error: None,
            runners,
            jobs,
            pulls,
            finished,
        })
    }

    /// At most `limit` of `repo`'s jobs, those `filter` (a SQL condition and order) picks.
    fn ci_jobs(&self, repo: &str, filter: &str, limit: u32) -> rusqlite::Result<Vec<Job>> {
        let mut jobs = self.db.prepare(&format!(
            "SELECT id, run_id, name, status, conclusion, runner, queued_ms, started_ms,
                 finished_ms
             FROM ci_job WHERE repo = ?1 AND {filter} LIMIT ?2"
        ))?;
        jobs.query_map(params![repo, limit], |row| {
            Ok(Job {
                id: unsql(row.get(0)?),
                run_id: unsql(row.get(1)?),
                name: row.get(2)?,
                status: row.get(3)?,
                conclusion: row.get(4)?,
                runner: row.get(5)?,
                queued: row.get(6)?,
                started: row.get(7)?,
                finished: row.get(8)?,
            })
        })?
        .collect()
    }
}

/// How many of a repo's latest completed jobs `factory status --json` shows.
const FINISHED_KEPT: u32 = 10;

/// A GitHub id as SQLite keeps it.
fn sql(id: u64) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
}

/// A GitHub id SQLite kept.
fn unsql(id: i64) -> u64 {
    u64::try_from(id).unwrap_or_default()
}

/// Schema 6's weekly columns, on a `claude_quota` table an older schema made without them.
fn add_weekly_columns(db: &Connection) -> rusqlite::Result<()> {
    let mut columns = db.prepare("SELECT name FROM pragma_table_info('claude_quota')")?;
    let present: Vec<String> = columns
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, kind) in WEEKLY_COLUMNS {
        if !present.iter().any(|column| column == name) {
            db.execute_batch(&format!(
                "ALTER TABLE claude_quota ADD COLUMN {name} {kind};"
            ))?;
        }
    }
    Ok(())
}

/// `codex_quota.asked_ms`, on a table an earlier build made without it (at this same schema):
/// the last ask is taken to be the last window it saw, so a restart soon after it asks nothing.
fn add_codex_asked_column(db: &Connection) -> rusqlite::Result<()> {
    let present: bool = db.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('codex_quota') WHERE name = 'asked_ms'",
        [],
        |row| row.get(0),
    )?;
    if !present {
        db.execute_batch(
            "ALTER TABLE codex_quota ADD COLUMN asked_ms INTEGER;
             UPDATE codex_quota
                 SET asked_ms = NULLIF(MAX(COALESCE(seen_ms, 0), COALESCE(weekly_seen_ms, 0)), 0);",
        )?;
    }
    Ok(())
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
