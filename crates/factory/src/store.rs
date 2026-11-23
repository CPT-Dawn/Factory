//! The daemon's SQLite database, `<state>/factory.db`, in WAL mode: what a daemon restart must
//! not lose: each review backend's quota, and the leads spawned whose end is not yet in the
//! ledger.

use std::path::Path;

use factory_core::quota::Quota;
use factory_core::timestamp::Timestamp;
use rusqlite::{Connection, params};

use crate::output::Failure;

/// The schema version this daemon writes, kept in `PRAGMA user_version`.
const VERSION: i64 = 1;

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
             PRAGMA user_version = 1;",
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

fn quota_of(state: &str, until: Option<i64>) -> Quota {
    match (state, until) {
        ("cooling", Some(until)) => Quota::CoolingDown {
            until: Timestamp::from_unix_millis(until),
        },
        ("available", _) => Quota::Available,
        _ => Quota::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("factory.db")).expect("open the store")
    }

    #[test]
    fn a_started_lead_is_running_across_a_reopen_until_its_end_is_kept() {
        let dir = tempfile::tempdir().expect("a temp dir");
        open(&dir)
            .lead_started("lead-cart", "shop", "/state/cart.md")
            .expect("keep the lead");

        let store = open(&dir);
        assert_eq!(
            store.leads_running().expect("list"),
            vec![(
                "lead-cart".to_owned(),
                "shop".to_owned(),
                "/state/cart.md".to_owned()
            )]
        );
        store.lead_ended("lead-cart").expect("keep the end");
        assert_eq!(open(&dir).leads_running().expect("list"), vec![]);
    }

    #[test]
    fn spawning_a_name_again_makes_it_run_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = open(&dir);
        store
            .lead_started("lead-cart", "shop", "/a.md")
            .expect("keep");
        store.lead_ended("lead-cart").expect("end");
        store
            .lead_started("lead-cart", "shop", "/b.md")
            .expect("keep");
        assert_eq!(store.leads_running().expect("list").len(), 1);
    }
}
