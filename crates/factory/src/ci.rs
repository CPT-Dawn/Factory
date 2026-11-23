//! `factory wait ci`: blocks, silent, until every check on a pull request's head has
//! finished, on the daemon, which shares one poller per pull request between its waits.

use std::env;
use std::time::Duration;

use factory_core::exit::Exit;
use serde_json::Value;

use crate::client;
use crate::config;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{WAIT_CI, WaitCiParams, WaitCiResult};

pub fn wait(pr: &str, timeout: Duration) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let poll = config::ci_poll(&paths)?;
    let cwd = env::current_dir().map_err(|e| {
        Failure::precondition(format!(
            "cannot read the working directory: {e}; run it from the repo's checkout"
        ))
    })?;
    let params = WaitCiParams {
        pr: pr.to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
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

/// `<pr>: <n> passed, <n> failed[, <n> skipped][; logs in <file>; failed: <names>]`, the names
/// last, so a line cut short keeps the file.
fn line(answer: &WaitCiResult) -> String {
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
    if !answer.failed.is_empty() {
        line.push_str(&format!("; failed: {}", answer.failed.join(", ")));
    }
    line
}
