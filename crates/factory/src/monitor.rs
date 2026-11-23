//! `factory monitor start|stop|status`: the context steward, which runs in the daemon.

use serde_json::{Value, json};

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{MONITOR_START, MONITOR_STATUS, MONITOR_STOP, MonitorStatus, MonitorSwitch};

pub fn start() -> Result<Done, Failure> {
    switch(MONITOR_START, "monitor started", "monitor already running")
}

pub fn stop() -> Result<Done, Failure> {
    switch(MONITOR_STOP, "monitor stopped", "monitor was not running")
}

pub fn status() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let status: MonitorStatus = client::call(&paths, MONITOR_STATUS, &json!({}))?;
    let json = serde_json::to_value(&status).unwrap_or(Value::Null);
    Ok(Done::new(line(&status), json))
}

fn switch(method: &str, changed: &str, unchanged: &str) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let switch: MonitorSwitch = client::call(&paths, method, &json!({}))?;
    let json = serde_json::to_value(&switch).unwrap_or(Value::Null);
    Ok(Done::new(
        if switch.changed { changed } else { unchanged },
        json,
    ))
}

/// `monitor off`, or how many leads and subagents it watches and the one with the most context.
fn line(status: &MonitorStatus) -> String {
    if !status.running {
        return "monitor off".to_owned();
    }
    let subagents: usize = status.leads.iter().map(|lead| lead.subagents.len()).sum();
    let mut line = format!(
        "monitor on: {}, {}",
        count(status.leads.len(), "lead"),
        count(subagents, "subagent")
    );
    let sessions = status.leads.iter().flat_map(|lead| {
        std::iter::once((lead.lead.clone(), lead.tokens)).chain(
            lead.subagents
                .iter()
                .map(|sub| (format!("{}/{}", lead.lead, sub.agent), sub.tokens)),
        )
    });
    let fullest = sessions
        .filter_map(|(name, tokens)| Some((tokens?, name)))
        .max_by_key(|(tokens, _)| *tokens);
    if let Some((tokens, name)) = fullest {
        line.push_str(&format!("; most context: {name} {}", kilo(tokens)));
    }
    line
}

fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// Tokens in thousands, as the steward's messages give them: `312k`.
pub fn kilo(tokens: u64) -> String {
    format!("{}k", tokens / 1000)
}
