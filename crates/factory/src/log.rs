//! `factory log <type> key=value…`: appends one event to the ledger through the daemon.

use std::env;

use factory_core::event::Event;
use serde_json::Value;

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{LOG, LogParams, LogResult};

pub fn run(kind: &str, pairs: &[String]) -> Result<Done, Failure> {
    let session = env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let event = Event::from_args(kind, pairs, session.as_deref())
        .map_err(|e| Failure::usage(e.to_string()))?;
    let paths = Paths::from_env()?;
    let params = LogParams {
        kind: event.kind().to_owned(),
        fields: event.fields().clone(),
    };
    let landed: LogResult = client::call(&paths, LOG, &params)?;
    let ts = landed
        .event
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or("?");
    Ok(Done {
        line: format!("logged {kind} at {ts}"),
        json: serde_json::to_value(&landed).unwrap_or(Value::Null),
    })
}
