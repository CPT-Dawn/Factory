//! The Claude plan's quota (spec #1's quota state machine, for the plan every lead and subagent
//! draws on), the CLI's side: `factory hook statusline` and the `StopFailure` hook tell the
//! quota keeper what they saw, and `factory quota claude` and `factory wait claude` tell a lead
//! whether to dispatch.

use std::io::{self, Read};
use std::process::ExitCode;
use std::time::Duration;

use factory_core::claude;
use factory_core::exit::Exit;
use factory_core::status::ClaudeState;
use factory_core::timestamp::Timestamp;
use factory_core::usage::{UsageWindow, WindowKind};
use serde_json::Value;

use crate::client;
use crate::clock;
use crate::config;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{
    CLAUDE_LIMIT, CLAUDE_QUOTA, CLAUDE_WINDOW, ClaudeLimitParams, ClaudeQuotaParams, ClaudeVerdict,
    ClaudeWindowParams, WAIT_CLAUDE, WaitClaudeParams, WindowReading,
};

/// How long the status-line hook waits for the daemon to take its reading. The status line
/// runs on every refresh: a daemon that is slow costs it this much at most.
const HOOK_PATIENCE: Duration = Duration::from_secs(1);

/// The most of stdin the status-line hook reads: its JSON is a few kilobytes.
const HOOK_INPUT_LIMIT: u64 = 1 << 20;

/// `factory quota claude`: exit 0 to dispatch, 75 to hold, naming until when.
pub fn quota() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let hold_at = config::claude_hold_at(&paths)?;
    let verdict: ClaudeVerdict =
        client::call(&paths, CLAUDE_QUOTA, &ClaudeQuotaParams { hold_at })?;
    judged(&verdict)
}

/// `factory wait claude`: blocks, silent, until a lead may dispatch.
pub fn wait(timeout: Duration) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let (hold_at, waiter) = (config::claude_hold_at(&paths)?, client::waiter());
    let params = |resume, timeout_secs| WaitClaudeParams {
        hold_at,
        waiter: waiter.clone(),
        timeout_secs,
        resume,
    };
    let verdict: ClaudeVerdict = client::hold(&paths, WAIT_CLAUDE, params, timeout)?;
    let json = serde_json::to_value(&verdict).unwrap_or(Value::Null);
    Ok(Done::new("claude: the windows have room; dispatch", json))
}

fn judged(verdict: &ClaudeVerdict) -> Result<Done, Failure> {
    let claude = &verdict.claude;
    let now = clock::now().unix_millis();
    if let (ClaudeState::Hold, Some(until)) = (claude.verdict, claude.until) {
        let until = Timestamp::from_unix_millis(until);
        let what = match claude.hold.as_deref() {
            Some("limit hit") | None => "hit its limit".to_owned(),
            Some(hold) => format!("hold: {hold}"),
        };
        return Err(Failure::new(
            Exit::Busy,
            format!(
                "claude: {what}; hold new dispatches until {until}; factory wait claude blocks until then"
            ),
        ));
    }
    let line = if claude.windows.is_empty() {
        "claude: no plan window seen yet; dispatch".to_owned()
    } else {
        let windows: Vec<String> = claude
            .windows
            .iter()
            .map(|reading| {
                if reading.current {
                    format!(
                        "{} resets {}",
                        reading.describe(now),
                        Timestamp::from_unix_millis(reading.resets)
                    )
                } else {
                    reading.describe(now)
                }
            })
            .collect();
        format!("claude: {}; dispatch", windows.join(", "))
    };
    let json = serde_json::to_value(verdict).unwrap_or(Value::Null);
    Ok(Done::new(line, json))
}

/// `factory hook statusline`: Claude Code's status-line JSON on stdin, handed on by the status
/// line command. Tells a running daemon both usage windows and prints nothing. It exits 0
/// whatever it read and whether or not the daemon answered, and never starts a daemon: a
/// status line must not fail or wait.
pub fn statusline_hook() -> ExitCode {
    let mut input = Vec::new();
    let _ = io::stdin().take(HOOK_INPUT_LIMIT).read_to_end(&mut input);
    let input: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let windows = claude::statusline_windows(&input, clock::now());
    if windows.is_empty() {
        return ExitCode::SUCCESS;
    }
    let reading = |kind| {
        windows
            .iter()
            .find(|window: &&UsageWindow| window.kind == kind)
            .map(|window| WindowReading {
                used_percent: window.used_percent,
                resets: window.resets.unix_millis(),
            })
    };
    let params = ClaudeWindowParams {
        five_hour: reading(WindowKind::FiveHour),
        seven_day: reading(WindowKind::Weekly),
    };
    if let Ok(paths) = Paths::from_env() {
        let _: Result<Value, Failure> =
            client::call_running(&paths, CLAUDE_WINDOW, &params, HOOK_PATIENCE);
    }
    ExitCode::SUCCESS
}

/// The `StopFailure` hook: a turn that ended on a rate limit, or whose message names a plan
/// limit, tells the daemon, with the reset its message names. Another error changes nothing.
pub fn stop_failure(input: &Value) -> Result<(), Failure> {
    let now = clock::now();
    let Some(hit) = claude::stop_failure(input, now, clock::utc_offset(now)) else {
        return Ok(());
    };
    let params = ClaudeLimitParams {
        reset: hit.reset().map(Timestamp::unix_millis),
    };
    let _: Value = client::call(&Paths::from_env()?, CLAUDE_LIMIT, &params)?;
    Ok(())
}
