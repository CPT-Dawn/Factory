//! The daemon's reviewer router: each backend's quota, kept in the database so a restart
//! loses no cooldown, and the review slots each backend's concurrency allows.
//!
//! A review asks for a backend with the configured order and limits. The router answers with
//! the first one [`quota::pick`] takes, or keeps the request until a slot frees (or a cooldown
//! ends) when it says to wait. The connection that asked holds the slot until it asks again or
//! closes, so a review that dies frees its slot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::exit::Exit;
use factory_core::quota::{self, Candidate, Pick, Quota, WhenBusy};
use factory_core::status::{self, ClaudeState, QuotaState};
use factory_core::timestamp::Timestamp;
use serde_json::Value;
use tokio::sync::Notify;

use crate::clock;
use crate::output::Failure;
use crate::protocol::{
    AcquireParams, AcquireResult, BackendSlots, MISMATCH, Outcome, QuotaParam, QuotaParams,
};
use crate::quotas::QuotaKeeper;
use crate::store::Store;

/// The longest a review waits on a Codex hold before it looks at the plan again: a reading is
/// stale after 5 minutes, and a hold can last until a week's window resets.
const HOLD_LOOK: Duration = Duration::from_secs(300);

/// The CLI whose plan the quota keeper watches, by the name a backend gives it.
const CODEX: &str = "codex";

/// What holds reviews on Codex, from the plan's windows: when the hold ends and what holds.
struct Hold {
    until: Timestamp,
    by: String,
}

impl Hold {
    fn of(codex: &status::Codex) -> Option<Hold> {
        match (codex.verdict, codex.until, &codex.hold) {
            (ClaudeState::Hold, Some(until), Some(by)) => Some(Hold {
                until: Timestamp::from_unix_millis(until),
                by: by.clone(),
            }),
            _ => None,
        }
    }

    /// `quota` as it stands at `now`, cooling at least until the hold ends.
    fn over(&self, quota: Quota, now: Timestamp) -> Quota {
        match quota.at(now) {
            Quota::CoolingDown { until } if until > self.until => Quota::CoolingDown { until },
            _ => Quota::CoolingDown { until: self.until },
        }
    }
}

pub struct Router {
    store: Mutex<Store>,
    state: Mutex<State>,
    /// Woken when a slot frees or a quota changes, so waiting reviews ask the router again.
    changed: Notify,
}

#[derive(Default)]
struct State {
    quotas: HashMap<String, Quota>,
    /// Why a backend's quota is unknown, as the review that could not read it said. Kept in
    /// memory only: the next review that tries the backend says again.
    unknown: HashMap<String, String>,
    running: HashMap<String, u32>,
    /// The tier of each review running on a backend, for `factory status`.
    tiers: HashMap<String, Vec<String>>,
}

/// The review slot a connection holds: its backend, and the tier of the review in it.
#[derive(Debug)]
pub struct Held {
    backend: String,
    tier: Option<String>,
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
                unknown: HashMap::new(),
                running: HashMap::new(),
                tiers: HashMap::new(),
            }),
            changed: Notify::new(),
        })
    }

    /// Takes a slot for the connection that `held` belongs to, first freeing the one it holds.
    ///
    /// A backend that runs on Codex is held off while the plan's windows say so: its quota
    /// cools down until the hold ends, so the review goes to the next backend of its chain, or
    /// is refused with exit 75 when none is left.
    pub async fn acquire(
        &self,
        keeper: &Arc<QuotaKeeper>,
        params: Value,
        held: &mut Option<Held>,
    ) -> Outcome {
        self.release(held);
        let params: AcquireParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(e) => return refuse(Exit::Usage, format!("bad acquire params: {e}; {MISMATCH}")),
        };
        let wanted: Vec<Wanted> = params
            .backends
            .iter()
            .map(|backend| Wanted {
                name: backend.name.as_str(),
                concurrency: backend.concurrency,
                when_busy: backend.when_busy.into(),
                codex: backend.cli.as_deref() == Some(CODEX),
            })
            .collect();
        let has_codex = wanted.iter().any(|backend| backend.codex);
        loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            // A first look, from the quotas as they stand now: only a review that is about to take
            // Codex asks about its plan, and it waits for the answer, so none starts on a plan
            // that a reading on its way would have held.
            // The hold the stored reading names is laid over it, so a held Codex is stepped over
            // even when its slots are full, and the ask starts under the same lock as the pick.
            // A review that a hold keeps off Codex, to wait or to be refused, asks too: a stale
            // hold is looked at again before it turns a review away, and every 5 minutes while
            // one waits on it.
            let (takes_codex, asking) = {
                let state = lock(&self.state);
                let standing = Hold::of(&keeper.codex_standing(&params.codex));
                let now = clock::now();
                let candidates = candidates(&state, &wanted, standing.as_ref(), now);
                let pick = quota::pick(&candidates, now);
                let takes_codex = matches!(pick, Pick::Take(index) if wanted[index].codex);
                let held_off = has_codex && standing.is_some() && !matches!(pick, Pick::Take(_));
                (
                    takes_codex,
                    (takes_codex || held_off) && keeper.ask_codex_for_dispatch(&params.codex),
                )
            };
            if asking {
                keeper.codex_asked().await;
            }
            let now = clock::now();
            let (next_reset, hold) = {
                let mut state = lock(&self.state);
                // The hold as it stands under the lock the grant is made in: a reading that
                // landed since the first look, whoever asked for it, is the one this pick obeys.
                let hold = Hold::of(&keeper.codex_standing(&params.codex));
                let candidates = candidates(&state, &wanted, hold.as_ref(), now);
                let next_reset = match quota::pick(&candidates, now) {
                    Pick::Take(index) => {
                        if wanted[index].codex && !takes_codex {
                            // The slot the first look counted on went to another review, so the
                            // pick fell to Codex, whose plan nobody has settled: look again. The
                            // next look settles it only if Codex is still the one it would take.
                            continue;
                        }
                        let name = wanted[index].name.to_owned();
                        *state.running.entry(name.clone()).or_default() += 1;
                        if let Some(tier) = &params.tier {
                            state
                                .tiers
                                .entry(name.clone())
                                .or_default()
                                .push(tier.clone());
                        }
                        *held = Some(Held {
                            backend: name.clone(),
                            tier: params.tier.clone(),
                        });
                        let result = AcquireResult { backend: name };
                        return Outcome::Result(
                            serde_json::to_value(result).unwrap_or(Value::Null),
                        );
                    }
                    Pick::OutOfQuota { until } => {
                        return out_of_quota(&wanted, &candidates, until, hold.as_ref());
                    }
                    Pick::Wait => candidates
                        .iter()
                        .filter_map(|candidate| match candidate.quota.at(now) {
                            Quota::CoolingDown { until } => Some(until),
                            _ => None,
                        })
                        .min(),
                };
                (next_reset, hold)
            };
            match next_reset {
                Some(reset) => {
                    let mut wait = until(reset, now);
                    if hold.is_some() {
                        wait = wait.min(HOLD_LOOK);
                    }
                    let _ = tokio::time::timeout(wait, changed).await;
                }
                None => changed.await,
            }
        }
    }

    /// Each of `backends` with its quota as it stands at `now` and the reviews running on it,
    /// from memory alone.
    /// A backend that runs on Codex shows as cooling, with why, while `codex` holds it.
    pub fn snapshot(
        &self,
        backends: &[BackendSlots],
        now: Timestamp,
        codex: &status::Codex,
    ) -> Vec<status::Backend> {
        let hold = Hold::of(codex);
        let state = lock(&self.state);
        backends
            .iter()
            .map(|backend| {
                let stored = state
                    .quotas
                    .get(&backend.name)
                    .copied()
                    .unwrap_or(Quota::Unknown);
                let held = hold
                    .as_ref()
                    .filter(|_| backend.cli.as_deref() == Some(CODEX));
                let quota = match held {
                    Some(hold) => hold.over(stored, now),
                    None => stored.at(now),
                };
                let (quota, resets, reason) = match quota {
                    Quota::Unknown => (
                        QuotaState::Unknown,
                        None,
                        state.unknown.get(&backend.name).cloned(),
                    ),
                    Quota::Available => (QuotaState::Available, None, None),
                    Quota::CoolingDown { until } => (
                        QuotaState::Cooling,
                        Some(until.unix_millis()),
                        held.map(|hold| format!("held at {}", hold.by)),
                    ),
                };
                status::Backend {
                    name: backend.name.clone(),
                    quota,
                    resets,
                    reason,
                    running: state.running.get(&backend.name).copied().unwrap_or(0),
                    tiers: {
                        let mut tiers = state.tiers.get(&backend.name).cloned().unwrap_or_default();
                        tiers.sort();
                        tiers
                    },
                    concurrency: backend.concurrency,
                    when_busy: match WhenBusy::from(backend.when_busy) {
                        WhenBusy::Queue => "queue",
                        WhenBusy::Next => "next",
                    }
                    .to_owned(),
                }
            })
            .collect()
    }

    /// Frees the slot `held` names, if any, and wakes the reviews waiting for one.
    pub fn release(&self, held: &mut Option<Held>) {
        let Some(Held { backend, tier }) = held.take() else {
            return;
        };
        let mut state = lock(&self.state);
        if let Some(running) = state.running.get_mut(&backend) {
            *running = running.saturating_sub(1);
        }
        if let Some(tiers) = state.tiers.get_mut(&backend)
            && let Some(at) = tier.and_then(|tier| tiers.iter().position(|t| *t == tier))
        {
            tiers.remove(at);
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Records what a review learned of a backend's quota, in the database, then in the
    /// router. A finished review, or one that could not read the quota, leaves a cooldown
    /// another review hit meanwhile in place.
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
        let quota = known.learn((&params.quota).into(), clock::now());
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
        match (&params.quota, quota) {
            (QuotaParam::Unknown { reason }, Quota::Unknown) => {
                state.unknown.insert(params.backend.clone(), reason.clone());
            }
            _ => {
                state.unknown.remove(&params.backend);
            }
        }
        drop(state);
        self.changed.notify_waiters();
        Outcome::Result(serde_json::to_value(params).unwrap_or(Value::Null))
    }
}

/// One backend of a review's chain, as a pick weighs it.
struct Wanted<'a> {
    name: &'a str,
    concurrency: u32,
    when_busy: WhenBusy,
    /// It runs on Codex, so a Codex hold is laid over its quota.
    codex: bool,
}

/// The candidates for a pick at `now`: each backend's quota, with a Codex hold laid over those
/// that run on Codex, and its load.
fn candidates(
    state: &State,
    wanted: &[Wanted],
    hold: Option<&Hold>,
    now: Timestamp,
) -> Vec<Candidate> {
    wanted
        .iter()
        .map(|backend| {
            let stored = state
                .quotas
                .get(backend.name)
                .copied()
                .unwrap_or(Quota::Unknown);
            Candidate {
                quota: match hold {
                    Some(hold) if backend.codex => hold.over(stored, now),
                    _ => stored,
                },
                running: state.running.get(backend.name).copied().unwrap_or(0),
                concurrency: backend.concurrency,
                when_busy: backend.when_busy,
            }
        })
        .collect()
}

/// Every backend is cooling down: exit 75, naming when each resets and the first reset.
fn out_of_quota(
    wanted: &[Wanted],
    candidates: &[Candidate],
    first: Option<Timestamp>,
    hold: Option<&Hold>,
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
        .filter_map(|(backend, candidate)| match candidate.quota {
            Quota::CoolingDown { until } => Some(match hold.filter(|_| backend.codex) {
                Some(hold) => format!("{} until {until}, held at {}", backend.name, hold.by),
                None => format!("{} until {until}", backend.name),
            }),
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
