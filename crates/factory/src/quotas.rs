//! The quota keeper: the daemon's one owner of every harness's latest usage readings, kept in
//! its database so a restart forgets no hold. Status, the bar and a dispatch all ask it; nothing
//! else reads or records a quota. Claude's two windows come in through `factory hook
//! statusline`, and its limit through the `StopFailure` hook. agy's come from its usage query,
//! which the keeper asks for itself, at most once in [`AGY_DISPLAY_FRESH`]: what status, the
//! bar and `factory quota agy` show is one reading, however often they look.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::claude::{ClaudeQuota, Verdict};
use factory_core::exit::Exit;
use factory_core::quota;
use factory_core::status::Agy;
use factory_core::timestamp::Timestamp;
use factory_core::usage::{self, UsageWindow, WindowKind};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::clock;
use crate::output::Failure;
use crate::protocol::{ClaudeLimitParams, ClaudeVerdict, ClaudeWindowParams, WaitClaudeParams};
use crate::store::Store;

/// How long agy's reading, good or not, is shown before its usage is asked again. Routing keeps
/// its own, shorter freshness check: a dispatch wants the newest answer, a bar poll does not.
pub const AGY_DISPLAY_FRESH: Duration = Duration::from_secs(300);

/// How long status and the bar wait for a usage query that is under way: past it they show the
/// last reading and the query finishes on its own, so a slow agy never holds a poll up.
pub const AGY_GLANCE: Duration = Duration::from_secs(4);

/// How long `factory quota agy` waits for a usage query: agy's own limit for it, and a margin.
pub const AGY_QUERY: Duration = Duration::from_secs(65);

/// agy's usage answer, or why there is none.
pub type AgyAsk = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

#[derive(Default)]
struct AgyState {
    /// The last reading, whatever it said.
    reading: Option<Agy>,
    /// Whether a usage query is under way: at most one is, whoever asks.
    asking: bool,
}

pub struct QuotaKeeper {
    store: Mutex<Store>,
    claude: Mutex<ClaudeQuota>,
    /// Woken when Claude's quota changes, so a waiting lead looks again.
    changed: Notify,
    agy: Mutex<AgyState>,
    /// Woken when agy's usage query ends.
    agy_asked: Notify,
    ask_agy: AgyAsk,
}

impl QuotaKeeper {
    pub fn open(store: Store) -> Result<QuotaKeeper, Failure> {
        let claude = store.claude_quota().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the Claude quota: {e}; move the database aside and restart"
            ))
        })?;
        Ok(QuotaKeeper {
            store: Mutex::new(store),
            claude: Mutex::new(claude),
            changed: Notify::new(),
            agy: Mutex::new(AgyState::default()),
            agy_asked: Notify::new(),
            ask_agy: Arc::new(|| {
                factory_adapters::reviewer::agy_usage_query(
                    factory_adapters::reviewer::AgyAuth::Subscription,
                )
            }),
        })
    }

    /// The keeper asking `ask` for agy's usage: how a test hands it a fake.
    #[cfg(test)]
    fn asking(mut self, ask: AgyAsk) -> QuotaKeeper {
        self.ask_agy = ask;
        self
    }

    /// agy's quota groups and their windows as of `AGY_DISPLAY_FRESH`: the reading while it is
    /// fresh, else a new usage query, which is never run twice at once. A caller waits for it
    /// at most `patience`; past that it gets the last reading (none, before the first), and
    /// the query goes on and lands for the next caller. An answer that can't be read is the
    /// reading too, with its reason: it is shown, and asked again after the same time.
    pub async fn agy(self: &Arc<Self>, patience: Duration) -> Agy {
        self.agy_at(clock::now(), patience).await
    }

    async fn agy_at(self: &Arc<Self>, now: Timestamp, patience: Duration) -> Agy {
        let mut asked = std::pin::pin!(self.agy_asked.notified());
        {
            let mut state = lock(&self.agy);
            if let Some(reading) = state.reading.as_ref().filter(|r| agy_fresh(r, now)) {
                return reading.clone();
            }
            // Listening before the query can end, so its end is never missed.
            asked.as_mut().enable();
            if !state.asking {
                state.asking = true;
                let keeper = Arc::clone(self);
                tokio::spawn(async move { keeper.ask_agy_now(now).await });
            }
        }
        let _ = tokio::time::timeout(patience, asked).await;
        lock(&self.agy).reading.clone().unwrap_or_default()
    }

    async fn ask_agy_now(&self, now: Timestamp) {
        let ask = Arc::clone(&self.ask_agy);
        let answer = tokio::task::spawn_blocking(move || ask())
            .await
            .unwrap_or_else(|e| Err(format!("failed: {e}")));
        let reading = read_agy(answer, now);
        {
            let mut state = lock(&self.agy);
            state.reading = Some(reading);
            state.asking = false;
        }
        self.agy_asked.notify_waiters();
    }

    /// A status line saw Claude's windows. A window whose share or reset is out of range keeps
    /// its last reading.
    pub fn see_claude(&self, params: &ClaudeWindowParams) -> Result<(), Failure> {
        self.change(|quota, now| {
            let windows: Vec<UsageWindow> = [
                (WindowKind::FiveHour, &params.five_hour),
                (WindowKind::Weekly, &params.seven_day),
            ]
            .into_iter()
            .filter_map(|(kind, reading)| {
                let reading = reading.as_ref()?;
                UsageWindow::read(kind, reading.used_percent, reading.resets, now)
            })
            .collect();
            quota.see(&windows, now);
        })
    }

    /// A Claude turn ended on the plan's limit. A reset out of range counts as none named.
    pub fn claude_limit(&self, params: &ClaudeLimitParams) -> Result<(), Failure> {
        let reset = params
            .reset
            .filter(|reset| usage::representable(*reset))
            .map(Timestamp::from_unix_millis);
        self.change(|quota, now| quota.hit_limit(reset, now))
    }

    fn change(&self, apply: impl FnOnce(&mut ClaudeQuota, Timestamp)) -> Result<(), Failure> {
        let now = clock::now();
        let mut quota = lock(&self.claude);
        let mut next = *quota;
        apply(&mut next, now);
        lock(&self.store).set_claude_quota(&next).map_err(|e| {
            Failure::precondition(format!(
                "cannot record the Claude quota: {e}; check the disk and FACTORY_STATE"
            ))
        })?;
        *quota = next;
        drop(quota);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Whether a lead may dispatch on Claude now, holding at `hold_at` percent of either window.
    pub fn claude_verdict(&self, hold_at: f64) -> ClaudeVerdict {
        ClaudeVerdict {
            claude: self.claude(hold_at),
            hold_at,
        }
    }

    /// Claude's quota as `factory status` and the bar record it.
    pub fn claude(&self, hold_at: f64) -> factory_core::status::Claude {
        lock(&self.claude).record(clock::now(), hold_at)
    }

    /// Answers once a lead may dispatch on Claude, or with exit 124 at `deadline`, `limit`
    /// after the wait began.
    pub async fn wait_claude(
        &self,
        params: &WaitClaudeParams,
        deadline: Instant,
        limit: Duration,
    ) -> Result<ClaudeVerdict, Failure> {
        loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            let now = clock::now();
            // One look at the quota: the answer is the verdict that ended the wait.
            let quota = *lock(&self.claude);
            let Verdict::Hold { until, by } = quota.verdict(now, params.hold_at) else {
                return Ok(ClaudeVerdict {
                    claude: quota.record(now, params.hold_at),
                    hold_at: params.hold_at,
                });
            };
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    Exit::Timeout,
                    format!(
                        "claude: still holding dispatches until {until} ({}) after {}; wait again",
                        by.describe(),
                        factory_core::duration::show(limit)
                    ),
                ));
            }
            let reset = Instant::now() + millis_until(until, now);
            let _ = tokio::time::timeout_at(reset.min(deadline), changed).await;
        }
    }
}

/// Whether `reading` is still the one to show at `now`.
fn agy_fresh(reading: &Agy, now: Timestamp) -> bool {
    let Some(seen) = reading.seen else {
        return false;
    };
    let age = now.unix_millis() - seen;
    (0..i64::try_from(AGY_DISPLAY_FRESH.as_millis()).unwrap_or(i64::MAX)).contains(&age)
}

/// What agy's usage `answer` says, taken at `now`: its quota groups, or why it can't be read.
fn read_agy(answer: Result<String, String>, now: Timestamp) -> Agy {
    let seen = Some(now.unix_millis());
    match answer.and_then(|usage| quota::agy_groups(&usage)) {
        Ok(groups) => Agy {
            groups,
            error: None,
            seen,
        },
        Err(why) => Agy {
            groups: Vec::new(),
            error: Some(why),
            seen,
        },
    }
}

fn millis_until(until: Timestamp, now: Timestamp) -> Duration {
    let millis = until.unix_millis().saturating_sub(now.unix_millis());
    Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const USAGE: &str = r#"{"command":{"data":{"groups":[{"name":"Gemini Models","buckets":[
        {"window":"5h","remaining_fraction":0.7,"reset_time":"2099-01-01T00:00:00Z"},
        {"window":"weekly","remaining_fraction":0.93,"reset_time":"2099-01-07T00:00:00Z"}]}]}}}"#;

    fn keeper(answers: Vec<Result<String, String>>) -> (Arc<QuotaKeeper>, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::open(&dir.path().join("factory.db")).expect("a store");
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let keeper = QuotaKeeper::open(store)
            .expect("a keeper")
            .asking(Arc::new(move || {
                let n = counted.fetch_add(1, Ordering::SeqCst);
                answers[n.min(answers.len() - 1)].clone()
            }));
        (Arc::new(keeper), asked)
    }

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_unix_millis(1_700_000_000_000 + secs * 1000)
    }

    #[tokio::test]
    async fn a_reading_is_shown_for_five_minutes_then_asked_again() {
        let (keeper, asked) = keeper(vec![Ok(USAGE.to_owned())]);
        let patience = Duration::from_secs(5);

        let first = keeper.agy_at(at(0), patience).await;
        assert_eq!(first.groups.len(), 1);
        assert_eq!(first.error, None);
        keeper.agy_at(at(299), patience).await;
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "a fresh reading is not asked again"
        );

        keeper.agy_at(at(301), patience).await;
        assert_eq!(asked.load(Ordering::SeqCst), 2, "a stale one is");
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_a_reading_that_is_shown_and_not_asked_again_at_once() {
        let (keeper, asked) = keeper(vec![Err("exited 127".to_owned())]);
        let patience = Duration::from_secs(5);

        let first = keeper.agy_at(at(0), patience).await;
        assert_eq!(first.error.as_deref(), Some("exited 127"));
        assert!(first.groups.is_empty());
        let second = keeper.agy_at(at(60), patience).await;
        assert_eq!(second, first);
        assert_eq!(asked.load(Ordering::SeqCst), 1);

        let garbage = keeper_garbage().await;
        assert_eq!(garbage.error.as_deref(), Some("its answer is not JSON"));
    }

    async fn keeper_garbage() -> Agy {
        let (keeper, _) = keeper(vec![Ok("not json".to_owned())]);
        keeper.agy_at(at(0), Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn a_slow_query_is_asked_once_however_many_look_and_does_not_hold_them() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::open(&dir.path().join("factory.db")).expect("a store");
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let keeper = Arc::new(QuotaKeeper::open(store).expect("a keeper").asking(Arc::new(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                Ok(USAGE.to_owned())
            },
        )));

        let impatient = keeper.agy_at(at(0), Duration::from_millis(20)).await;
        assert_eq!(
            impatient,
            Agy::default(),
            "no reading yet, and not waited for"
        );
        let patient = keeper.agy_at(at(1), Duration::from_secs(5)).await;
        assert_eq!(patient.groups.len(), 1);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }
}
