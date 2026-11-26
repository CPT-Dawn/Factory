//! The quota keeper: the daemon's one owner of every harness's latest usage readings, kept in
//! its database so a restart forgets no hold. Status, the bar and a dispatch all ask it; nothing
//! else reads or records a quota. Claude's two windows come in through `factory hook
//! statusline`, and its limit through the `StopFailure` hook.

use std::sync::Mutex;
use std::time::Duration;

use factory_core::claude::{ClaudeQuota, Verdict};
use factory_core::exit::Exit;
use factory_core::history::{self, HistoryRow};
use factory_core::timestamp::Timestamp;
use factory_core::usage::{self, UsageWindow, WindowKind};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::clock;
use crate::output::Failure;
use crate::protocol::{
    ClaudeLimitParams, ClaudeVerdict, ClaudeWindowParams, HistoryParams, HistoryRows,
    WaitClaudeParams,
};
use crate::store::Store;

pub struct QuotaKeeper {
    store: Mutex<Store>,
    claude: Mutex<ClaudeQuota>,
    /// When the usage history was last pruned; none until this daemon has.
    pruned: Mutex<Option<Timestamp>>,
    /// Woken when Claude's quota changes, so a waiting lead looks again.
    changed: Notify,
}

impl QuotaKeeper {
    pub fn open(store: Store) -> Result<QuotaKeeper, Failure> {
        let claude = store.claude_quota().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the Claude quota: {e}; move the database aside and restart"
            ))
        })?;
        let keeper = QuotaKeeper {
            store: Mutex::new(store),
            claude: Mutex::new(claude),
            pruned: Mutex::new(None),
            changed: Notify::new(),
        };
        keeper.prune_if_due(clock::now())?;
        Ok(keeper)
    }

    /// A status line saw Claude's windows. A window whose share or reset is out of range keeps
    /// its last reading.
    pub fn see_claude(&self, params: &ClaudeWindowParams) -> Result<(), Failure> {
        let now = clock::now();
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
        self.change(|quota| quota.see(&windows, now))?;
        self.remember("claude", None, &windows, now)
    }

    /// Adds readings of a harness's windows to the usage history, each unless it is the reading
    /// its window last had. Every harness's readings come through here as they are stored, so
    /// each one is kept with no code of its own; `group` names the quota inside a harness that
    /// meters several.
    pub fn remember(
        &self,
        harness: &str,
        group: Option<&str>,
        windows: &[UsageWindow],
        now: Timestamp,
    ) -> Result<(), Failure> {
        self.prune_if_due(now)?;
        let store = lock(&self.store);
        for window in windows {
            store
                .append_history(&HistoryRow::of(harness, group, window))
                .map_err(|e| {
                    Failure::precondition(format!(
                        "cannot record the {harness} usage history: {e}; check the disk and FACTORY_STATE"
                    ))
                })?;
        }
        Ok(())
    }

    /// The usage history from `params.since` on, oldest first.
    pub fn history(&self, params: &HistoryParams) -> Result<HistoryRows, Failure> {
        self.prune_if_due(clock::now())?;
        lock(&self.store)
            .usage_history(params.since)
            .map(|rows| HistoryRows { rows })
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot read the usage history: {e}; check the disk and FACTORY_STATE"
                ))
            })
    }

    /// Drops the history past its 90 days, when a day or more has passed since it last did.
    /// Every reading, every read of the history and every start comes by here, so a daemon in
    /// use prunes at least daily.
    fn prune_if_due(&self, now: Timestamp) -> Result<(), Failure> {
        let mut pruned = lock(&self.pruned);
        if !history::prune_due(*pruned, now) {
            return Ok(());
        }
        lock(&self.store)
            .prune_history(history::cutoff(now).unix_millis())
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot prune the usage history: {e}; check the disk and FACTORY_STATE"
                ))
            })?;
        *pruned = Some(now);
        Ok(())
    }

    /// A Claude turn ended on the plan's limit. A reset out of range counts as none named.
    pub fn claude_limit(&self, params: &ClaudeLimitParams) -> Result<(), Failure> {
        let reset = params
            .reset
            .filter(|reset| usage::representable(*reset))
            .map(Timestamp::from_unix_millis);
        let now = clock::now();
        self.change(|quota| quota.hit_limit(reset, now))
    }

    fn change(&self, apply: impl FnOnce(&mut ClaudeQuota)) -> Result<(), Failure> {
        let mut quota = lock(&self.claude);
        let mut next = *quota;
        apply(&mut next);
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
