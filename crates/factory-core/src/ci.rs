//! A pull request's checks: which runs count as passed, skipped, failed or still pending, and
//! when a wait on them is over (spec #1, user story 3).
//!
//! A run is pending until it completes. A completed run passed on `success`, was skipped on
//! `skipped` or `neutral`, and failed on any other conclusion (`failure`, `cancelled`,
//! `timed_out`, `action_required`, `startup_failure`, `stale`, or one GitHub adds later), so
//! only a run that finished can fail. A head with no runs yet is pending: CI has not reported.

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
}

/// What a check run counts as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Pending,
    Passed,
    Skipped,
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
            Some(_) => Bucket::Failed,
        }
    }
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
}

impl Tally {
    /// 0 when nothing failed, 10 when something did.
    pub fn exit(&self) -> Exit {
        if self.failed.is_empty() {
            Exit::Ok
        } else {
            Exit::NeedsAttention
        }
    }
}

/// Where the runs of one head stand.
pub fn verdict(runs: &[CheckRun]) -> Verdict {
    let mut tally = Tally {
        passed: 0,
        skipped: 0,
        failed: Vec::new(),
    };
    let mut pending = 0;
    for run in runs {
        match run.bucket() {
            Bucket::Pending => pending += 1,
            Bucket::Passed => tally.passed += 1,
            Bucket::Skipped => tally.skipped += 1,
            Bucket::Failed => tally.failed.push(run.clone()),
        }
    }
    if pending > 0 || runs.is_empty() {
        Verdict::Pending { pending }
    } else {
        Verdict::Finished(tally)
    }
}

/// The last `lines` lines of a log, without the blank ones at its end.
pub fn tail(log: &str, lines: usize) -> Vec<&str> {
    let all: Vec<&str> = log.lines().collect();
    let end = all
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);
    all[end.saturating_sub(lines)..end].to_vec()
}
