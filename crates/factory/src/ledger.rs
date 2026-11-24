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

/// Whether an event's field names `want`. A repo or slug of digits is stored as a number, so
/// what each prints is compared.
fn named(event: &Fields, field: &str, want: &str) -> bool {
    match event.get(field) {
        Some(Value::String(text)) => text == want,
        Some(other) => format!("{other}") == want,
        None => false,
    }
}

/// Whether the lead `slug` of `repo` owes an end at `path`: its latest start or end there is a
/// start.
fn owes_end_in(path: &std::path::Path, repo: &str, slug: &str) -> io::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut owes = false;
    for event in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        let Value::Object(event) = &event else {
            continue;
        };
        if !(named(event, "repo", repo) && named(event, "slug", slug)) {
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
        owes_end_in(&self.path, repo, slug)
    }

    /// Appends the `lead-end` of the lead `slug` of `repo` unless the ledger already holds
    /// one for its latest start, whoever wrote it. The check and the write share the lock every
    /// append takes, so a `lead-end` logged in between cannot slip past. Whether it was written.
    pub fn append_end(&self, repo: &str, slug: &str, event: &Event) -> io::Result<bool> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !owes_end_in(&self.path, repo, slug)? {
            return Ok(false);
        }
        let mut line = Value::Object(event.stamped(clock::now())).to_string();
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(true)
    }

    /// The latest `review` event of `label` (in `repo`, when one is named) as it landed.
    pub fn last_review(&self, repo: Option<&str>, label: &str) -> io::Result<Option<Fields>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter_map(|line| match serde_json::from_str::<Value>(line) {
                Ok(Value::Object(event)) => Some(event),
                _ => None,
            })
            .rfind(|event| {
                event.get("type").and_then(Value::as_str) == Some("review")
                    && named(event, "label", label)
                    && repo.is_none_or(|repo| named(event, "repo", repo))
            }))
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
