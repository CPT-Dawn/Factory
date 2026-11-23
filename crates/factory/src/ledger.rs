//! The append-only event ledger, `<state>/events.jsonl`: the file factory-stats reads, written
//! only by the daemon.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use factory_core::event::{Event, Fields};
use serde_json::Value;

use crate::clock;
use crate::output::Failure;

/// The ledger's file. Each event is stamped and written as one line under one lock, so lines
/// never interleave.
pub struct Ledger {
    pub path: PathBuf,
    file: Mutex<File>,
}

impl Ledger {
    /// Whether the lead `slug` of `repo` still owes the ledger its `lead-end`: its latest event
    /// is a `lead-start`. A lead with no events owes nothing, since an end without a start
    /// reads as a lead that never began.
    pub fn owes_end(&self, repo: &str, slug: &str) -> io::Result<bool> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut owes = false;
        for event in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            // A repo or slug of digits is stored as a number: compare what each prints.
            let named = |field: &str, want: &str| match &event[field] {
                Value::String(text) => text == want,
                other => format!("{other}") == want,
            };
            if !(named("repo", repo) && named("slug", slug)) {
                continue;
            }
            match event["type"].as_str() {
                Some("lead-start") => owes = true,
                Some("lead-end") => owes = false,
                _ => {}
            }
        }
        Ok(owes)
    }

    pub fn open(path: PathBuf) -> Result<Ledger, Failure> {
        let file = File::options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot open {}: {e}; check FACTORY_STATE and XDG_STATE_HOME",
                    path.display()
                ))
            })?;
        Ok(Ledger {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn append(&self, event: &Event) -> io::Result<Fields> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let landed = event.stamped(clock::now());
        let mut line = Value::Object(landed.clone()).to_string();
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(landed)
    }
}
