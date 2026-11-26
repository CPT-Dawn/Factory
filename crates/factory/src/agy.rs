//! agy's quota (spec #67), the CLI's side: `factory quota agy` shows each quota group's windows
//! as the daemon's quota keeper read them from agy's usage query, or says why they can't be
//! read. Whether agy may review is the router's to say, from its own, fresher look.

use factory_core::status::Agy;
use serde_json::{Value, json};

use crate::client;
use crate::clock;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::AGY_QUOTA;
use crate::quotas::AGY_QUERY;

/// `factory quota agy`: one row per quota group, `Gemini Models: 5h 30%, weekly 7%`; or
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
        .map(|group| {
            let windows: Vec<String> = group
                .windows
                .iter()
                .map(|window| window.describe(now))
                .collect();
            format!("{}: {}", group.name, windows.join(", "))
        })
        .collect();
    if groups.is_empty() {
        return Done::new("agy: no reading yet; try again", json);
    }
    Done::new(format!("agy: {}", groups.join("; ")), json)
}
