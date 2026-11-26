//! `factory quota history`: the usage-window history the quota keeper holds, oldest first.
//! The rows are the JSON; the line shows a few, or names the file that holds them.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::time::Duration;

use factory_core::history::{self, HistoryRow};
use factory_core::timestamp::Timestamp;
use serde_json::Value;

use crate::client;
use crate::clock;
use crate::output::{Done, Failure, LINE_LIMIT};
use crate::paths::Paths;
use crate::protocol::{HistoryParams, HistoryRows, QUOTA_HISTORY};

/// How long an export file stays after it was written.
const EXPORT_LIFE: Duration = Duration::from_secs(24 * 3600);

pub fn run(since: Option<&str>, json: bool) -> Result<Done, Failure> {
    let since = since
        .map(|text| history::parse_since(text, clock::now()))
        .transpose()
        .map_err(Failure::usage)?;
    let paths = Paths::from_env()?;
    let answer: HistoryRows = client::call(
        &paths,
        QUOTA_HISTORY,
        &HistoryParams {
            since: since.map(Timestamp::unix_millis),
        },
    )?;
    let document = serde_json::to_value(&answer).unwrap_or(Value::Null);
    let rows = &answer.rows;
    if rows.is_empty() {
        return Ok(Done::new("quota history: no readings", document));
    }
    let inline = format!(
        "quota history: {} rows: {}",
        rows.len(),
        rows.iter()
            .map(HistoryRow::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
    // --json prints the rows themselves, whatever their number, and writes no file.
    if json || inline.chars().count() <= LINE_LIMIT {
        return Ok(Done::new(inline, document));
    }
    let file = export(&paths.state, &document.to_string())?;
    let mut done = Done::new(
        format!("quota history: {} rows in {}", rows.len(), file.display()),
        document,
    );
    // The point of the line is the file it names: a cut path is no path.
    done.whole = true;
    Ok(done)
}

/// Writes the rows to a file in the state dir that no other result's rows share: its name is
/// made from its content, and it appears whole or not at all.
fn export(state: &Path, content: &str) -> Result<std::path::PathBuf, Failure> {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let file = state.join(format!("quota-history-{:016x}.json", hasher.finish()));
    let partial = file.with_extension(format!("json.{}.tmp", std::process::id()));
    let cannot = |e: std::io::Error| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check FACTORY_STATE",
            file.display()
        ))
    };
    fs::write(&partial, format!("{content}\n")).map_err(cannot)?;
    fs::rename(&partial, &file).map_err(|e| {
        let _ = fs::remove_file(&partial);
        cannot(e)
    })?;
    forget_old_exports(state, &file);
    Ok(file)
}

/// Removes the exports of earlier results that nobody has written to for a day, `keep` aside:
/// a file lives as long as the line that named it is likely to be read.
fn forget_old_exports(state: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(state) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > EXPORT_LIFE));
        if name.starts_with("quota-history-")
            && name.ends_with(".json")
            && old
            && entry.path() != keep
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}
