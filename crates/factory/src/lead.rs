//! `factory lead list|status` and `factory wait lead`: what the leads in herdr are doing, from
//! the daemon, which follows herdr's events.

use std::env;
use std::time::Duration;

use factory_core::exit::Exit;
use factory_core::lead::lead_name;
use serde_json::{Value, json};

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{
    LEAD_LIST, LEAD_STATUS, LeadList, LeadParams, LeadReport, WAIT_LEAD, WaitLeadParams,
    WaitLeadResult,
};

pub fn list() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let list: LeadList = client::call(&paths, LEAD_LIST, &json!({}))?;
    let json = serde_json::to_value(&list).unwrap_or(Value::Null);
    let mut lines = list.leads.iter().map(line);
    let Some(first) = lines.next() else {
        return Ok(Done::new("no leads in herdr", json));
    };
    let mut done = Done::new(first, json);
    done.detail = lines.collect();
    Ok(done)
}

pub fn status(lead: &str) -> Result<Done, Failure> {
    let params = LeadParams {
        lead: lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?,
    };
    let paths = Paths::from_env()?;
    let report: LeadReport = client::call(&paths, LEAD_STATUS, &params)?;
    Ok(shown(&report, Exit::Ok))
}

pub fn wait(lead: &str, timeout: Duration) -> Result<Done, Failure> {
    let params = WaitLeadParams {
        lead: lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?,
        waiter: env::var("HERDR_PANE_ID")
            .ok()
            .filter(|pane| !pane.is_empty()),
        timeout_secs: timeout.as_secs(),
    };
    let paths = Paths::from_env()?;
    let answer: WaitLeadResult = client::wait(&paths, WAIT_LEAD, &params, timeout)?;
    let exit = Exit::from_code(answer.exit).unwrap_or(Exit::NeedsAttention);
    let mut done = shown(&answer.lead, exit);
    done.json = serde_json::to_value(&answer).unwrap_or(Value::Null);
    Ok(done)
}

/// A lead's line, then its pane's last lines when it is blocked or asking.
fn shown(report: &LeadReport, exit: Exit) -> Done {
    let mut done = Done::new(
        line(report),
        serde_json::to_value(report).unwrap_or(Value::Null),
    );
    done.detail.clone_from(&report.lines);
    done.exit = exit;
    done
}

fn line(report: &LeadReport) -> String {
    match &report.report {
        Some(path) if report.state == "finished" => format!("{} finished: {path}", report.lead),
        _ => format!("{} {}", report.lead, report.status),
    }
}
