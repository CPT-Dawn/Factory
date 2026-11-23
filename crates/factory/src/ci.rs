//! `factory wait ci`: blocks, silent, until every check on a pull request's head has
//! finished, on the daemon, which shares one poller per pull request between its waits.

use std::env;
use std::time::Duration;

use factory_core::exit::Exit;
use serde_json::Value;

use crate::client;
use crate::config;
use crate::notify;
use crate::output::{Done, Failure, LINE_LIMIT};
use crate::paths::Paths;
use crate::protocol::{NotifyOn, WAIT_CI, WaitCiParams, WaitCiResult};

pub fn wait(pr: &str, timeout: Duration) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let poll = config::ci_poll(&paths)?;
    let params = WaitCiParams {
        pr: pr.to_owned(),
        cwd: cwd()?,
        waiter: env::var("HERDR_PANE_ID")
            .ok()
            .filter(|pane| !pane.is_empty()),
        timeout_secs: timeout.as_secs(),
        poll_secs: poll.as_secs(),
    };
    let answer: WaitCiResult = client::wait(&paths, WAIT_CI, &params, timeout)?;
    let mut done = Done::new(
        line(&answer),
        serde_json::to_value(&answer).unwrap_or(Value::Null),
    );
    done.exit = Exit::from_code(answer.exit).unwrap_or(Exit::NeedsAttention);
    Ok(done)
}

/// `factory wait ci --notify`: registers the wait, and the daemon posts its line when it ends.
pub fn notify(pr: &str, timeout: Duration) -> Result<Done, Failure> {
    let poll = config::ci_poll(&Paths::from_env()?)?;
    let on = NotifyOn::Ci {
        pr: pr.to_owned(),
        cwd: cwd()?,
        poll_secs: poll.as_secs(),
    };
    notify::register(on, timeout)
}

/// The working directory, where `gh` finds the repo a pull request's number belongs to.
fn cwd() -> Result<String, Failure> {
    let cwd = env::current_dir().map_err(|e| {
        Failure::precondition(format!(
            "cannot read the working directory: {e}; run it from the repo's checkout"
        ))
    })?;
    Ok(cwd.to_string_lossy().into_owned())
}

/// `<pr>: <n> passed, <n> failed[, <n> skipped][; logs in <file>; failed: <names>]`, with as
/// many names as fit the line and a count of the rest, which the file and `--json` list. Only a
/// state dir so deep that its path alone fills the line gets cut.
pub fn line(answer: &WaitCiResult) -> String {
    let mut line = format!(
        "{}: {} passed, {} failed",
        answer.pr,
        answer.passed,
        answer.failed.len()
    );
    if answer.skipped > 0 {
        line.push_str(&format!(", {} skipped", answer.skipped));
    }
    if let Some(file) = &answer.file {
        line.push_str(&format!("; logs in {file}"));
    }
    let names = &answer.failed;
    for shown in (1..=names.len()).rev() {
        let rest = names.len() - shown;
        let more = if rest > 0 {
            format!(" and {rest} more")
        } else {
            String::new()
        };
        let candidate = format!("{line}; failed: {}{more}", names[..shown].join(", "));
        if candidate.chars().count() <= LINE_LIMIT {
            return candidate;
        }
    }
    // Not one name fits beside the file, which the line keeps.
    line
}
