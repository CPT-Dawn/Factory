//! `factory bar`: the bar feed. One daemon call for the whole snapshot, and no other process;
//! factory-core's feed builder turns it into one line, or with `--json` into Zenith's cockpit v1
//! document. The factory measures, the bar displays (ADR 0001).
//!
//! With the daemon unreachable, `--json` still prints a valid document, one critical "factory
//! daemon down" chip, and exits 0: the bar says what is wrong rather than going blank. Without
//! `--json` it follows the exit-code table.

use std::process::ExitCode;

use factory_core::bar::{self, Snapshot};
use serde_json::{Value, json};

use crate::client::{self, ANSWER_TIMEOUT};
use crate::clock;
use crate::config;
use crate::leads::Leads;
use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::protocol::{BAR, BarParams};
use crate::quotas::QuotaKeeper;
use crate::status;
use crate::steward::Steward;

pub fn run(json: bool) -> ExitCode {
    match ask() {
        Ok(snapshot) => {
            let feed = serde_json::to_value(bar::feed(&snapshot))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            output::finish(Ok(Done::new(bar::line(&snapshot), feed)), json)
        }
        Err(failure) if json => {
            let feed = bar::daemon_down(&failure.message, clock::now().unix_millis());
            let feed = serde_json::to_value(feed).unwrap_or(Value::Null);
            output::finish(Ok(Done::new("factory daemon down", feed)), true)
        }
        Err(failure) => output::finish(
            Err(Failure::new(
                failure.exit,
                format!("factory daemon down: {}", failure.message),
            )),
            false,
        ),
    }
}

/// The snapshot, from a daemon that is already running.
fn ask() -> Result<Snapshot, Failure> {
    let paths = Paths::from_env()?;
    let claude_hold_at = config::claude_hold_at(&paths)?;
    client::call_running(&paths, BAR, &BarParams { claude_hold_at }, ANSWER_TIMEOUT)
}

/// The daemon's side: everything the bar shows, from memory.
pub fn snapshot(
    params: &BarParams,
    leads: &Leads,
    steward: &Steward,
    quotas: &QuotaKeeper,
) -> Snapshot {
    Snapshot {
        now: clock::now().unix_millis(),
        claude: quotas.claude(params.claude_hold_at),
        waits: leads.registered(),
        leads: status::leads_seen(leads, steward),
    }
}
