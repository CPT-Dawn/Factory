//! GitHub issues, pull requests and their checks, through `gh`.

use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use factory_core::ci::{CheckRun, PullRequest};
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc as mpsc_async;

use crate::process;

/// How long one gh call may take.
const TIMEOUT: Duration = Duration::from_secs(60);
/// An issue as Markdown: its number and title as the heading, then its body.
const TEMPLATE: &str = r#"# #{{.number}} {{.title}}{{"\n\n"}}{{.body}}{{"\n"}}"#;

/// Why an issue could not be fetched: what gh said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhError(pub String);

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GhError {}

/// The issue tracker of the repository in the working directory.
pub trait Issues {
    /// The issue `issue` names (a number, or an issue URL) as Markdown: `# #<number> <title>`,
    /// a blank line, then its body.
    fn issue(&self, issue: &str) -> Result<String, GhError>;
}

/// The issues `gh` on PATH sees from the working directory.
pub struct Gh;

impl Issues for Gh {
    fn issue(&self, issue: &str) -> Result<String, GhError> {
        let mut cmd = Command::new("gh");
        cmd.args(["issue", "view", issue])
            .args(["--json", "number,title,body", "--template", TEMPLATE])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = process::run(cmd, TIMEOUT).map_err(|e| GhError(e.to_string()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GhError(
                stderr
                    .lines()
                    .next()
                    .unwrap_or("it failed and said nothing")
                    .to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Issues held in memory, for tests of code that fetches them.
#[derive(Clone, Debug, Default)]
pub struct FakeIssues {
    /// Each known issue, as the caller names it, and its Markdown.
    pub issues: Vec<(String, String)>,
}

impl Issues for FakeIssues {
    fn issue(&self, issue: &str) -> Result<String, GhError> {
        self.issues
            .iter()
            .find(|(known, _)| known == issue)
            .map(|(_, text)| text.clone())
            .ok_or_else(|| GhError(format!("no issue {issue}")))
    }
}

/// What a conditional GET found: the resource as it is now, or no change since the ETag sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched<T> {
    Fresh { etag: Option<String>, value: T },
    NotModified,
}

/// What `gh event listen` said about a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// The event stream attached (again): what happened before it is not replayed.
    Attached,
    /// A check suite completed or the head moved.
    Changed,
    /// The listener ended, and why.
    Ended(String),
}

/// The events `gh event listen` names, one listener per pull request.
pub const LISTEN_EVENTS: &str = "com.github.pull_request.check_suite.status.changed.v1,\
                                 com.github.pull_request.head.changed.v1";
/// How many entries one page of a list holds.
const PAGE: usize = 100;
/// How long one listener may run: an event subscription expires after 24 hours.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// A running `gh event listen`; its process group is killed when this drops.
pub struct Listener {
    heard: mpsc_async::UnboundedReceiver<Heard>,
    group: Option<i32>,
}

impl Listener {
    /// A listener fed by `heard` instead of a process, for fakes.
    pub fn from_channel(heard: mpsc_async::UnboundedReceiver<Heard>) -> Listener {
        Listener { heard, group: None }
    }

    /// What the listener says next; none once it has said [`Heard::Ended`].
    pub async fn next(&mut self) -> Option<Heard> {
        self.heard.recv().await
    }

    /// What it has said already and not been asked for, without waiting.
    pub fn ready(&mut self) -> Vec<Heard> {
        std::iter::from_fn(|| self.heard.try_recv().ok()).collect()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(group) = self.group.and_then(Pid::from_raw) {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

/// The pull requests and checks of GitHub, as a wait on CI reads them.
pub trait PullRequests: Send + Sync + 'static {
    /// The URL of the pull request `pr` names (a number, a URL or a branch), seen from `cwd`.
    fn url(&self, pr: &str, cwd: &Path) -> Result<String, GhError>;
    /// The pull request's head commit, unless it has not changed since `etag`.
    fn head(&self, pr: &PullRequest, etag: Option<&str>) -> Result<Fetched<String>, GhError>;
    /// The check runs on `head`, every page of them, unless they have not changed since `etag`.
    /// Only a single page carries an ETag: one of page 1 says nothing of page 2.
    fn check_runs(
        &self,
        pr: &PullRequest,
        head: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<CheckRun>>, GhError>;
    /// The commit statuses on `head`, the latest of each context, as runs: unless they have not
    /// changed since `etag`.
    fn statuses(
        &self,
        pr: &PullRequest,
        head: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<CheckRun>>, GhError>;
    /// The log of a GitHub Actions job.
    fn job_log(&self, pr: &PullRequest, job: u64) -> Result<String, GhError>;
    /// Starts listening for the pull request's check suites and head; call it inside the
    /// daemon's runtime.
    fn listen(&self, pr: &PullRequest) -> Result<Listener, GhError>;
}

impl Gh {
    /// `gh api -i` on `path` with `etag` as If-None-Match: the body and ETag of a 200, or
    /// [`Fetched::NotModified`] on a 304 (which gh exits 1 on).
    fn conditional(
        &self,
        pr: &PullRequest,
        path: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Value>, GhError> {
        let mut cmd = Command::new("gh");
        cmd.args(["api", "--hostname", &pr.host, "-i"]);
        if let Some(etag) = etag {
            cmd.arg("-H").arg(format!("If-None-Match: {etag}"));
        }
        cmd.arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = process::run(cmd, TIMEOUT).map_err(|e| GhError(e.to_string()))?;
        let text = String::from_utf8_lossy(&output.stdout);
        let Some((status, etag, body)) = http_response(&text) else {
            return Err(failed(&output.stderr));
        };
        match status {
            304 => Ok(Fetched::NotModified),
            200..=299 => {
                let value = serde_json::from_str(body).map_err(|e| {
                    GhError(format!("GitHub answered {path} with unreadable JSON: {e}"))
                })?;
                Ok(Fetched::Fresh { etag, value })
            }
            _ => Err(failed(&output.stderr)),
        }
    }

    /// Every entry of the list `list` in a paged response at `path` (which has a query
    /// already): page 1 sent with `etag`, then the pages after it that `total_count` says
    /// exist. A list on more than one page comes without an ETag, so the next read fetches it
    /// whole.
    fn paged(
        &self,
        pr: &PullRequest,
        path: &str,
        list: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<Value>>, GhError> {
        let (mut etag, page) = match self.conditional(pr, path, etag)? {
            Fetched::NotModified => return Ok(Fetched::NotModified),
            Fetched::Fresh { etag, value } => (etag, value),
        };
        let total = page["total_count"]
            .as_u64()
            .and_then(|total| usize::try_from(total).ok())
            .unwrap_or(0);
        let mut all = entries(&page, list);
        let mut number = 1;
        while all.len() < total {
            number += 1;
            etag = None;
            let more = match self.conditional(pr, &format!("{path}&page={number}"), None)? {
                Fetched::Fresh { value, .. } => entries(&value, list),
                Fetched::NotModified => Vec::new(),
            };
            if more.is_empty() {
                break;
            }
            all.extend(more);
        }
        Ok(Fetched::Fresh { etag, value: all })
    }
}

impl PullRequests for Gh {
    fn url(&self, pr: &str, cwd: &Path) -> Result<String, GhError> {
        let mut cmd = Command::new("gh");
        cmd.args(["pr", "view", pr, "--json", "url", "--jq", ".url"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = process::run(cmd, TIMEOUT).map_err(|e| GhError(e.to_string()))?;
        if !output.status.success() {
            return Err(failed(&output.stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn head(&self, pr: &PullRequest, etag: Option<&str>) -> Result<Fetched<String>, GhError> {
        let path = format!("repos/{}/{}/pulls/{}", pr.owner, pr.repo, pr.number);
        match self.conditional(pr, &path, etag)? {
            Fetched::NotModified => Ok(Fetched::NotModified),
            Fetched::Fresh { etag, value } => {
                let head = value["head"]["sha"]
                    .as_str()
                    .ok_or_else(|| GhError(format!("GitHub's {path} names no head commit")))?;
                Ok(Fetched::Fresh {
                    etag,
                    value: head.to_owned(),
                })
            }
        }
    }

    fn check_runs(
        &self,
        pr: &PullRequest,
        head: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<CheckRun>>, GhError> {
        let path = format!(
            "repos/{}/{}/commits/{head}/check-runs?per_page={PAGE}",
            pr.owner, pr.repo
        );
        Ok(match self.paged(pr, &path, "check_runs", etag)? {
            Fetched::NotModified => Fetched::NotModified,
            Fetched::Fresh { etag, value } => Fetched::Fresh {
                etag,
                value: value.iter().filter_map(check_run).collect(),
            },
        })
    }

    fn statuses(
        &self,
        pr: &PullRequest,
        head: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<CheckRun>>, GhError> {
        let path = format!(
            "repos/{}/{}/commits/{head}/status?per_page={PAGE}",
            pr.owner, pr.repo
        );
        Ok(match self.paged(pr, &path, "statuses", etag)? {
            Fetched::NotModified => Fetched::NotModified,
            Fetched::Fresh { etag, value } => Fetched::Fresh {
                etag,
                value: value.iter().filter_map(status).collect(),
            },
        })
    }

    fn job_log(&self, pr: &PullRequest, job: u64) -> Result<String, GhError> {
        let mut cmd = Command::new("gh");
        cmd.args(["api", "--hostname", &pr.host])
            .arg(format!(
                "repos/{}/{}/actions/jobs/{job}/logs",
                pr.owner, pr.repo
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = process::run(cmd, TIMEOUT).map_err(|e| GhError(e.to_string()))?;
        if !output.status.success() {
            return Err(failed(&output.stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn listen(&self, pr: &PullRequest) -> Result<Listener, GhError> {
        let mut cmd = tokio::process::Command::new("gh");
        cmd.args(["event", "listen", "--resource", &pr.url()])
            .args(["--event", LISTEN_EVENTS, "--follow"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| GhError(format!("cannot start gh event listen: {e}")))?;
        let group = child.id().and_then(|pid| i32::try_from(pid).ok());
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(GhError(
                "gh event listen has no stdout or stderr".to_owned(),
            ));
        };
        let (tx, heard) = mpsc_async::unbounded_channel();
        tokio::spawn(async move {
            let said = tokio::time::timeout(LISTEN_TIMEOUT, async {
                let mut stdout = BufReader::new(stdout).lines();
                let mut stderr = BufReader::new(stderr).lines();
                let (mut out_open, mut err_open) = (true, true);
                let mut last = String::new();
                while out_open || err_open {
                    tokio::select! {
                        line = stdout.next_line(), if out_open => match line {
                            Ok(Some(_)) => { let _ = tx.send(Heard::Changed); }
                            _ => out_open = false,
                        },
                        line = stderr.next_line(), if err_open => match line {
                            Ok(Some(line)) => {
                                if !line.trim().is_empty() {
                                    line.trim().clone_into(&mut last);
                                }
                                let _ = tx.send(Heard::Attached);
                            }
                            _ => err_open = false,
                        },
                    }
                }
                let status = child.wait().await;
                match status {
                    Ok(status) if last.is_empty() => format!("gh event listen ended ({status})"),
                    Ok(_) => last,
                    Err(e) => format!("lost gh event listen: {e}"),
                }
            })
            .await;
            let why = said.unwrap_or_else(|_| {
                // Killed here, not when the listener drops: its reader may be busy for a while.
                if let Some(group) = group.and_then(Pid::from_raw) {
                    let _ = kill_process_group(group, Signal::KILL);
                }
                format!(
                    "gh event listen outlived {}h and was stopped",
                    LISTEN_TIMEOUT.as_secs() / 3600
                )
            });
            let _ = tx.send(Heard::Ended(why));
        });
        Ok(Listener { heard, group })
    }
}

/// The status, ETag and body of what `gh api -i` printed: a status line, headers, a blank
/// line, then the body. None when it printed no response.
fn http_response(text: &str) -> Option<(u16, Option<String>, &str)> {
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .unwrap_or((text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()?
        .strip_prefix("HTTP/")?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let etag = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("etag")
            .then(|| value.trim().to_owned())
    });
    Some((status, etag, body))
}

/// One entry of a `check-runs` response; none when it lacks a field.
fn check_run(run: &Value) -> Option<CheckRun> {
    Some(CheckRun {
        id: run["id"].as_u64()?,
        name: run["name"].as_str()?.to_owned(),
        status: run["status"].as_str()?.to_owned(),
        conclusion: run["conclusion"].as_str().map(str::to_owned),
        url: run["html_url"]
            .as_str()
            .or_else(|| run["details_url"].as_str())
            .unwrap_or_default()
            .to_owned(),
        app: run["app"]["slug"].as_str().map(str::to_owned),
    })
}

/// One commit status as a run: `pending` is still running, `success`, `failure` and `error`
/// are its conclusion.
fn status(status: &Value) -> Option<CheckRun> {
    let state = status["state"].as_str()?;
    let finished = state != "pending";
    Some(CheckRun {
        id: status["id"].as_u64().unwrap_or(0),
        name: status["context"].as_str()?.to_owned(),
        status: if finished { "completed" } else { "pending" }.to_owned(),
        conclusion: finished.then(|| state.to_owned()),
        url: status["target_url"].as_str().unwrap_or_default().to_owned(),
        app: None,
    })
}

/// The entries of the list `list` in one page of a response.
fn entries(page: &Value, list: &str) -> Vec<Value> {
    page[list].as_array().cloned().unwrap_or_default()
}

/// gh's first line on stderr.
fn failed(stderr: &[u8]) -> GhError {
    GhError(
        String::from_utf8_lossy(stderr)
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("it failed and said nothing")
            .trim()
            .to_owned(),
    )
}

/// A GitHub held in memory, for tests of code that waits on CI.
#[derive(Default)]
pub struct FakePullRequests {
    pub state: Mutex<FakeChecks>,
}

#[derive(Default)]
pub struct FakeChecks {
    /// The URL every pull request resolves to; none when no pull request is found.
    pub url: Option<String>,
    pub head: String,
    pub runs: Vec<CheckRun>,
    pub statuses: Vec<CheckRun>,
    /// Each job's log.
    pub logs: Vec<(u64, String)>,
    /// Bumped on every change, and served as the ETag.
    pub version: u64,
    /// The listener the next `listen` returns; none makes `listen` fail.
    pub listener: Option<Listener>,
    /// How many times the check runs were read.
    pub reads: u64,
}

impl FakePullRequests {
    fn state(&self) -> std::sync::MutexGuard<'_, FakeChecks> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl PullRequests for FakePullRequests {
    fn url(&self, pr: &str, _cwd: &Path) -> Result<String, GhError> {
        self.state()
            .url
            .clone()
            .ok_or_else(|| GhError(format!("no pull request {pr}")))
    }

    fn head(&self, _pr: &PullRequest, etag: Option<&str>) -> Result<Fetched<String>, GhError> {
        let state = self.state();
        Ok(fresh(&state, etag, state.head.clone()))
    }

    fn check_runs(
        &self,
        _pr: &PullRequest,
        _head: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<CheckRun>>, GhError> {
        let mut state = self.state();
        state.reads += 1;
        Ok(fresh(&state, etag, state.runs.clone()))
    }

    fn statuses(
        &self,
        _pr: &PullRequest,
        _head: &str,
        etag: Option<&str>,
    ) -> Result<Fetched<Vec<CheckRun>>, GhError> {
        let state = self.state();
        Ok(fresh(&state, etag, state.statuses.clone()))
    }

    fn job_log(&self, _pr: &PullRequest, job: u64) -> Result<String, GhError> {
        self.state()
            .logs
            .iter()
            .find(|(id, _)| *id == job)
            .map(|(_, log)| log.clone())
            .ok_or_else(|| GhError(format!("no log for job {job}")))
    }

    fn listen(&self, _pr: &PullRequest) -> Result<Listener, GhError> {
        self.state()
            .listener
            .take()
            .ok_or_else(|| GhError("event subscriptions are not enabled".to_owned()))
    }
}

fn fresh<T>(state: &FakeChecks, etag: Option<&str>, value: T) -> Fetched<T> {
    let current = format!("\"{}\"", state.version);
    if etag == Some(current.as_str()) {
        Fetched::NotModified
    } else {
        Fetched::Fresh {
            etag: Some(current),
            value,
        }
    }
}
