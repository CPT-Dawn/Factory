//! The CI the factory watches (spec #67, stories 25 to 29 and 42): for each configured repo, its
//! self-hosted runner fleet, its queued and running workflow jobs, and the latest check state of
//! each open `factory/*` pull request.
//!
//! A repo needs watching while its last answer showed an open factory pull request or a job not
//! yet completed. A read that fails keeps the last good answer, marked with why, so a network
//! blip shows as old numbers rather than as no runners.

use serde::{Deserialize, Serialize};

use crate::ci::{self, CheckRun, Verdict};

/// The branches whose pull requests the factory opened.
pub const BRANCH_PREFIX: &str = "factory/";

/// What the daemon knows of the configured repos' CI.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ci {
    /// How many runners the owner runs across these repos; 0 when unset.
    pub expected: u32,
    /// Why the watcher watches nothing: the config can't be read.
    #[serde(default)]
    pub error: Option<String>,
    /// In the config's order.
    pub repos: Vec<RepoCi>,
}

/// One repo's CI, as its last good answer showed it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCi {
    /// `owner/name`.
    pub repo: String,
    /// When GitHub last answered every read, in milliseconds since the epoch; none before it
    /// ever has.
    pub seen: Option<i64>,
    /// Why the latest read failed; none when it answered.
    pub error: Option<String>,
    pub runners: Vec<Runner>,
    /// The jobs not yet completed.
    pub jobs: Vec<Job>,
    /// The open pull requests from `factory/*` branches.
    pub pulls: Vec<Pull>,
}

/// One self-hosted runner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub state: RunnerState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerState {
    Idle,
    Busy,
    Offline,
}

impl RunnerState {
    /// From GitHub's `status` (`online` or `offline`) and `busy`.
    pub fn of(status: &str, busy: bool) -> RunnerState {
        match (status, busy) {
            ("online", true) => RunnerState::Busy,
            ("online", false) => RunnerState::Idle,
            _ => RunnerState::Offline,
        }
    }
}

/// One job of a workflow run. Its times are milliseconds since the epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    /// `queued`, `in_progress`, `completed`, ...
    pub status: String,
    pub conclusion: Option<String>,
    /// The runner it runs or ran on.
    pub runner: Option<String>,
    /// When it was queued.
    pub queued: Option<i64>,
    /// When a runner took it; none while it waits, whatever GitHub's `started_at` says then.
    pub started: Option<i64>,
    /// When it completed.
    pub finished: Option<i64>,
}

impl Job {
    pub fn completed(&self) -> bool {
        self.status == "completed"
    }

    /// Waiting for a runner: neither running nor completed.
    pub fn waiting(&self) -> bool {
        !self.completed() && self.status != "in_progress"
    }
}

/// An open pull request from a `factory/*` branch, and where its head's checks stand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub branch: String,
    pub url: String,
    pub state: PullState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    /// Every check finished and none failed.
    Passing,
    /// Some check finished and failed.
    Failing,
    /// Some check is still going, or none has reported yet.
    Pending,
}

impl PullState {
    /// Where a head's runs stand: failing as soon as one failed, even while others run.
    pub fn of(runs: &[CheckRun]) -> PullState {
        if runs.iter().any(|run| run.bucket() == ci::Bucket::Failed) {
            return PullState::Failing;
        }
        match ci::verdict(runs) {
            Verdict::Pending { .. } => PullState::Pending,
            Verdict::Finished(_) => PullState::Passing,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            PullState::Passing => "passing",
            PullState::Failing => "failing",
            PullState::Pending => "pending",
        }
    }
}

/// Whether `branch` is one the factory works on.
pub fn is_factory_branch(branch: &str) -> bool {
    branch.starts_with(BRANCH_PREFIX)
}

/// Whether `repo` reads as `owner/name`.
pub fn is_repo(repo: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    matches!(repo.split_once('/'), Some((owner, name)) if part(owner) && part(name))
}

impl RepoCi {
    /// An open factory pull request or a job not yet completed, as its last answer showed.
    pub fn needs_watching(&self) -> bool {
        !self.pulls.is_empty() || self.jobs.iter().any(|job| !job.completed())
    }
}

/// The fleet across every repo, as the bar sums it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub busy: usize,
    pub idle: usize,
    pub offline: usize,
    /// Expected runners that no repo lists.
    pub missing: usize,
    /// The runners to count busy ones against: the expected number, or the listed ones when
    /// more are listed or none is expected.
    pub total: usize,
    /// Jobs waiting for a runner.
    pub queued: usize,
    /// When the longest-waiting of them was queued.
    pub oldest: Option<i64>,
    /// Jobs on a runner now.
    pub running: usize,
    pub failing: usize,
    pub pending: usize,
}

/// Where the watched CI stands, as the bar's chip shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// No repo has answered yet.
    Unknown,
    /// An open factory pull request has a failed check.
    Failing,
    /// A pull request's checks or a job are still going.
    Busy,
    /// Nothing failing and nothing going.
    Passing,
}

impl Ci {
    /// Whether any repo has answered.
    pub fn answered(&self) -> bool {
        self.repos.iter().any(|repo| repo.seen.is_some())
    }

    pub fn mark(&self) -> Mark {
        if !self.answered() {
            return Mark::Unknown;
        }
        let tally = self.tally();
        if tally.failing > 0 {
            Mark::Failing
        } else if tally.pending + tally.queued + tally.running > 0 {
            Mark::Busy
        } else {
            Mark::Passing
        }
    }

    /// Since when the numbers are old: the last good answer of the oldest repo whose latest
    /// read failed; none while every repo that answered answers still.
    pub fn stale_since(&self) -> Option<i64> {
        self.repos
            .iter()
            .filter(|repo| repo.error.is_some())
            .filter_map(|repo| repo.seen)
            .min()
    }

    /// Why nothing can be shown: the config can't be read, or a repo never answered.
    pub fn error(&self) -> Option<String> {
        let mut why: Vec<String> = self.error.iter().cloned().collect();
        why.extend(
            self.repos
                .iter()
                .filter(|repo| repo.seen.is_none())
                .filter_map(|repo| Some(format!("{}: {}", repo.repo, repo.error.as_ref()?))),
        );
        (!why.is_empty()).then(|| why.join("; "))
    }

    /// `2/7 runners busy, 1 offline, 3 queued (oldest 12m), PRs 1 failing, 1 pending`, and how
    /// old the numbers are while a read fails.
    pub fn summary(&self, now: i64) -> String {
        let (fleet, pulls) = self.summary_lines(now);
        match pulls {
            Some(pulls) => format!("{fleet}, {pulls}"),
            None => fleet,
        }
    }

    /// The summary in two: the runners and the queue, then the pull requests and how old the
    /// numbers are; only the first before any repo answered.
    pub fn summary_lines(&self, now: i64) -> (String, Option<String>) {
        if !self.answered() {
            let first = match self.error() {
                Some(why) => format!("unread: {why}"),
                None => "not read yet".to_owned(),
            };
            return (first, None);
        }
        let tally = self.tally();
        let mut parts = vec![format!("{}/{} runners busy", tally.busy, tally.total)];
        if tally.offline > 0 {
            parts.push(format!("{} offline", tally.offline));
        }
        if tally.missing > 0 {
            parts.push(format!("{} missing", tally.missing));
        }
        parts.push(match (tally.queued, tally.oldest) {
            (0, _) => "none queued".to_owned(),
            (queued, Some(oldest)) => {
                format!(
                    "{queued} queued (oldest {})",
                    crate::lab::ago(now / 1000, oldest / 1000)
                )
            }
            (queued, None) => format!("{queued} queued"),
        });
        let fleet = parts.join(", ");
        let mut parts = Vec::new();
        let pulls: usize = self.repos.iter().map(|repo| repo.pulls.len()).sum();
        let passing = pulls - tally.failing - tally.pending;
        let states: Vec<String> = [
            (tally.failing, "failing"),
            (tally.pending, "pending"),
            (passing, "passing"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, word)| format!("{count} {word}"))
        .collect();
        parts.push(if states.is_empty() {
            "no open PRs".to_owned()
        } else {
            format!("PRs {}", states.join(", "))
        });
        if let Some(since) = self.stale_since() {
            parts.push(format!(
                "stale {}",
                crate::lab::ago(now / 1000, since / 1000)
            ));
        }
        (fleet, Some(parts.join(", ")))
    }

    pub fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        for repo in &self.repos {
            for runner in &repo.runners {
                match runner.state {
                    RunnerState::Busy => tally.busy += 1,
                    RunnerState::Idle => tally.idle += 1,
                    RunnerState::Offline => tally.offline += 1,
                }
            }
            for job in repo.jobs.iter().filter(|job| !job.completed()) {
                if job.waiting() {
                    tally.queued += 1;
                    tally.oldest = match (tally.oldest, job.queued) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                } else {
                    tally.running += 1;
                }
            }
            for pull in &repo.pulls {
                match pull.state {
                    PullState::Failing => tally.failing += 1,
                    PullState::Pending => tally.pending += 1,
                    PullState::Passing => {}
                }
            }
        }
        let listed = tally.busy + tally.idle + tally.offline;
        let expected = usize::try_from(self.expected).unwrap_or(usize::MAX);
        tally.missing = expected.saturating_sub(listed);
        tally.total = expected.max(listed);
        tally
    }
}
