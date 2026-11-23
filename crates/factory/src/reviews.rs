//! `factory wait review`: blocks, silent, until the review of a label has ended, and prints
//! its one-line result.
//!
//! The ledger's `review` event is the end of a review. The daemon also notes when each review
//! asked for its slot, so a wait on a label that is being run again waits for that run's event
//! instead of answering from the last run's.

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_adapters::git::{GitRepo, Repo};
use factory_core::duration;
use factory_core::exit::Exit;
use factory_core::finished;
use factory_core::review::check_label;
use factory_core::timestamp::Timestamp;
use serde_json::Value;
use tokio::sync::Notify;

use crate::client;
use crate::clock;
use crate::ledger::Ledger;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{ReviewRef, WAIT_REVIEW, WaitReviewParams};
use factory_core::event::Fields;

pub fn wait(label: &str, timeout: Duration) -> Result<Done, Failure> {
    check_label(label).map_err(|e| Failure::usage(e.to_string()))?;
    let paths = Paths::from_env()?;
    // The repo the review ran in, as the review named it; a directory that is no repo waits
    // for the label in any.
    let repo = env::current_dir()
        .ok()
        .and_then(|cwd| GitRepo::at(&cwd).name().ok());
    let waiter = client::waiter();
    let params = |resume, timeout_secs| WaitReviewParams {
        label: label.to_owned(),
        repo: repo.clone(),
        waiter: waiter.clone(),
        timeout_secs,
        resume,
    };
    let event: Fields = client::hold(&paths, WAIT_REVIEW, params, timeout)?;
    let ended = finished::of(&event);
    let mut json = event;
    json.insert("exit".to_owned(), Value::from(ended.exit.code()));
    let mut done = Done::new(ended.line, Value::Object(json));
    done.exit = ended.exit;
    // Whole, as `factory review`'s line is: it ends in the report's path.
    done.whole = true;
    Ok(done)
}

/// When each review last asked for a slot, and the waits that watch for one to end.
#[derive(Default)]
pub struct Reviews {
    started: Mutex<HashMap<ReviewRef, Timestamp>>,
    changed: Notify,
}

impl Reviews {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ReviewRef, Timestamp>> {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Notes that `review` is under way now: an event of its label older than this is the end
    /// of an earlier run, not of this one.
    pub fn started(&self, review: ReviewRef) {
        self.lock().insert(review, clock::now());
    }

    /// Wakes the waits: a `review` event landed.
    pub fn landed(&self) {
        self.changed.notify_waiters();
    }

    /// When the latest run of `label` (in `repo`, when one is named) began, if this daemon saw it.
    pub fn began(&self, repo: Option<&str>, label: &str) -> Option<Timestamp> {
        self.lock()
            .iter()
            .filter(|(review, _)| {
                review.label == label && repo.is_none_or(|repo| review.repo == repo)
            })
            .map(|(_, began)| *began)
            .max()
    }

    /// The `review` event that ended the latest run of `label`, a run that began at `since` or
    /// later (the run the wait began on, which an earlier daemon may have seen). Fails with exit
    /// 124 when none lands by `deadline`, `limit` after the wait began.
    pub async fn wait(
        &self,
        ledger: &Arc<Ledger>,
        params: &WaitReviewParams,
        since: Option<Timestamp>,
        deadline: tokio::time::Instant,
        limit: Duration,
    ) -> Result<Fields, Failure> {
        loop {
            // Enabled before the look, so an end that lands during it is not missed.
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            let began = self.began(params.repo.as_deref(), &params.label).max(since);
            let found = {
                let ledger = Arc::clone(ledger);
                let (repo, label) = (params.repo.clone(), params.label.clone());
                tokio::task::spawn_blocking(move || ledger.last_review(repo.as_deref(), &label))
                    .await
                    .map_err(|e| {
                        Failure::precondition(format!(
                            "reading the ledger failed in the daemon: {e}; retry"
                        ))
                    })?
                    .map_err(|e| {
                        Failure::precondition(format!(
                            "cannot read the ledger: {e}; check FACTORY_STATE and XDG_STATE_HOME"
                        ))
                    })?
            };
            if let Some(event) = found
                && ended_since(&event, began)
            {
                return Ok(event);
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(Failure::new(
                    Exit::Timeout,
                    format!(
                        "no review {} ended within {}; wait again",
                        params.label,
                        duration::show(limit)
                    ),
                ));
            }
        }
    }
}

/// Whether `event` landed after the run that began at `began`; any event ends a run no one saw
/// begin.
fn ended_since(event: &Fields, began: Option<Timestamp>) -> bool {
    let landed = event
        .get("ts")
        .and_then(Value::as_str)
        .and_then(Timestamp::parse);
    match (began, landed) {
        (Some(began), Some(landed)) => landed >= began,
        _ => true,
    }
}
