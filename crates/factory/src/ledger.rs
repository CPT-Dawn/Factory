//! The append-only event ledger, `<state>/events.jsonl`: the file factory-stats reads, written
//! only by the daemon.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use factory_core::event::{Event, Fields};
use serde_json::Value;

use crate::clock;

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
/// reads as a lead that never began. A start with `outcome=error` began no lead, so it is no
/// start here: counting it would leave the lead it shares a slug with owing an end it already
/// has, or a run its settlement no longer matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    pub starts: usize,
    pub owes: bool,
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
            Some("lead-start") if !named(event, "outcome", "error") => {
                standing.starts += 1;
                standing.owes = true;
            }
            Some("lead-end") => standing.owes = false,
            _ => {}
        }
    }
    Ok(standing)
}

/// Opens the ledger for appending, creating it.
fn open_file(path: &std::path::Path) -> io::Result<File> {
    File::options().create(true).append(true).open(path)
}

/// Stamps `event` and writes it as one line, opening the ledger first if it isn't open: one
/// that could not be opened before may be openable now.
fn write_line(
    file: &mut Option<File>,
    path: &std::path::Path,
    event: &Event,
) -> io::Result<Fields> {
    if file.is_none() {
        *file = Some(open_file(path)?);
    }
    let Some(file) = file.as_mut() else {
        return Err(io::Error::other("the ledger is not open"));
    };
    let landed = event.stamped(clock::now());
    let mut line = Value::Object(landed.clone()).to_string();
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(landed)
}

/// The ledger's file. Each event is stamped and written as one line under one lock, so lines
/// never interleave. A ledger that can't be opened fails each write, not the daemon: the lab
/// goes on without its events (#62), and `factory log` says why it can't log.
pub struct Ledger {
    pub path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Ledger {
    /// Where the lead `slug` of `repo` stands: whether it still owes the ledger its `lead-end`,
    /// and which run of that slug the latest start is.
    pub fn standing(&self, repo: &str, slug: &str) -> io::Result<Standing> {
        standing_in(&self.path, repo, slug)
    }

    /// Appends the `lead-start` of the lead `slug` of `repo` and says which run of that slug it
    /// is. The run is counted, and `keep` called, under the same lock as the write, so a start
    /// logged beside it cannot take its number and a settlement of an earlier run, which holds
    /// that lock too, cannot fall between what `keep` records and the start it records.
    pub fn append_start(
        &self,
        repo: &str,
        slug: &str,
        event: &Event,
        keep: impl FnOnce(),
    ) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let run = standing_in(&self.path, repo, slug)?.starts + 1;
        keep();
        write_line(&mut file, &self.path, event)?;
        Ok(run)
    }

    /// Appends the `lead-end` of the lead `slug` of `repo`, the run whose start was the
    /// `run`th, unless the ledger already holds an end for it, whoever wrote it, or the lead has
    /// been started again since. The check, the write and `settled`, called once the run has its
    /// end, share the lock every append takes, so an event logged in between cannot slip past.
    pub fn append_end(
        &self,
        repo: &str,
        slug: &str,
        run: usize,
        event: &Event,
        settled: impl FnOnce(),
    ) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let standing = standing_in(&self.path, repo, slug)?;
        if standing.starts != run {
            return Ok(());
        }
        if standing.owes {
            write_line(&mut file, &self.path, event)?;
        }
        settled();
        Ok(())
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

    /// The ledger at `path`; one that can't be opened yet is said once on stderr and tried
    /// again on each write.
    pub fn open(path: PathBuf) -> Ledger {
        let file = match open_file(&path) {
            Ok(file) => Some(file),
            Err(e) => {
                eprintln!(
                    "factory: cannot open {}: {e}; check FACTORY_STATE and XDG_STATE_HOME",
                    path.display()
                );
                None
            }
        };
        Ledger {
            path,
            file: Mutex::new(file),
        }
    }

    pub fn append(&self, event: &Event) -> io::Result<Fields> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_line(&mut file, &self.path, event)
    }
}
