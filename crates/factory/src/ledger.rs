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
    /// Whether the ledger holds a `lead-end` for the lead `slug` of `repo` after its latest
    /// `lead-start`.
    pub fn has_ended(&self, repo: &str, slug: &str) -> bool {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let mut ended = false;
        for event in text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            // A repo or slug of digits is stored as a number: compare what each prints.
            let named = |field: &str, want: &str| match &event[field] {
                Value::String(text) => text == want,
                other => other.to_string() == want,
            };
            if !(named("repo", repo) && named("slug", slug)) {
                continue;
            }
            match event["type"].as_str() {
                Some("lead-start") => ended = false,
                Some("lead-end") => ended = true,
                _ => {}
            }
        }
        ended
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
