//! The Claude plan's quota (spec #1's quota state machine, for the plan every lead and subagent
//! draws on): the statusline and the `StopFailure` hook tell the daemon what they saw, and
//! `factory quota claude` and `factory wait claude` tell a lead whether to dispatch.
//!
//! The daemon keeps the quota in its database, so a restart forgets no hold.

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use factory_core::claude::{self, ClaudeQuota, Verdict, Window};
use factory_core::exit::Exit;
use factory_core::status::ClaudeState;
use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::client;
use crate::clock;
use crate::config;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{
    CLAUDE_LIMIT, CLAUDE_QUOTA, CLAUDE_WINDOW, ClaudeLimitParams, ClaudeQuotaParams, ClaudeVerdict,
    ClaudeWindowParams, WAIT_CLAUDE, WaitClaudeParams,
};
use crate::store::Store;

/// The daemon's side: the quota, in memory and in the database.
pub struct Claude {
    store: Mutex<Store>,
    quota: Mutex<ClaudeQuota>,
    /// Woken when the quota changes, so a waiting lead looks again.
    changed: Notify,
}

impl Claude {
    pub fn open(store: Store) -> Result<Claude, Failure> {
        let quota = store.claude_quota().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the Claude quota: {e}; move the database aside and restart"
            ))
        })?;
        Ok(Claude {
            store: Mutex::new(store),
            quota: Mutex::new(quota),
            changed: Notify::new(),
        })
    }

    /// A statusline saw the window.
    pub fn see(&self, params: &ClaudeWindowParams) -> Result<(), Failure> {
        let window = Window {
            used_percent: params.used_percent,
            resets: Timestamp::from_unix_millis(params.resets),
        };
        self.change(|quota, now| quota.see_window(window, now))
    }

    /// A turn ended on the plan's limit.
    pub fn limit(&self, params: &ClaudeLimitParams) -> Result<(), Failure> {
        let reset = params.reset.map(Timestamp::from_unix_millis);
        self.change(|quota, now| quota.hit_limit(reset, now))
    }

    fn change(&self, apply: impl FnOnce(&mut ClaudeQuota, Timestamp)) -> Result<(), Failure> {
        let now = clock::now();
        let mut quota = lock(&self.quota);
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

    /// Whether a lead may dispatch now, holding at `hold_at` percent.
    pub fn verdict(&self, hold_at: f64) -> ClaudeVerdict {
        answer(lock(&self.quota).verdict(clock::now(), hold_at), hold_at)
    }

    /// The verdict as `factory status` records it.
    pub fn record(&self, hold_at: f64) -> factory_core::status::Claude {
        lock(&self.quota).verdict(clock::now(), hold_at).record()
    }

    /// Answers once a lead may dispatch, or with exit 124 once `timeout_secs` pass.
    pub async fn wait(&self, params: &WaitClaudeParams) -> Result<ClaudeVerdict, Failure> {
        let limit = Duration::from_secs(params.timeout_secs);
        let deadline = Instant::now() + limit;
        loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            let now = clock::now();
            let verdict = lock(&self.quota).verdict(now, params.hold_at);
            let Verdict::Hold { until, .. } = verdict else {
                return Ok(answer(verdict, params.hold_at));
            };
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    Exit::Timeout,
                    format!(
                        "claude: still holding dispatches until {until} after {}; wait again",
                        factory_core::duration::show(limit)
                    ),
                ));
            }
            let reset = Instant::now() + millis_until(until, now);
            let _ = tokio::time::timeout_at(reset.min(deadline), changed).await;
        }
    }
}

fn answer(verdict: Verdict, hold_at: f64) -> ClaudeVerdict {
    let (used_percent, resets, until) = match verdict {
        Verdict::Unknown => (None, None, None),
        Verdict::Go {
            used_percent,
            resets,
        } => (Some(used_percent), Some(resets.unix_millis()), None),
        Verdict::Hold {
            until,
            used_percent,
        } => (used_percent, None, Some(until.unix_millis())),
    };
    ClaudeVerdict {
        verdict: verdict.state(),
        used_percent,
        resets,
        until,
        hold_at,
    }
}

fn millis_until(until: Timestamp, now: Timestamp) -> Duration {
    let millis = until.unix_millis().saturating_sub(now.unix_millis());
    Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}

/// `factory quota claude`: exit 0 to dispatch, 75 to hold, naming until when.
pub fn quota() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let hold_at = config::claude_hold_at(&paths)?;
    let verdict: ClaudeVerdict =
        client::call(&paths, CLAUDE_QUOTA, &ClaudeQuotaParams { hold_at })?;
    judged(verdict)
}

/// `factory wait claude`: blocks, silent, until a lead may dispatch.
pub fn wait(timeout: Duration) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let params = WaitClaudeParams {
        hold_at: config::claude_hold_at(&paths)?,
        waiter: client::waiter(),
        timeout_secs: timeout.as_secs(),
    };
    let verdict: ClaudeVerdict = client::wait(&paths, WAIT_CLAUDE, &params, timeout)?;
    let json = serde_json::to_value(&verdict).unwrap_or(Value::Null);
    Ok(Done::new("claude: the window has room; dispatch", json))
}

fn judged(verdict: ClaudeVerdict) -> Result<Done, Failure> {
    let used = verdict.used_percent.map(percent);
    let line = match (verdict.verdict, verdict.until, verdict.resets, used) {
        (ClaudeState::Hold, Some(until), _, used) => {
            let until = Timestamp::from_unix_millis(until);
            let what = used.map_or_else(
                || "hit its limit".to_owned(),
                |used| format!("{used}% of the 5-hour window used"),
            );
            return Err(Failure::new(
                Exit::Busy,
                format!(
                    "claude: {what}; hold new dispatches until {until}; factory wait claude blocks until then"
                ),
            ));
        }
        (ClaudeState::Go, _, Some(resets), Some(used)) => format!(
            "claude: {used}% of the 5-hour window used, resets {}; dispatch",
            Timestamp::from_unix_millis(resets)
        ),
        _ => "claude: no plan window seen yet; dispatch".to_owned(),
    };
    let json = serde_json::to_value(&verdict).unwrap_or(Value::Null);
    Ok(Done::new(line, json))
}

#[allow(clippy::cast_possible_truncation)]
fn percent(used: f64) -> i64 {
    used.round() as i64
}

/// `factory statusline`: Claude Code's statusline JSON on stdin; tells the daemon the 5-hour
/// window and prints the model and the window in one line, or with `json` one JSON object. A
/// daemon that doesn't answer costs the line nothing.
pub fn statusline(json: bool) -> ExitCode {
    let mut text = String::new();
    let input: Value = match io::stdin().read_to_string(&mut text) {
        Ok(_) => serde_json::from_str(&text).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let model = input
        .pointer("/model/display_name")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    let mut line = model.to_owned();
    let window = claude::statusline_window(&input);
    if let Some(window) = window {
        let params = ClaudeWindowParams {
            used_percent: window.used_percent,
            resets: window.resets.unix_millis(),
        };
        if let Ok(paths) = Paths::from_env() {
            let _: Result<Value, Failure> = client::call(&paths, CLAUDE_WINDOW, &params);
        }
        line.push_str(&format!(
            " · 5h {}% · resets {}",
            percent(window.used_percent),
            local_clock(window.resets)
        ));
    }
    if json {
        let window = window.map(|window| {
            json!({"used_percent": window.used_percent, "resets": window.resets.unix_millis()})
        });
        line = json!({"line": line, "model": model, "five_hour": window}).to_string();
    }
    let _ = writeln!(io::stdout().lock(), "{line}");
    ExitCode::SUCCESS
}

/// `HH:MM` of `at` in the local zone.
fn local_clock(at: Timestamp) -> String {
    let local = at.unix_millis() + i64::from(clock::utc_offset(at)) * 1000;
    let minutes = local.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// The `StopFailure` hook: a turn that ended on a rate limit, or whose message names a plan
/// limit, tells the daemon, with the reset its message names. Another error changes nothing.
pub fn stop_failure(input: &Value) -> Result<(), Failure> {
    let now = clock::now();
    let Some(reset) = claude::stop_failure(input, now, clock::utc_offset(now)) else {
        return Ok(());
    };
    let params = ClaudeLimitParams {
        reset: reset.reset().map(Timestamp::unix_millis),
    };
    let _: Value = client::call(&Paths::from_env()?, CLAUDE_LIMIT, &params)?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
