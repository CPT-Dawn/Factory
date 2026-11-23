//! The cutover's pure rules (#20): what today's devlab shows held, said in one line.
//!
//! `factory cutover` hands the `devlab` name to factory lab only while today's TypeScript
//! devlab holds nothing; these turn what it shows (its `status --json`, its ledger's rows) and
//! what factory lab holds into the names a refusal gives.

use std::collections::BTreeMap;

use serde::Deserialize;

/// How many holdings a refusal names before it counts the rest.
const NAMED: usize = 3;

#[derive(Deserialize)]
struct Status {
    #[serde(default)]
    devices: Vec<Row>,
    #[serde(default)]
    slots: Vec<Row>,
    #[serde(default)]
    waiting: Vec<String>,
}

#[derive(Deserialize)]
struct Row {
    #[serde(default)]
    holder: String,
}

/// What `devlab status --json` shows held: each lease once, in id order, as its first device
/// names it (`lease 7 demo-a (claude)`) or else its slot (`lease 7 demo-a`), then each waiting
/// request (`waiting <label> for 2m`). An idle or free device or slot holds nothing.
pub fn shown(status_json: &str) -> Result<Vec<String>, String> {
    let status: Status = serde_json::from_str(status_json)
        .map_err(|e| format!("devlab status --json printed no status ({e})"))?;
    let mut leases: BTreeMap<i64, String> = BTreeMap::new();
    for holder in status
        .devices
        .iter()
        .chain(&status.slots)
        .map(|row| &row.holder)
    {
        if let Some(id) = lease_id(holder) {
            leases.entry(id).or_insert_with(|| holder.clone());
        }
    }
    Ok(leases
        .into_values()
        .chain(
            status
                .waiting
                .iter()
                .map(|label| format!("waiting {label}")),
        )
        .collect())
}

fn lease_id(holder: &str) -> Option<i64> {
    holder
        .strip_prefix("lease ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// A lease as devlab status names it: `lease <id> <worktree's last name> (<owner>)`.
pub fn lease(id: i64, worktree: &str, owner: &str) -> String {
    let name = worktree
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(worktree);
    format!("lease {id} {name} ({owner})")
}

/// One line: `<what> <the first holdings>[ and N more]; <then>`.
pub fn busy(what: &str, held: &[String], then: &str) -> String {
    format!("{what} {}; {then}", named(held))
}

/// The first holdings by name, then how many more: `a, b, c and 2 more`.
pub fn named(held: &[String]) -> String {
    let mut named = held
        .iter()
        .take(NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if held.len() > NAMED {
        named.push_str(&format!(" and {} more", held.len() - NAMED));
    }
    named
}

#[cfg(test)]
mod tests {
    use super::{busy, lease, shown};

    #[test]
    fn status_shows_each_lease_once_with_its_owner_then_each_waiter() {
        let status = r#"{
          "devices": [
            {"serial": "B", "holder": "free"},
            {"serial": "C", "holder": "idle 3m"},
            {"serial": "D", "holder": "lease 12 demo-b (codex)"},
            {"serial": "E", "holder": "lease 3 demo-a (claude)"},
            {"serial": "F", "holder": "lease 3 demo-a (claude)"}
          ],
          "slots": [
            {"project": "demo", "slot": 0, "holder": "lease 3 demo-a"},
            {"project": "demo", "slot": 1, "holder": "lease 5 demo-c"},
            {"project": "demo", "slot": 2, "holder": "free"}
          ],
          "waiting": ["claude @ demo-d for 1m"]
        }"#;

        assert_eq!(
            shown(status),
            Ok(vec![
                "lease 3 demo-a (claude)".to_owned(),
                "lease 5 demo-c".to_owned(),
                "lease 12 demo-b (codex)".to_owned(),
                "waiting claude @ demo-d for 1m".to_owned(),
            ])
        );
    }

    #[test]
    fn a_free_status_shows_nothing_held() {
        let free = r#"{"devices":[],"slots":[],"owners":[],"waiting":[],"watcher":"active"}"#;
        assert_eq!(shown(free), Ok(vec![]));
        assert_eq!(shown("{}"), Ok(vec![]));
    }

    #[test]
    fn a_status_that_is_no_json_is_named_as_such() {
        let err = shown("devices: none attached").unwrap_err();
        assert!(
            err.starts_with("devlab status --json printed no status ("),
            "{err}"
        );
    }

    #[test]
    fn a_lease_is_named_by_its_worktrees_last_name() {
        assert_eq!(lease(9, "/wt/plain-c", "codex"), "lease 9 plain-c (codex)");
        assert_eq!(lease(9, "/wt/plain-c/", "codex"), "lease 9 plain-c (codex)");
        assert_eq!(lease(1, "here", "me"), "lease 1 here (me)");
    }

    #[test]
    fn a_refusal_names_three_holdings_and_counts_the_rest() {
        let held: Vec<String> = (1..=5).map(|id| format!("lease {id}")).collect();
        assert_eq!(
            busy("devlab status shows", &held[..1], "wait"),
            "devlab status shows lease 1; wait"
        );
        assert_eq!(
            busy("devlab status shows", &held, "wait"),
            "devlab status shows lease 1, lease 2, lease 3 and 2 more; wait"
        );
    }
}
