//! The quota keeper: the daemon's one owner of every harness's latest usage readings, kept in
//! its database so a restart forgets no hold. Status, the bar and a dispatch all ask it; nothing
//! else reads or records a quota. Claude's two windows come in through `factory hook
//! statusline`, and its limit through the `StopFailure` hook. Codex's are asked of its
//! `app-server`, at most once in 5 minutes, by whichever look finds the reading stale: one ask
//! at a time, on a thread of its own, so a hung server holds up no look beyond its patience.

use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_adapters::codex::CodexUsage;
use factory_core::claude::{ClaudeQuota, Verdict};
use factory_core::codex::{self, CodexQuota};
use factory_core::exit::Exit;
use factory_core::timestamp::Timestamp;
use factory_core::usage::{self, UsageWindow, WindowKind};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::clock;
use crate::output::Failure;
use crate::protocol::{
    ClaudeLimitParams, ClaudeVerdict, ClaudeWindowParams, CodexParams, CodexVerdict,
    WaitClaudeParams,
};
use crate::store::Store;

/// How long past its own timeout a dispatch waits for an ask of Codex to end.
const SETTLE_MARGIN: Duration = Duration::from_secs(10);

/// How long a look waits for Codex's answer before it goes on with what it has. Under the
/// client's own 10 seconds, so a slow Codex never reads as a daemon that is down.
const PATIENCE: Duration = Duration::from_secs(8);

pub struct QuotaKeeper {
    store: Mutex<Store>,
    claude: Mutex<ClaudeQuota>,
    /// Woken when Claude's quota changes, so a waiting lead looks again.
    changed: Notify,
    codex: Mutex<CodexQuota>,
    codex_usage: Arc<dyn CodexUsage>,
    /// Whether an ask of Codex is out, so a second look waits for it instead of making its own.
    codex_asking: Mutex<bool>,
    /// Woken when an ask of Codex ends.
    codex_asked: Notify,
}

impl QuotaKeeper {
    pub fn open(store: Store, codex_usage: Arc<dyn CodexUsage>) -> Result<QuotaKeeper, Failure> {
        let claude = store.claude_quota().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the Claude quota: {e}; move the database aside and restart"
            ))
        })?;
        let codex = store.codex_quota().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the Codex quota: {e}; move the database aside and restart"
            ))
        })?;
        Ok(QuotaKeeper {
            store: Mutex::new(store),
            claude: Mutex::new(claude),
            changed: Notify::new(),
            codex: Mutex::new(codex),
            codex_usage,
            codex_asking: Mutex::new(false),
            codex_asked: Notify::new(),
        })
    }

    /// Codex's quota as `factory status`, the bar and `factory quota codex` record it. A reading
    /// older than 5 minutes is asked of Codex first, once however many look at the same time; a
    /// failed ask counts as an ask, so a Codex that is down is not pressed again within 5 minutes.
    pub async fn codex(self: &Arc<Self>, params: &CodexParams) -> factory_core::status::Codex {
        if self.start_codex_ask(params) {
            self.await_codex_ask(Instant::now() + PATIENCE).await;
        }
        let quota = lock(&self.codex).clone();
        let mut record = quota.record(clock::now(), params.hold_at);
        if *lock(&self.codex_asking) && quota.asked.is_none() && record.windows.is_empty() {
            record.unreadable = Some("it is still being asked; look again shortly".to_owned());
        }
        record
    }

    /// Codex's quota once its ask has ended, however long that takes: an ask ends by itself,
    /// killed at its timeout. A dispatch waits for this where a look does not, so a review never
    /// starts on a plan that a reading on its way would have held.
    pub async fn codex_settled(
        self: &Arc<Self>,
        params: &CodexParams,
    ) -> factory_core::status::Codex {
        if self.start_codex_ask(params) {
            // The ask's own time, with a margin for it to be killed and its answer taken in.
            let wait = Duration::from_secs(params.timeout_secs).saturating_add(SETTLE_MARGIN);
            self.await_codex_ask(Instant::now() + wait).await;
        }
        lock(&self.codex)
            .clone()
            .record(clock::now(), params.hold_at)
    }

    /// Whether a review may dispatch on Codex now: `codex` with the mark it held to.
    pub async fn codex_verdict(self: &Arc<Self>, params: &CodexParams) -> CodexVerdict {
        CodexVerdict {
            codex: self.codex(params).await,
            hold_at: params.hold_at,
        }
    }

    /// Starts an ask of Codex when the reading is stale and none is out. Whether a look should
    /// wait for the ask that is out.
    fn start_codex_ask(self: &Arc<Self>, params: &CodexParams) -> bool {
        let mut asking = lock(&self.codex_asking);
        if !lock(&self.codex).stale(clock::now()) {
            return false;
        }
        if !*asking {
            *asking = true;
            let keeper = Arc::clone(self);
            let timeout = Duration::from_secs(params.timeout_secs);
            tokio::spawn(async move { keeper.ask_codex(timeout).await });
        }
        true
    }

    /// Waits for the ask that is out, for as long as a look is patient.
    async fn await_codex_ask(&self, patience: Instant) {
        loop {
            let mut ended = pin!(self.codex_asked.notified());
            ended.as_mut().enable();
            if !*lock(&self.codex_asking) {
                return;
            }
            if tokio::time::timeout_at(patience, ended).await.is_err() {
                return;
            }
        }
    }

    /// One ask of Codex, on a thread of its own: its answer, or why there was none, is the new
    /// reading.
    async fn ask_codex(self: Arc<Self>, timeout: Duration) {
        let usage = Arc::clone(&self.codex_usage);
        let answer = tokio::task::spawn_blocking(move || usage.rate_limits(timeout)).await;
        let now = clock::now();
        let mut next = lock(&self.codex).clone();
        match answer {
            Ok(Ok(reply)) => match codex::read_usage(&reply, now) {
                Ok(read) => next.see(read, now),
                Err(why) => next.fail(why, now),
            },
            Ok(Err(why)) => next.fail(why.to_string(), now),
            Err(why) => next.fail(format!("its reader stopped: {why}"), now),
        }
        // A reading that could not be kept still holds this run, and says so beside it: the
        // next ask, 5 minutes on, keeps it again.
        if let Err(e) = lock(&self.store).set_codex_quota(&next) {
            next.unreadable = Some(format!(
                "its reading could not be kept: {e}; check the disk and FACTORY_STATE"
            ));
        }
        *lock(&self.codex) = next;
        *lock(&self.codex_asking) = false;
        self.codex_asked.notify_waiters();
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
