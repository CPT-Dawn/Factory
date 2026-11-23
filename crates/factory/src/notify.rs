//! `factory wait ci|lead --notify`, the CLI's half: it registers the wait with the daemon and
//! returns, and the daemon ([`crate::notifier`]) posts one line into this Claude Code session
//! when the wait ends.
//!
//! The session is the one whose shell runs the command: Claude Code exports its inbox socket and
//! token to its shells (`CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`), and its
//! own pid (`CLAUDE_PID`), which the daemon watches so a session that exits is told nothing.

use std::env;
use std::time::Duration;

use serde_json::Value;

use crate::client;
use crate::config;
use crate::lab::host;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{NotifyOn, NotifyParams, NotifyResult, SessionParam, WAIT_NOTIFY};

/// How long a `--notify` wait lasts without `--timeout`: no CLI sits on it, so it may outlast
/// a blocking wait's 9m by far.
pub const TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
/// How long registering may take, finding the pull request with `gh` included.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// Registers a wait on `on` that the daemon reports into this session, at most `limit` from now.
pub fn register(on: NotifyOn, limit: Duration) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let (enabled, config) = config::notify_enabled(&paths)?;
    if !enabled {
        return Err(config::notify_off(&config));
    }
    let params = NotifyParams {
        on,
        waiter: client::waiter(),
        timeout_secs: limit.as_secs(),
        session: session()?,
    };
    let answer: NotifyResult = client::wait(&paths, WAIT_NOTIFY, &params, REGISTER_TIMEOUT)?;
    Ok(Done::new(
        format!(
            "wait #{} on {} registered; its line comes to this session when it ends",
            answer.id, answer.on
        ),
        serde_json::to_value(&answer).unwrap_or(Value::Null),
    ))
}

/// The Claude Code session this command runs in, from the variables it exports to its shells.
fn session() -> Result<SessionParam, Failure> {
    let var = |name: &str| env::var(name).ok().filter(|value| !value.is_empty());
    let (Some(socket), Some(token)) = (
        var("CLAUDE_CODE_MESSAGING_SOCKET"),
        var("CLAUDE_CODE_MESSAGING_TOKEN"),
    ) else {
        return Err(Failure::precondition(
            "--notify posts into the Claude Code session it runs in, and this shell has no \
             CLAUDE_CODE_MESSAGING_SOCKET and TOKEN; run it from the session's own shell, or \
             wait without --notify",
        ));
    };
    let process = var("CLAUDE_PID")
        .and_then(|pid| pid.parse().ok())
        .and_then(host::proc);
    Ok(SessionParam {
        socket,
        token,
        pid: process.as_ref().map(|p| p.pid),
        start: process.map(|p| p.start),
    })
}
