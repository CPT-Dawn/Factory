//! agy's quota (spec #67), the CLI's side: `factory quota agy` shows each quota group's windows
//! as the daemon's quota keeper read them from agy's usage query, or says why they can't be
//! read. Whether agy may review is the router's to say, from its own, fresher look.

use factory_core::status::Agy;
use serde_json::{Value, json};

use crate::client;
use crate::clock;
use crate::output::{Done, Failure, LINE_LIMIT};
use crate::paths::Paths;
use crate::protocol::AGY_QUOTA;
use crate::quotas::AGY_QUERY;

/// `factory quota agy`: each quota group's windows, `Gemini Models: 5h 30%, weekly 7%`, on one
/// line, or under a summary line when they are too long for one; or
/// `agy usage unreadable: <why>`, never a share it did not read. It exits 0 either way: it
/// reports, and does not gate (`factory quota claude` does).
pub fn quota() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let agy: Agy =
        client::call_streaming(&paths, AGY_QUOTA, &json!({}), Some(AGY_QUERY), &mut |_| {})?;
    let json = serde_json::to_value(&agy).unwrap_or(Value::Null);
    Ok(said(&agy, clock::now().unix_millis(), json))
}

fn said(agy: &Agy, now: i64, json: Value) -> Done {
    if let Some(problem) = agy.problem() {
        return Done::new(problem, json);
    }
    let groups: Vec<String> = agy
        .groups
        .iter()
        .map(|group| format!("{}: {}", group.name, group.describe(now, ", ")))
        .collect();
    if groups.is_empty() {
        return Done::new("agy: no reading yet; try again", json);
    }
    let line = format!("agy: {}", groups.join("; "));
    if line.chars().count() <= LINE_LIMIT {
        return Done::new(line, json);
    }
    // Too many groups, or names too long, for one line: a summary, and a row for each group.
    let worst = agy
        .worst(now)
        .map_or_else(|| "unknown".to_owned(), |used| format!("{used}%"));
    let mut done = Done::new(
        format!("agy: {} quota groups, worst window {worst}", groups.len()),
        json,
    );
    done.more = groups
        .into_iter()
        .map(|group| format!("  {group}"))
        .collect();
    done
}
