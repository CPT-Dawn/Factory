//! `factory status`: one screen of what the daemon holds.
//!
//! The CLI sends the configured review backends (the config is the CLI's to read) and the daemon
//! answers from its own state: the lease store, the router's quotas, the picture of the leads, the
//! wait registry and the context steward's table. No call to adb, docker or herdr is made.

use factory_core::status::{self, Status};
use serde_json::json;

use crate::client;
use crate::config;
use crate::lab::service::Lab;
use crate::leads::Leads;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{BackendSlots, LeadContext, STATUS, StatusParams};
use crate::router::Router;
use crate::steward::Steward;

pub fn run() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let backends = config::review_backends(&paths)?
        .into_iter()
        .map(|backend| BackendSlots {
            name: backend.name,
            concurrency: backend.concurrency,
            when_busy: backend.when_busy.into(),
        })
        .collect();
    let status: Status = client::call(&paths, STATUS, &StatusParams { backends })?;
    let mut rows = status::screen(&status).into_iter();
    let mut done = Done::new(
        rows.next().unwrap_or_default(),
        serde_json::to_value(&status).unwrap_or_else(|e| json!({"error": e.to_string()})),
    );
    done.more = rows.collect();
    Ok(done)
}

/// The daemon's side: every section from memory or the lease store.
pub async fn gather(
    params: &StatusParams,
    router: &Router,
    lab: &std::sync::Arc<Lab>,
    leads: &Leads,
    steward: &Steward,
) -> Result<Status, Failure> {
    let now = crate::clock::now();
    let lab = lab
        .overview()
        .await
        .map_err(|e| Failure::precondition(e.message))?;
    let (error, reports) = leads.picture();
    let monitor = steward.status();
    let leads_seen = reports
        .into_iter()
        .map(|report| {
            let context = monitor.leads.iter().find(|ctx| ctx.lead == report.lead);
            lead(report, context)
        })
        .collect();
    Ok(Status {
        now: now.unix_millis(),
        lab,
        review: router.snapshot(&params.backends, now),
        waits: leads.registered(),
        leads: status::Leads {
            error,
            monitor: monitor.running,
            leads: leads_seen,
        },
    })
}

fn lead(report: crate::protocol::LeadReport, context: Option<&LeadContext>) -> status::Lead {
    status::Lead {
        name: report.lead,
        state: report.state,
        status: report.status,
        pane: report.pane,
        report: report.report,
        session: context.and_then(|ctx| ctx.session.clone()),
        tokens: context.and_then(|ctx| ctx.tokens),
        asked: context.is_some_and(|ctx| ctx.asked),
        compacting: context.is_some_and(|ctx| ctx.compacting),
        subagents: context.map_or_else(Vec::new, |ctx| {
            ctx.subagents
                .iter()
                .map(|sub| status::Subagent {
                    agent: sub.agent.clone(),
                    description: sub.description.clone(),
                    tokens: sub.tokens,
                    told: sub.told,
                })
                .collect()
        }),
    }
}

