//! A pull request's checks: which runs count as passed, skipped, failed or still pending, and
//! when a wait on them is over (spec #1, user story 3).
//!
//! Only the newest run of each check counts: a re-run, or a workflow run that a newer one on the
//! same head cancelled, leaves older runs of the same name behind, and those say nothing about
//! the head any more. The newest is the one with the highest id (GitHub's ids only grow); a
//! check is its name and the app that reports it, so a commit status never replaces a check run.
//!
//! A run is pending until it completes. A completed run passed on `success`, was skipped on
//! `skipped` or `neutral`, was cancelled on `cancelled`, and failed on any other conclusion
//! (`failure`, `timed_out`, `action_required`, `startup_failure`, `stale`, or one GitHub adds
//! later), so only a run that finished can fail. A cancelled run is counted apart, as it is
//! mostly fail-fast's work beside the one that failed; a head whose newest run of some check was
//! cancelled still needs attention, as that check never verified it. A head with no runs yet is
//! pending: CI has not reported. A commit status (the older API some CI services still report
//! through) is a run too: pending until it is `success`, `failure` or `error`.
//!
//! None of it knows about pull requests beyond [`PullRequest`]: [`verdict`] reads the runs of
//! any head, a branch's as well as a pull request's.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::exit::Exit;

/// A pull request on a GitHub host.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PullRequest {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PullRequest {
    /// Reads `https://<host>/<owner>/<repo>/pull/<number>`, as `gh pr view --json url` gives it.
    pub fn from_url(url: &str) -> Result<PullRequest, BadPullRequest> {
        let bad = || BadPullRequest(url.to_owned());
        let rest = url.trim().strip_prefix("https://").ok_or_else(bad)?;
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        let [host, owner, repo, "pull", number] = parts.as_slice() else {
            return Err(bad());
        };
        if [host, owner, repo].iter().any(|part| part.is_empty()) {
            return Err(bad());
        }
        let number = number.parse().ok().filter(|n| *n > 0).ok_or_else(bad)?;
        Ok(PullRequest {
            host: (*host).to_owned(),
            owner: (*owner).to_owned(),
            repo: (*repo).to_owned(),
            number,
        })
    }

    pub fn url(&self) -> String {
        format!(
            "https://{}/{}/{}/pull/{}",
            self.host, self.owner, self.repo, self.number
        )
    }
}

/// `<owner>/<repo>#<number>`.
impl fmt::Display for PullRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadPullRequest(pub String);

impl fmt::Display for BadPullRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a pull request URL like https://github.com/<owner>/<repo>/pull/<number>",
            self.0
        )
    }
}

impl std::error::Error for BadPullRequest {}

/// One check run on a pull request's head, as GitHub reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRun {
    /// Its id, which is the job's id for a GitHub Actions run.
    pub id: u64,
    pub name: String,
    /// `queued`, `in_progress`, `completed`, ...
    pub status: String,
    /// Set once it completed: `success`, `failure`, `skipped`, ...
    pub conclusion: Option<String>,
    /// Where a person reads it.
    pub url: String,
    /// The app that reported it (`github-actions` keeps its log behind the jobs API); none for a
    /// commit status.
    pub app: Option<String>,
}

/// What a check run counts as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Pending,
    Passed,
    Skipped,
    Cancelled,
    Failed,
}

impl CheckRun {
    pub fn bucket(&self) -> Bucket {
        if self.status != "completed" {
            return Bucket::Pending;
        }
        match self.conclusion.as_deref() {
            None => Bucket::Pending,
            Some("success") => Bucket::Passed,
            Some("skipped" | "neutral") => Bucket::Skipped,
            Some("cancelled") => Bucket::Cancelled,
            Some(_) => Bucket::Failed,
        }
    }

    /// Whether it is a GitHub Actions job that ran, so the jobs API has a log for it: a
    /// cancelled job, or one that never started (`startup_failure`, `action_required`,
    /// `stale`), answers 404. Other apps and commit statuses keep their logs at the URL.
    pub fn has_log(&self) -> bool {
        self.app.as_deref() == Some(ACTIONS)
            && !matches!(
                self.conclusion.as_deref(),
                Some("cancelled" | "startup_failure" | "action_required" | "stale" | "skipped")
            )
    }
}

/// The app whose check runs are GitHub Actions jobs, with logs behind the jobs API.
pub const ACTIONS: &str = "github-actions";

/// The newest run of each check, in the order GitHub listed them.
pub fn latest(runs: &[CheckRun]) -> Vec<CheckRun> {
    let mut newest: HashMap<(Option<&str>, &str), u64> = HashMap::new();
    for run in runs {
        let id = newest
            .entry((run.app.as_deref(), &run.name))
            .or_insert(run.id);
        *id = (*id).max(run.id);
    }
    let mut kept = HashSet::new();
    runs.iter()
        .filter(|run| {
            let key = (run.app.as_deref(), run.name.as_str());
            newest[&key] == run.id && kept.insert(key)
        })
        .cloned()
        .collect()
}

/// Where a pull request's checks stand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Some run is still going, or none has been reported yet.
    Pending { pending: usize },
    /// Every run finished.
    Finished(Tally),
}

/// The runs of a head whose checks all finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tally {
    pub passed: usize,
    pub skipped: usize,
    /// The failed runs, in the order GitHub listed them.
    pub failed: Vec<CheckRun>,
    /// The cancelled runs, in the order GitHub listed them.
    pub cancelled: Vec<CheckRun>,
}

impl Tally {
    /// 0 when every check passed or was skipped, 10 when one failed or was cancelled.
    pub fn exit(&self) -> Exit {
        if self.failed.is_empty() && self.cancelled.is_empty() {
            Exit::Ok
        } else {
            Exit::NeedsAttention
        }
    }
}

/// Where the runs of one head stand, by the newest run of each check.
pub fn verdict(runs: &[CheckRun]) -> Verdict {
    let mut tally = Tally {
        passed: 0,
        skipped: 0,
        failed: Vec::new(),
        cancelled: Vec::new(),
    };
    let mut pending = 0;
    for run in latest(runs) {
        match run.bucket() {
            Bucket::Pending => pending += 1,
            Bucket::Passed => tally.passed += 1,
            Bucket::Skipped => tally.skipped += 1,
            Bucket::Cancelled => tally.cancelled.push(run),
            Bucket::Failed => tally.failed.push(run),
        }
    }
    if pending > 0 || runs.is_empty() {
        Verdict::Pending { pending }
    } else {
        Verdict::Finished(tally)
    }
}

/// The last `lines` lines of a log, without the blank ones at its end, and without the
/// terminal escape sequences (colours, links) a job's log is full of.
pub fn tail(log: &str, lines: usize) -> Vec<String> {
    let all: Vec<String> = log.lines().map(plain).collect();
    let end = all
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);
    all[end.saturating_sub(lines)..end].to_vec()
}

/// A line without its escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] … BEL` or
/// `ESC ] … ESC \`) and the two-byte ones (`ESC x`).
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // Parameters and intermediates, up to the final byte in @..=~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
