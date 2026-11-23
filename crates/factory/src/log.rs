//! `factory log <type> key=value…`: appends one event to the ledger through the daemon.

use std::env;

use factory_core::event::Event;
use serde_json::Value;

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{LOG, LogParams, LogResult};

pub fn run(kind: &str, pairs: &[String]) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let landed = land(&paths, kind, pairs)?;
    let ts = landed
        .event
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or("?");
    Ok(Done::new(
        format!("logged {kind} at {ts}"),
        serde_json::to_value(&landed).unwrap_or(Value::Null),
    ))
}

/// Lands the event `kind` with `key=value` `pairs` in the ledger, through the daemon, with
/// the Claude Code session it was logged from.
pub fn land<S: AsRef<str>>(paths: &Paths, kind: &str, pairs: &[S]) -> Result<LogResult, Failure> {
    let session = env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let event = Event::from_args(kind, pairs, session.as_deref())
        .map_err(|e| Failure::usage(e.to_string()))?;
    let params = LogParams {
        kind: event.kind().to_owned(),
        fields: event.fields().clone(),
    };
    client::call(paths, LOG, &params)
}
