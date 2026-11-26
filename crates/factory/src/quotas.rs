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
use factory_core::history::{self, HistoryRow};
use factory_core::status::Agy;
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

/// How often the keeper looks whether the history is due a prune.
const PRUNE_LOOK: Duration = Duration::from_secs(5);

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
    /// When the usage history was last pruned, by the wall clock and by one that never steps
    /// back; none until this daemon has.
    pruned: Mutex<Option<(Timestamp, Instant)>>,
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
        let keeper = QuotaKeeper {
            store: Mutex::new(store),
            claude: Mutex::new(claude),
            pruned: Mutex::new(None),
            changed: Notify::new(),
            agy: Mutex::new(AgyState::default()),
            agy_asked: Notify::new(),
            usage,
        };
        keeper.prune_if_due(clock::now())?;
        Ok(keeper)
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
        self.change(&windows, |quota| quota.see(&windows, now))
    }

    /// A Claude turn ended on the plan's limit. A reset out of range counts as none named.
    pub fn claude_limit(&self, params: &ClaudeLimitParams) -> Result<(), Failure> {
        let reset = params
            .reset
            .filter(|reset| usage::representable(*reset))
            .map(Timestamp::from_unix_millis);
        let now = clock::now();
        self.change(&[], |quota| quota.hit_limit(reset, now))
    }

    /// Applies a change to Claude's quota and stores it, with the `windows` it read, in the
    /// shared path ([`QuotaKeeper::store_readings`]).
    fn change(
        &self,
        windows: &[UsageWindow],
        apply: impl FnOnce(&mut ClaudeQuota),
    ) -> Result<(), Failure> {
        let mut quota = lock(&self.claude);
        let mut next = *quota;
        apply(&mut next);
        self.store_readings("claude", None, windows, |store| {
            store.set_claude_quota(&next)
        })
        .map_err(|e| {
            Failure::precondition(format!(
                "cannot record the Claude quota: {e}; check the disk and FACTORY_STATE"
            ))
        })?;
        *quota = next;
        drop(quota);
        self.changed.notify_waiters();
        Ok(())
    }

    /// The one path every harness's readings are stored by: `write` keeps the harness's own
    /// state, and each of `windows` joins the usage history unless it is the reading its
    /// window last had, all in one transaction. A harness that stores its readings here is
    /// kept in history with no code of its own, and a stored reading is never missing from it.
    /// `group` names the quota inside a harness that meters several. The caller holds its
    /// harness's lock, so readings reach the history in the order they were stored.
    fn store_readings(
        &self,
        harness: &str,
        group: Option<&str>,
        windows: &[UsageWindow],
        write: impl FnOnce(&Store) -> rusqlite::Result<()>,
    ) -> rusqlite::Result<()> {
        lock(&self.store).atomically(|store| {
            write(store)?;
            for window in windows {
                store.append_history(&HistoryRow::of(harness, group, window))?;
            }
            Ok(())
        })
    }

    /// The usage history from `params.since` on, oldest first.
    pub fn history(&self, params: &HistoryParams) -> Result<HistoryRows, Failure> {
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
    fn prune_if_due(&self, now: Timestamp) -> Result<(), Failure> {
        let mut pruned = lock(&self.pruned);
        let due = pruned.is_none_or(|(at, since)| {
            history::prune_due(Some(at), now) || history::overdue(since.elapsed())
        });
        if !due {
            return Ok(());
        }
        lock(&self.store)
            .prune_history(history::cutoff(now).unix_millis())
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot prune the usage history: {e}; check the disk and FACTORY_STATE"
                ))
            })?;
        *pruned = Some((now, Instant::now()));
        Ok(())
    }

    /// Keeps the history to its 90 days for as long as the daemon runs, idle or not: it looks
    /// every few seconds whether a day has passed since the last prune (a start prunes too).
    pub async fn keep(self: Arc<Self>) {
        let mut failing = false;
        loop {
            tokio::time::sleep(PRUNE_LOOK).await;
            // The daemon's log hears of a failure once, not at every look until it is mended.
            let pruned = self.prune_if_due(clock::now());
            if let (Err(failure), false) = (&pruned, failing) {
                eprintln!("factory: {}", failure.message);
            }
            failing = pruned.is_err();
        }
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
