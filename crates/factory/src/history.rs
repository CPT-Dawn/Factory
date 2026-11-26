//! `factory quota history`: the usage-window history the quota keeper holds, oldest first.
//! The rows are the JSON; the line shows a few, or names the file that holds them.

use std::fs;

use factory_core::history::{self, HistoryRow};
use factory_core::timestamp::Timestamp;
use serde_json::Value;

use crate::client;
use crate::clock;
use crate::output::{Done, Failure, LINE_LIMIT};
use crate::paths::Paths;
use crate::protocol::{HistoryParams, HistoryRows, QUOTA_HISTORY};

/// The file the rows go to when they don't fit the line, in the state dir.
const FILE: &str = "quota-history.json";

pub fn run(since: Option<&str>) -> Result<Done, Failure> {
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
    let json = serde_json::to_value(&answer).unwrap_or(Value::Null);
    let rows = &answer.rows;
    if rows.is_empty() {
        return Ok(Done::new("quota history: no readings", json));
    }
    let inline = format!(
        "quota history: {} rows: {}",
        rows.len(),
        rows.iter()
            .map(HistoryRow::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
    if inline.chars().count() <= LINE_LIMIT {
        return Ok(Done::new(inline, json));
    }
    let file = paths.state.join(FILE);
    fs::write(&file, format!("{json}\n")).map_err(|e| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check FACTORY_STATE",
            file.display()
        ))
    })?;
    let mut done = Done::new(
        format!("quota history: {} rows in {}", rows.len(), file.display()),
        json,
    );
    // The point of the line is the file it names: a cut path is no path.
    done.whole = true;
    Ok(done)
}
