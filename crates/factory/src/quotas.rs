//! The quota keeper: the daemon's one owner of every harness's latest usage readings. Claude's
//! are kept in its database so a restart forgets no hold; agy's are held in memory and asked
//! again after a restart. Status, the bar and a dispatch's Claude hold all ask it;
//! nothing else reads or records a quota, but agy's routing, which keeps its own 120 s look. Claude's two windows come in through `factory hook
//! statusline`, and its limit through the `StopFailure` hook. agy's come from its usage query,
//! which the keeper asks for itself, at most once in [`AGY_DISPLAY_FRESH_MS`]: what status, the
//! bar and `factory quota agy` show is one reading, however often they look.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_adapters::reviewer::AgyUsage;
use factory_core::claude::{ClaudeQuota, Verdict};
use factory_core::exit::Exit;
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
pub const AGY_DISPLAY_FRESH_MS: i64 = 300_000;

/// How long status and the bar wait for a usage query that is under way: past it they show the
/// last reading and the query finishes on its own, so a slow agy never holds a poll up.
pub const AGY_GLANCE: Duration = Duration::from_secs(4);

/// How long `factory quota agy` waits for a usage query: agy's own limit for it, and a margin.
pub const AGY_QUERY: Duration = Duration::from_secs(65);

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
    usage: Arc<dyn AgyUsage>,
}

impl QuotaKeeper {
    /// The keeper over `store`, asking agy's usage through `usage`.
    pub fn open(store: Store, usage: Arc<dyn AgyUsage>) -> Result<QuotaKeeper, Failure> {
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
            usage,
        })
    }

    /// agy's quota groups and their windows as of `AGY_DISPLAY_FRESH_MS`: the reading while it is
    /// fresh, else a new usage query, which is never run twice at once. A caller waits for it
    /// at most `patience`; past that it gets the last reading (none, before the first), and
    /// the query goes on and lands for the next caller. An answer that can't be read is the
    /// reading too, with its reason: it is shown, and asked again after the same time.
    pub async fn agy(self: &Arc<Self>, patience: Duration) -> Agy {
        let mut asked = std::pin::pin!(self.agy_asked.notified());
        {
            let mut state = lock(&self.agy);
            // Looked at under the lock: a query that lands first stamps a time the look after
            // it must not find in its future.
            let now = clock::lab_now_ms();
            if let Some(reading) = state
                .reading
                .as_ref()
                .filter(|r| r.fresh(now, AGY_DISPLAY_FRESH_MS))
            {
                return reading.clone();
            }
            // Listening before the query can end, so its end is never missed.
            asked.as_mut().enable();
            if !state.asking {
                state.asking = true;
                let keeper = Arc::clone(self);
                tokio::spawn(async move { keeper.ask_agy_now().await });
            }
        }
        let _ = tokio::time::timeout(patience, asked).await;
        lock(&self.agy).reading.clone().unwrap_or_default()
    }

    /// Asks agy's usage and keeps the answer, stamped with when it landed, not when it was asked.
    async fn ask_agy_now(&self) {
        let usage = Arc::clone(&self.usage);
        let answer = tokio::task::spawn_blocking(move || usage.query())
            .await
            .unwrap_or_else(|e| Err(format!("failed: {e}")));
        let reading = Agy::read(answer, clock::lab_now_ms());
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

fn millis_until(until: Timestamp, now: Timestamp) -> Duration {
    let millis = until.unix_millis().saturating_sub(now.unix_millis());
    Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
