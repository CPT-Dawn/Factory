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

/// Where a lead stands in the ledger: how many starts it has, and whether the latest of its
/// starts and ends is a start. A lead with no events owes nothing, since an end without a start
/// reads as a lead that never began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    pub starts: usize,
    pub owes: bool,
}

/// What came of asking for a lead's end to be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Written,
    /// The ledger already holds an end for that start, whoever logged it.
    Already,
    /// The lead was started again since: the end is not this run's to write.
    Superseded,
}

fn standing_in(path: &std::path::Path, repo: &str, slug: &str) -> io::Result<Standing> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Standing {
                starts: 0,
                owes: false,
            });
        }
        Err(e) => return Err(e),
    };
    let mut standing = Standing {
        starts: 0,
        owes: false,
    };
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
            Some("lead-start") => {
                standing.starts += 1;
                standing.owes = true;
            }
            Some("lead-end") => standing.owes = false,
            _ => {}
        }
    }
    Ok(standing)
}

/// Stamps `event` and writes it as one line.
fn write_line(file: &mut File, event: &Event) -> io::Result<Fields> {
    let landed = event.stamped(clock::now());
    let mut line = Value::Object(landed.clone()).to_string();
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(landed)
}

/// The ledger's file. Each event is stamped and written as one line under one lock, so lines
/// never interleave.
pub struct Ledger {
    pub path: PathBuf,
    file: Mutex<File>,
}

impl Ledger {
    /// Where the lead `slug` of `repo` stands: whether it still owes the ledger its `lead-end`,
    /// and which run of that slug the latest start is.
    pub fn standing(&self, repo: &str, slug: &str) -> io::Result<Standing> {
        standing_in(&self.path, repo, slug)
    }

    /// Appends the `lead-start` of the lead `slug` of `repo` and says which run of that slug it
    /// is, counted under the same lock as the write, so a start logged beside it cannot take
    /// its number.
    pub fn append_start(&self, repo: &str, slug: &str, event: &Event) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_line(&mut file, event)?;
        Ok(standing_in(&self.path, repo, slug)?.starts)
    }

    /// Appends the `lead-end` of the lead `slug` of `repo`, the run whose start was the
    /// `run`th, unless the ledger already holds an end for it, whoever wrote it, or the lead has
    /// been started again since. The check and the write share the lock every append takes, so
    /// an event logged in between cannot slip past.
    pub fn append_end(&self, repo: &str, slug: &str, run: usize, event: &Event) -> io::Result<End> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let standing = standing_in(&self.path, repo, slug)?;
        if standing.starts != run {
            return Ok(End::Superseded);
        }
        if !standing.owes {
            return Ok(End::Already);
        }
        write_line(&mut file, event)?;
        Ok(End::Written)
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
        write_line(&mut file, event)
    }
}
