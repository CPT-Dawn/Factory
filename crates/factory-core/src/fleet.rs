//! The CI the factory watches (spec #67, stories 25 to 29 and 42): for each configured repo, its
//! self-hosted runner fleet, its queued and running workflow jobs, and the latest check state of
//! each open `factory/*` pull request and of main's latest head.
//!
//! A repo needs watching while its last answer showed an open factory pull request or a job not
//! yet completed. A read that fails keeps the last good answer, marked with why, so a network
//! blip shows as old numbers rather than as no runners.

use serde::{Deserialize, Serialize};

use crate::ci::{self, CheckRun, Verdict};
use crate::exit::Exit;

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
    /// When the watcher reads next, in milliseconds since the epoch; none while nothing needs
    /// watching, when only a `factory wait ci` on a watched repo wakes it.
    #[serde(default)]
    pub next: Option<i64>,
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
    /// The latest completed jobs, newest first, with the times the database keeps.
    #[serde(default)]
    pub finished: Vec<Job>,
    /// Where [`MAIN`]'s latest head stands; none before it was ever read.
    #[serde(default)]
    pub main: Option<Main>,
}

/// The branch whose latest head the watcher reads in each repo: with lean CI, its runs are the
/// only signal that what merged works together.
pub const MAIN: &str = "main";

/// A repo's [`MAIN`]: its latest head, and where that head's checks stand, by the newest run
/// of each.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Main {
    pub sha: String,
    pub state: PullState,
}

impl Main {
    /// `red`, `green` or `pending`.
    pub fn word(&self) -> &'static str {
        match self.state {
            PullState::Failing => "red",
            PullState::Passing => "green",
            PullState::Pending => "pending",
        }
    }

    /// The head's first 7 characters, as git abbreviates it.
    pub fn short(&self) -> &str {
        self.sha.get(..7).unwrap_or(&self.sha)
    }

    /// Where a person reads the head of `repo` (`owner/name`).
    pub fn url(&self, repo: &str) -> String {
        format!("https://github.com/{repo}/commit/{}", self.sha)
    }
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
    /// Each state with GitHub's `status` and `busy` for it.
    const GITHUB: [(RunnerState, &'static str, bool); 3] = [
        (RunnerState::Busy, "online", true),
        (RunnerState::Idle, "online", false),
        (RunnerState::Offline, "offline", false),
    ];

    /// From GitHub's `status` (`online` or `offline`) and `busy`: offline for any other.
    pub fn of(status: &str, busy: bool) -> RunnerState {
        Self::GITHUB
            .into_iter()
            .find(|&(_, s, b)| s == status && b == busy)
            .map_or(RunnerState::Offline, |(state, _, _)| state)
    }

    /// GitHub's `status` and `busy` for this state: [`RunnerState::of`] reversed.
    pub fn github(self) -> (&'static str, bool) {
        Self::GITHUB
            .into_iter()
            .find(|&(state, _, _)| state == self)
            .map_or(("offline", false), |(_, status, busy)| (status, busy))
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
    /// Where a head's runs stand, by the newest run of each check: failing as soon as one
    /// failed, even while others run, and once all finished with one cancelled.
    pub fn of(runs: &[CheckRun]) -> PullState {
        if ci::latest(runs)
            .iter()
            .any(|run| run.bucket() == ci::Bucket::Failed)
        {
            return PullState::Failing;
        }
        match ci::verdict(runs) {
            Verdict::Pending { .. } => PullState::Pending,
            Verdict::Finished(tally) if tally.exit() == Exit::Ok => PullState::Passing,
            Verdict::Finished(_) => PullState::Failing,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            PullState::Passing => "passing",
            PullState::Failing => "failing",
            PullState::Pending => "pending",
        }
    }

    /// The state [`PullState::word`] names; pending for a word it never gives.
    pub fn parse(word: &str) -> PullState {
        [PullState::Passing, PullState::Failing]
            .into_iter()
            .find(|state| state.word() == word)
            .unwrap_or(PullState::Pending)
    }
}

/// Whether `branch` is one the factory works on.
pub fn is_factory_branch(branch: &str) -> bool {
    branch.starts_with(BRANCH_PREFIX)
}

/// The `owner/name` a git remote's URL names on github.com, in any way git writes one
/// (`git@github.com:o/n.git`, `https://github.com/o/n`, `ssh://git@github.com/o/n.git`);
/// none for a remote elsewhere, or one that names no repo.
pub fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .or_else(|| url.strip_prefix("ssh://"))?;
        let host = rest.split('/').next()?;
        let host = host.rsplit('@').next()?;
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        rest.split_once('/')?.1
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    is_repo(path).then(|| path.to_owned())
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
    /// Repos whose main's latest head failed.
    pub main_failing: usize,
    /// Repos whose main's latest head is still checked.
    pub main_pending: usize,
}

/// Where the watched CI stands, as the bar's chip shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// No repo has answered yet.
    Unknown,
    /// A repo's main failed on its latest head: louder than a pull request, as everything
    /// merged after it lands on red.
    MainFailing,
    /// An open factory pull request has a failed check.
    Failing,
    /// A pull request's or main's checks or a job are still going.
    Busy,
    /// Nothing failing and nothing going.
    Passing,
}

impl Tally {
    /// `2/7 busy, 1 offline, 2 missing`.
    pub fn runners_text(&self) -> String {
        let mut text = format!("{}/{} busy", self.busy, self.total);
        if self.offline > 0 {
            text.push_str(&format!(", {} offline", self.offline));
        }
        if self.missing > 0 {
            text.push_str(&format!(", {} missing", self.missing));
        }
        text
    }

    /// `3 queued, oldest 12m` at `now`, or `none queued`.
    pub fn queue_text(&self, now: i64) -> String {
        match (self.queued, self.oldest) {
            (0, _) => "none queued".to_owned(),
            (queued, Some(oldest)) => format!(
                "{queued} queued, oldest {}",
                crate::lab::ago(now / 1000, oldest / 1000)
            ),
            (queued, None) => format!("{queued} queued"),
        }
    }
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
        if tally.main_failing > 0 {
            Mark::MainFailing
        } else if tally.failing > 0 {
            Mark::Failing
        } else if tally.pending + tally.main_pending + tally.queued + tally.running > 0 {
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

    /// Whether the owner watches repos, or tried to: the bar's CI chip and card and the status
    /// screen's CI row show then.
    pub fn shown(&self) -> bool {
        !self.repos.is_empty() || self.error.is_some()
    }

    /// `stale 4m · main red shop@42fdcd8 · PRs 1 failing, 1 pending · main green web@0b1c2d3 ·
    /// 2/7 busy, 1 offline · 3 queued, oldest 12m`: how old the numbers are while a read
    /// fails, each red main, the open factory pull requests, each other main, the runners and
    /// the queue, the least needed last where a screen cuts it.
    pub fn summary(&self, now: i64) -> String {
        if !self.answered() {
            return match self.error() {
                Some(why) => format!("unread: {why}"),
                None => "not read yet".to_owned(),
            };
        }
        let tally = self.tally();
        let mut parts = Vec::new();
        if let Some(since) = self.stale_since() {
            parts.push(format!(
                "stale {}",
                crate::lab::ago(now / 1000, since / 1000)
            ));
        }
        let mains = |red: bool| {
            self.repos.iter().filter_map(move |repo| {
                let main = repo.main.as_ref()?;
                let name = repo.repo.rsplit('/').next().unwrap_or(&repo.repo);
                ((main.state == PullState::Failing) == red)
                    .then(|| format!("main {} {name}@{}", main.word(), main.short()))
            })
        };
        parts.extend(mains(true));
        let pulls: usize = self.repos.iter().map(|repo| repo.pulls.len()).sum();
        let passing = pulls - tally.failing - tally.pending;
        let states: Vec<String> = [
            (tally.failing, PullState::Failing),
            (tally.pending, PullState::Pending),
            (passing, PullState::Passing),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, state)| format!("{count} {}", state.word()))
        .collect();
        parts.push(if states.is_empty() {
            "no open PRs".to_owned()
        } else {
            format!("PRs {}", states.join(", "))
        });
        parts.extend(mains(false));
        parts.push(tally.runners_text());
        parts.push(tally.queue_text(now));
        parts.join(" · ")
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
            match repo.main.as_ref().map(|main| main.state) {
                Some(PullState::Failing) => tally.main_failing += 1,
                Some(PullState::Pending) => tally.main_pending += 1,
                Some(PullState::Passing) | None => {}
            }
        }
        let listed = tally.busy + tally.idle + tally.offline;
        let expected = usize::try_from(self.expected).unwrap_or(usize::MAX);
        tally.missing = expected.saturating_sub(listed);
        tally.total = expected.max(listed);
        tally
    }
}
