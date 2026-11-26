//! `factory bar`: the bar feed. One daemon call for the whole snapshot, and no other process;
//! factory-core's feed builder turns it into one line, or with `--json` into Zenith's cockpit v1
//! document. The factory measures, the bar displays (ADR 0001).
//!
//! With the daemon unreachable (nothing listens, it hangs up or it doesn't answer in time),
//! `--json` still prints a valid document, one critical "factory daemon down" chip, and exits 0:
//! the bar says what is wrong rather than going blank. Without `--json`, and for any other
//! failure (a bad config, a daemon that refuses), it follows the exit-code table.

use std::process::ExitCode;
use std::sync::Arc;

use factory_core::bar::{self, Snapshot};
use serde_json::{Value, json};

use crate::client::{self, ANSWER_TIMEOUT, Unanswered};
use crate::clock;
use crate::config;
use crate::leads::Leads;
use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::protocol::{BAR, BarParams};
use crate::quotas::{AGY_GLANCE, QuotaKeeper};
use crate::status;
use crate::steward::Steward;

pub fn run(json: bool) -> ExitCode {
    let (paths, params) = match request() {
        Ok(request) => request,
        Err(failure) => return output::finish(Err(failure), json),
    };
    match client::call_running::<_, Snapshot>(&paths, BAR, &params, ANSWER_TIMEOUT) {
        Ok(snapshot) => {
            let feed = serde_json::to_value(bar::feed(&snapshot))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            output::finish(Ok(Done::new(bar::line(&snapshot), feed)), json)
        }
        Err(Unanswered::Down(failure)) if json => {
            let feed = bar::daemon_down(&failure.message, clock::now().unix_millis());
            let feed = serde_json::to_value(feed).unwrap_or(Value::Null);
            output::finish(Ok(Done::new("factory daemon down", feed)), true)
        }
        Err(Unanswered::Down(failure)) => output::finish(
            Err(Failure::new(
                failure.exit,
                format!("factory daemon down: {}", failure.message),
            )),
            false,
        ),
        // The daemon is up and said no: the exit-code table as usual.
        Err(Unanswered::Refused(failure)) => output::finish(Err(failure), json),
    }
}

/// The request, from the paths and the config: a failure here is no daemon's, and keeps the
/// exit-code table.
fn request() -> Result<(Paths, BarParams), Failure> {
    let paths = Paths::from_env()?;
    let claude_hold_at = config::claude_hold_at(&paths)?;
    Ok((paths, BarParams { claude_hold_at }))
}

/// The daemon's side: everything the bar shows, from memory, and agy's last reading (its usage
/// is asked at most every five minutes, and never waited on for long).
pub async fn snapshot(
    params: &BarParams,
    leads: &Leads,
    steward: &Steward,
    quotas: &Arc<QuotaKeeper>,
    lab: bar::lab::Lab,
) -> Snapshot {
    Snapshot {
        now: clock::now().unix_millis(),
        claude: quotas.claude(params.claude_hold_at),
        agy: quotas.agy(AGY_GLANCE).await,
        waits: leads.registered(),
        leads: status::leads_seen(leads, steward),
        lab,
    }
}
