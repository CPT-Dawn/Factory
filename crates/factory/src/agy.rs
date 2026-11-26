//! agy's quota (spec #67), the CLI's side: `factory quota agy` shows each quota group's windows
//! as the daemon's quota keeper read them from agy's usage query, or says why they can't be
//! read. Whether agy may review is the router's to say, from its own, fresher look.

use std::fs;
use std::path::Path;

use factory_core::status::Agy;
use serde_json::{Value, json};

use crate::client;
use crate::clock;
use crate::output::{Done, Failure, LINE_LIMIT};
use crate::paths::Paths;
use crate::protocol::AGY_QUOTA;
use crate::quotas::AGY_QUERY;

/// `factory quota agy`: each quota group's windows, `Gemini Models: 5h 30%, weekly 7%`, on one
/// line, or a summary line naming a file with a row for each when they are too long for one; or
/// `agy usage unreadable: <why>`, never a share it did not read. It exits 0 either way: it
/// reports, and does not gate (`factory quota claude` does).
pub fn quota(json_output: bool) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let agy: Agy =
        client::call_streaming(&paths, AGY_QUOTA, &json!({}), Some(AGY_QUERY), &mut |_| {})?;
    let json = serde_json::to_value(&agy).unwrap_or(Value::Null);
    said(
        &agy,
        clock::now().unix_millis(),
        json,
        (!json_output).then(|| paths.agy_quota()).as_deref(),
    )
}

/// The listing is where a long answer's rows go, when the caller wants the plain output: `--json`
/// has every group and writes no file.
fn said(agy: &Agy, now: i64, json: Value, listing: Option<&Path>) -> Result<Done, Failure> {
    if let Some(problem) = agy.problem() {
        return Ok(Done::new(problem, json));
    }
    let groups: Vec<String> = agy
        .groups
        .iter()
        .map(|group| {
            // A name with a line break in it is still one row.
            let name = group.name.replace(['\n', '\r'], " ");
            format!("{name}: {}", group.describe(now, ", "))
        })
        .collect();
    if groups.is_empty() {
        return Ok(Done::new("agy: no reading yet; try again", json));
    }
    let line = format!("agy: {}", groups.join("; "));
    if line.chars().count() <= LINE_LIMIT {
        return Ok(Done::new(line, json));
    }
    // Too many groups, or names too long, for one line: a summary naming the file that has a
    // row for each.
    let worst = agy
        .worst(now)
        .map_or_else(|| "unknown".to_owned(), |used| format!("{used}%"));
    let summary = format!("agy: {} quota groups, worst window {worst}", groups.len());
    let Some(listing) = listing else {
        return Ok(Done::new(summary, json));
    };
    fs::write(listing, groups.join("\n") + "\n").map_err(|e| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check the disk and FACTORY_STATE, or use --json",
            listing.display()
        ))
    })?;
    let mut done = Done::new(format!("{summary}; listing in {}", listing.display()), json);
    // The file is the line's to name whole, however deep its path.
    done.whole = true;
    Ok(done)
}
