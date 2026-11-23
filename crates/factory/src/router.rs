//! The daemon's reviewer router: each backend's quota, kept in the database so a restart
//! loses no cooldown, and the review slots each backend's concurrency allows.
//!
//! A review asks for a backend with the configured order and limits. The router answers with
//! the first one [`quota::pick`] takes, or keeps the request until a slot frees (or a cooldown
//! ends) when it says to wait. The connection that asked holds the slot until it asks again or
//! closes, so a review that dies frees its slot.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use factory_core::exit::Exit;
use factory_core::quota::{self, Candidate, Pick, Quota, WhenBusy};
use factory_core::timestamp::Timestamp;
use serde_json::Value;
use tokio::sync::Notify;

use crate::clock;
use crate::output::Failure;
use crate::protocol::{AcquireParams, AcquireResult, MISMATCH, Outcome, QuotaParams};
use crate::store::Store;

pub struct Router {
    store: Mutex<Store>,
    state: Mutex<State>,
    /// Woken when a slot frees or a quota changes, so waiting reviews ask the router again.
    changed: Notify,
}

#[derive(Default)]
struct State {
    quotas: HashMap<String, Quota>,
    running: HashMap<String, u32>,
}

impl Router {
    pub fn open(store: Store) -> Result<Router, Failure> {
        let quotas = store.quotas().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the reviewer quotas: {e}; move the database aside and restart"
            ))
        })?;
        Ok(Router {
            store: Mutex::new(store),
            state: Mutex::new(State {
                quotas: quotas.into_iter().collect(),
                running: HashMap::new(),
            }),
            changed: Notify::new(),
        })
    }

    /// Takes a slot for the connection that `held` belongs to, first freeing the one it holds.
    pub async fn acquire(&self, params: Value, held: &mut Option<String>) -> Outcome {
        self.release(held);
        let params: AcquireParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(e) => return refuse(Exit::Usage, format!("bad acquire params: {e}; {MISMATCH}")),
        };
        let wanted: Vec<(&str, u32, WhenBusy)> = params
            .backends
            .iter()
            .map(|backend| {
                let when_busy = backend.when_busy.into();
                (backend.name.as_str(), backend.concurrency, when_busy)
            })
            .collect();
        loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            let now = clock::now();
            let next_reset = {
                let mut state = lock(&self.state);
                let candidates: Vec<Candidate> = wanted
                    .iter()
                    .map(|&(name, concurrency, when_busy)| Candidate {
                        quota: state.quotas.get(name).copied().unwrap_or(Quota::Unknown),
                        running: state.running.get(name).copied().unwrap_or(0),
                        concurrency,
                        when_busy,
                    })
                    .collect();
                match quota::pick(&candidates, now) {
                    Pick::Take(index) => {
                        let name = wanted[index].0.to_owned();
                        *state.running.entry(name.clone()).or_default() += 1;
                        *held = Some(name.clone());
                        let result = AcquireResult { backend: name };
                        return Outcome::Result(
                            serde_json::to_value(result).unwrap_or(Value::Null),
                        );
                    }
                    Pick::OutOfQuota { until } => return out_of_quota(&wanted, &candidates, until),
                    Pick::Wait => candidates
                        .iter()
                        .filter_map(|candidate| match candidate.quota.at(now) {
                            Quota::CoolingDown { until } => Some(until),
                            _ => None,
                        })
                        .min(),
                }
            };
            match next_reset {
                Some(reset) => {
                    let _ = tokio::time::timeout(until(reset, now), changed).await;
                }
                None => changed.await,
            }
        }
    }

    /// Frees the slot `held` names, if any, and wakes the reviews waiting for one.
    pub fn release(&self, held: &mut Option<String>) {
        let Some(name) = held.take() else {
            return;
        };
        if let Some(running) = lock(&self.state).running.get_mut(&name) {
            *running = running.saturating_sub(1);
        }
        self.changed.notify_waiters();
    }

    /// Records what a review learned of a backend's quota, in the database, then in the
    /// router. A finished review leaves a cooldown another review hit meanwhile in place.
    pub fn record(&self, params: Value) -> Outcome {
        let params: QuotaParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(e) => return refuse(Exit::Usage, format!("bad quota params: {e}; {MISMATCH}")),
        };
        let mut state = lock(&self.state);
        let known = state
            .quotas
            .get(&params.backend)
            .copied()
            .unwrap_or(Quota::Unknown);
        let quota = known.learn(params.quota.into(), clock::now());
        if let Err(e) = lock(&self.store).set_quota(&params.backend, quota) {
            return refuse(
                Exit::Precondition,
                format!(
                    "cannot record {}'s quota: {e}; check the disk and FACTORY_STATE",
                    params.backend
                ),
            );
        }
        state.quotas.insert(params.backend.clone(), quota);
        drop(state);
        self.changed.notify_waiters();
        Outcome::Result(serde_json::to_value(params).unwrap_or(Value::Null))
    }
}

/// Every backend is cooling down: exit 75, naming when each resets and the first reset.
fn out_of_quota(
    wanted: &[(&str, u32, WhenBusy)],
    candidates: &[Candidate],
    first: Option<Timestamp>,
) -> Outcome {
    let Some(first) = first else {
        return refuse(
            Exit::Precondition,
            "no review backend is configured; add a [[review.backend]] to the config".to_owned(),
        );
    };
    let each: Vec<String> = wanted
        .iter()
        .zip(candidates)
        .filter_map(|(&(name, ..), candidate)| match candidate.quota {
            Quota::CoolingDown { until } => Some(format!("{name} until {until}")),
            _ => None,
        })
        .collect();
    refuse(
        Exit::Busy,
        format!(
            "every review backend is out of quota ({}); retry after {first}",
            each.join(", ")
        ),
    )
}

/// How long from `now` to `reset`.
fn until(reset: Timestamp, now: Timestamp) -> Duration {
    let millis = reset.unix_millis().saturating_sub(now.unix_millis());
    Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}

fn refuse(exit: Exit, message: String) -> Outcome {
    Outcome::refused(exit, message)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
