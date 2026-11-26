//! The Codex plan's quota, the CLI's side: `factory quota codex` tells a lead or a review
//! whether to dispatch on Codex. The daemon asks `codex app-server` for the windows (at most once
//! in 5 minutes) and holds reviews off Codex at `[codex] hold_at_percent`.

use factory_core::exit::Exit;
use factory_core::status::ClaudeState;
use factory_core::timestamp::Timestamp;
use serde_json::Value;

use crate::client;
use crate::clock;
use crate::config;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{CODEX_QUOTA, CodexQuotaParams, CodexVerdict};

/// `factory quota codex`: exit 0 to dispatch, 75 to hold, naming until when. A Codex that
/// could not be read is no hold: its line says why, and the exit is 0.
pub fn quota() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let codex = config::codex_params(&paths)?;
    let verdict: CodexVerdict = client::call(&paths, CODEX_QUOTA, &CodexQuotaParams { codex })?;
    judged(&verdict)
}

fn judged(verdict: &CodexVerdict) -> Result<Done, Failure> {
    let codex = &verdict.codex;
    let now = clock::now().unix_millis();
    let windows: Vec<String> = codex
        .windows
        .iter()
        .map(|reading| {
            reading.describe_with_reset(now, |resets| {
                Timestamp::from_unix_millis(resets).to_string()
            })
        })
        .collect();
    let windows = windows.join(", ");
    let plan = match (&codex.plan, codex.reset_credits) {
        (Some(plan), 0) => format!("; plan {plan}"),
        (Some(plan), credits) => format!("; plan {plan}, {credits} reset credits"),
        (None, 0) => String::new(),
        (None, credits) => format!("; {credits} reset credits"),
    };
    if let (ClaudeState::Hold, Some(until)) = (codex.verdict, codex.until) {
        let until = Timestamp::from_unix_millis(until);
        // A hold that a standing reading names holds though the last ask could not be read: both
        // are said.
        let unreadable = codex
            .unreadable_line()
            .map_or_else(String::new, |line| format!("; {line}"));
        return Err(Failure::new(
            Exit::Busy,
            format!(
                "codex: hold: {}; hold new reviews on codex until {until}; {windows}{plan}{unreadable}",
                codex.hold.as_deref().unwrap_or("limit hit"),
            ),
        ));
    }
    let line = match (&codex.unreadable_line(), windows.is_empty()) {
        (Some(line), _) => format!("{line}; dispatch"),
        (None, true) => "codex: no plan window seen; dispatch".to_owned(),
        (None, false) => format!("codex: {windows}{plan}; dispatch"),
    };
    let json = serde_json::to_value(verdict).unwrap_or(Value::Null);
    Ok(Done::new(line, json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::status::Codex;

    #[test]
    fn a_hold_that_a_failed_ask_leaves_standing_says_both() {
        let verdict = CodexVerdict {
            codex: Codex {
                verdict: ClaudeState::Hold,
                until: Some(1_795_831_256_000),
                hold: Some("weekly 92%".to_owned()),
                unreadable: Some("it did not answer within 20s".to_owned()),
                ..Codex::default()
            },
            hold_at: 90.0,
        };

        let failure = judged(&verdict).err();

        let message = failure.map(|f| f.message).unwrap_or_default();
        assert!(
            message.starts_with("codex: hold: weekly 92%; "),
            "{message}"
        );
        assert!(
            message.ends_with("; codex usage unreadable: it did not answer within 20s"),
            "{message}"
        );
    }
}
