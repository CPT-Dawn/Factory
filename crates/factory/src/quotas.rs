//! The quota keeper: the daemon's one owner of every harness's latest usage readings. Claude's
//! and Codex's are kept in its database so a restart forgets no hold; agy's are held in memory
//! and asked again after a restart. Status, the bar and a dispatch all ask it; nothing else
//! reads or records a quota, but agy's routing, which keeps its own 120 s look. Claude's two
//! windows come in through `factory hook statusline`, and its limit through the `StopFailure`
//! hook. Codex's are asked of its `app-server`, at most once in 5 minutes, by whichever look
//! finds the reading stale: one ask at a time, on a thread of its own, so a hung server holds up
//! no look beyond its patience. agy's come from its usage query, which the keeper asks for
//! itself, at most once in [`AGY_DISPLAY_FRESH_MS`]: what status, the bar and `factory quota
//! agy` show is one reading, however often they look.

use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_adapters::codex::CodexUsage;
use factory_adapters::reviewer::AgyUsage;
use factory_core::claude::{ClaudeQuota, Verdict};
use factory_core::codex::{self, CodexQuota};
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
    ClaudeLimitParams, ClaudeVerdict, ClaudeWindowParams, CodexParams, CodexVerdict, HistoryParams,
    HistoryRows, WaitClaudeParams,
};
use crate::store::Store;

/// How long a look waits for Codex's answer before it goes on with what it has. Under the
/// client's own 10 seconds, so a slow Codex never reads as a daemon that is down.
const PATIENCE: Duration = Duration::from_secs(8);

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

/// A Codex reading that could not be written: its `unreadable`, to put back once a look has
/// written it, and the windows it read then, which join the history with it. A failed ask
/// read none: the windows it keeps were read before, and are in the history already, or
/// pruned from it.
struct Unsaved {
    unreadable: Option<String>,
    read: Vec<UsageWindow>,
}

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
    codex: Mutex<CodexQuota>,
    codex_usage: Arc<dyn CodexUsage>,
    /// Whether an ask of Codex is out, so a second look waits for it instead of making its own.
    codex_asking: Mutex<bool>,
    /// Woken when an ask of Codex ends.
    codex_asked: Notify,
    /// Set while the last reading could not be written: what it said, to write again.
    codex_unsaved: Mutex<Option<Unsaved>>,
    agy: Mutex<AgyState>,
    /// Woken when agy's usage query ends.
    agy_asked: Notify,
    usage: Arc<dyn AgyUsage>,
}

impl QuotaKeeper {
    /// The keeper over `store`, asking Codex's usage through `codex_usage` and agy's through
    /// `usage`.
    pub fn open(
        store: Store,
        codex_usage: Arc<dyn CodexUsage>,
        usage: Arc<dyn AgyUsage>,
    ) -> Result<QuotaKeeper, Failure> {
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
        let keeper = QuotaKeeper {
            store: Mutex::new(store),
            claude: Mutex::new(claude),
            pruned: Mutex::new(None),
            changed: Notify::new(),
            codex: Mutex::new(codex),
            codex_usage,
            codex_asking: Mutex::new(false),
            codex_asked: Notify::new(),
            codex_unsaved: Mutex::new(None),
            agy: Mutex::new(AgyState::default()),
            agy_asked: Notify::new(),
            usage,
        };
        keeper.prune_if_due(clock::now())?;
        Ok(keeper)
    }

    /// Codex's quota as `factory status`, the bar and `factory quota codex` record it. A reading
    /// older than 5 minutes is asked of Codex first, once however many look at the same time; a
    /// failed ask counts as an ask, so a Codex that is down is not pressed again within 5 minutes.
    pub async fn codex(self: &Arc<Self>, params: &CodexParams) -> factory_core::status::Codex {
        self.keep_codex();
        if self.start_codex_ask(params) {
            self.await_codex_ask(Some(Instant::now() + PATIENCE)).await;
        }
        let quota = lock(&self.codex).clone();
        let mut record = quota.record(clock::now(), params.hold_at);
        if *lock(&self.codex_asking) && quota.asked.is_none() && record.windows.is_empty() {
            record.unreadable = Some("it is still being asked; look again shortly".to_owned());
        }
        record
    }

    /// Codex's quota as it stands, asking nothing: what a dispatch lays over each pick.
    pub fn codex_standing(&self, params: &CodexParams) -> factory_core::status::Codex {
        self.keep_codex();
        lock(&self.codex)
            .clone()
            .record(clock::now(), params.hold_at)
    }

    /// For a dispatch whose pick has just fallen to Codex, under the lock it picked with: starts
    /// an ask when the reading is stale and none is out. Whether an ask is out, for
    /// [`QuotaKeeper::codex_asked`] to wait on.
    pub fn ask_codex_for_dispatch(self: &Arc<Self>, params: &CodexParams) -> bool {
        self.start_codex_ask(params)
    }

    /// Waits for the ask that is out to end, however long that takes: an ask ends by itself,
    /// killed at its own timeout, whoever started it. A dispatch waits for this where a look does
    /// not, so a review never starts on a plan that a reading on its way would have held.
    pub async fn codex_asked(&self) {
        self.await_codex_ask(None).await;
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

    /// Waits for the ask that is out: until `patience`, or without end (it ends itself) for none.
    async fn await_codex_ask(&self, patience: Option<Instant>) {
        loop {
            let mut ended = pin!(self.codex_asked.notified());
            ended.as_mut().enable();
            if !*lock(&self.codex_asking) {
                return;
            }
            match patience {
                Some(patience) => {
                    if tokio::time::timeout_at(patience, ended).await.is_err() {
                        return;
                    }
                }
                None => ended.await,
            }
        }
    }

    /// Tries again to write a reading that could not be written; the reading it says so beside
    /// stands as it was once it has been.
    fn keep_codex(&self) {
        let mut unsaved = lock(&self.codex_unsaved);
        let Some(reading) = unsaved.take() else {
            return;
        };
        let mut quota = lock(&self.codex);
        if self.store_codex(&quota, &reading.read).is_ok() {
            quota.unreadable = reading.unreadable;
        } else {
            *unsaved = Some(reading);
        }
    }

    /// One ask of Codex, on a thread of its own: its answer, or why there was none, is the new
    /// reading.
    async fn ask_codex(self: Arc<Self>, timeout: Duration) {
        let usage = Arc::clone(&self.codex_usage);
        let answer = tokio::task::spawn_blocking(move || usage.rate_limits(timeout)).await;
        let now = clock::now();
        {
            // The marker, the reading and its write are taken in the order `keep_codex` takes
            // them, and held until the new reading is out: no look can write an older reading
            // over it, or take the marker that says it is still to be written.
            let mut unsaved = lock(&self.codex_unsaved);
            let mut quota = lock(&self.codex);
            let mut next = quota.clone();
            match answer {
                Ok(Ok(reply)) => match codex::read_usage(&reply, now) {
                    Ok(read) => next.see(read, now),
                    Err(why) => next.fail(why, now),
                },
                Ok(Err(why)) => next.fail(why.to_string(), now),
                Err(why) => next.fail(format!("its reader stopped: {why}"), now),
            }
            // Only an answer read now joins the history: a failed ask keeps the windows it had,
            // which are no new reading.
            let read = if next.unreadable.is_none() {
                next.windows()
            } else {
                Vec::new()
            };
            // A reading that could not be kept still holds this run, and says so beside it: the
            // next look writes it again.
            *unsaved = None;
            if let Err(e) = self.store_codex(&next, &read) {
                *unsaved = Some(Unsaved {
                    unreadable: next.unreadable.take(),
                    read,
                });
                next.unreadable = Some(format!(
                    "its reading could not be kept: {e}; check the disk and FACTORY_STATE"
                ));
            }
            *quota = next;
        }
        *lock(&self.codex_asking) = false;
        self.codex_asked.notify_waiters();
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

    /// Stores Codex's reading by the shared path, with `read`, the windows its ask read, joining
    /// the history. The caller holds the Codex lock.
    fn store_codex(&self, quota: &CodexQuota, read: &[UsageWindow]) -> rusqlite::Result<()> {
        self.store_readings("codex", None, read, |store| store.set_codex_quota(quota))
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
