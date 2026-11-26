//! The quota keeper: the daemon's one owner of every harness's latest usage readings, kept in
//! its database so a restart forgets no hold. Status, the bar and a dispatch all ask it; nothing
//! else reads or records a quota. Claude's two windows come in through `factory hook
//! statusline`, and its limit through the `StopFailure` hook.

use std::sync::Mutex;
use std::time::Duration;

use factory_core::claude::{ClaudeQuota, Verdict};
use factory_core::exit::Exit;
use factory_core::timestamp::Timestamp;
use factory_core::usage::{UsageWindow, WindowKind};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::clock;
use crate::output::Failure;
use crate::protocol::{ClaudeLimitParams, ClaudeVerdict, ClaudeWindowParams, WaitClaudeParams};
use crate::store::Store;

pub struct QuotaKeeper {
    store: Mutex<Store>,
    claude: Mutex<ClaudeQuota>,
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
        Ok(QuotaKeeper {
            store: Mutex::new(store),
            claude: Mutex::new(claude),
            changed: Notify::new(),
        })
    }

    /// A status line saw Claude's windows.
    pub fn see_claude(&self, params: &ClaudeWindowParams) -> Result<(), Failure> {
        self.change(|quota, now| {
            let windows: Vec<UsageWindow> = [
                (WindowKind::FiveHour, &params.five_hour),
                (WindowKind::Weekly, &params.seven_day),
            ]
            .into_iter()
            .filter_map(|(kind, reading)| {
                reading.as_ref().map(|reading| UsageWindow {
                    kind,
                    used_percent: reading.used_percent,
                    resets: Timestamp::from_unix_millis(reading.resets),
                    seen: now,
                })
            })
            .collect();
            quota.see(&windows, now);
        })
    }

    /// A Claude turn ended on the plan's limit.
    pub fn claude_limit(&self, params: &ClaudeLimitParams) -> Result<(), Failure> {
        let reset = params.reset.map(Timestamp::from_unix_millis);
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
            let verdict = lock(&self.claude).verdict(now, params.hold_at);
            let Verdict::Hold { until, by } = verdict else {
                return Ok(self.claude_verdict(params.hold_at));
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
