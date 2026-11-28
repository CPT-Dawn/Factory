//! The daemon's CI watcher (spec #67, stories 25 to 29 and 42): it reads each `[ci]` repo's
//! self-hosted runners, its queued and running workflow jobs, the checks of its open
//! `factory/*` pull requests and those of its main's latest head, for the status screen and the
//! bar. Main is read with the rest of the repo, so it adds no read while nothing needs watching.
//!
//! It reads every repo once as the daemon starts, so the runner fleet shows even when nothing
//! needs watching (#79). After that it reads only while something needs watching: a
//! `factory wait ci` on one of the repos'
//! pull requests wakes it, and it reads a repo every minute while the last answer shows an open
//! factory pull request or a job not yet completed, or while the factory has live work in it: a
//! lead herdr shows at work in a checkout of the repo (its `origin` on github.com), while one of
//! the repo's worktrees is on a `factory/*` branch. Reviews and waits count through the lead
//! they run under. Live work is looked at again on each change to the leads and every minute,
//! with git and herdr's picture only, so an idle factory costs GitHub nothing past that first
//! read. A read that fails keeps the last good answer, marked with why, and every answer is kept in the database, so a restarted daemon resumes the watch.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use factory_adapters::gh::{Actions, GhActions, GhError, HOST};
use factory_adapters::git::GitRepo;
use factory_core::ci::Watched;
use factory_core::fleet::{
    Ci, Job, MAIN, Main, Pull, PullState, RepoCi, github_repo, is_factory_branch,
};
use tokio::sync::watch;

use crate::clock;
use crate::config::CiWatch;
use crate::output::Failure;
use crate::store::Store;

/// How long the watcher leaves between reads, on the lab's clock.
const INTERVAL_MS: i64 = 60_000;
/// How often a watcher with something to watch looks at the clock.
const TICK: Duration = Duration::from_secs(1);
/// How far before the last good answer a read looks for runs updated since, for the clocks of
/// GitHub and of this machine to disagree.
const OVERLAP_MS: i64 = 120_000;
/// What names the repos, of those it is given, that the factory has live work in.
pub type LiveWork = Arc<dyn Fn(&[String]) -> Vec<String> + Send + Sync>;

/// How often an idle watcher looks again for live work, on top of each change to the leads.
const LIVE_LOOK: Duration = Duration::from_secs(60);
/// How far back the first read of a repo looks for runs updated since.
const LOOKBACK_MS: i64 = 3_600_000;
/// What a failed GitHub read tells the owner to do.
const RETRY: &str = "the last good answer stays shown; check `gh auth status` and the [ci] repos";

pub struct Fleet<A: Actions = GhActions> {
    actions: Arc<A>,
    store: Mutex<Store>,
    ci: Mutex<Ci>,
}

impl<A: Actions> Fleet<A> {
    /// The watcher of `watch`'s repos, from the answers the database kept; when the config
    /// can't be read (`watch` is that failure), one that watches nothing and says why.
    pub fn open(watch: Result<CiWatch, Failure>, store: Store, actions: A) -> Fleet<A> {
        let ci = match watch {
            Ok(watch) => Ci {
                expected: watch.expected,
                repos: watch
                    .repos
                    .iter()
                    .map(|repo| {
                        // The next good read replaces what couldn't be read.
                        store.ci(repo).unwrap_or_else(|e| RepoCi {
                            repo: repo.clone(),
                            error: Some(format!(
                                "cannot read the CI the database keeps: {e}; check \
                                 FACTORY_STATE, or move factory.db aside"
                            )),
                            ..RepoCi::default()
                        })
                    })
                    .collect(),
                ..Ci::default()
            },
            Err(failure) => Ci {
                error: Some(failure.message),
                ..Ci::default()
            },
        };
        Fleet {
            actions: Arc::new(actions),
            store: Mutex::new(store),
            ci: Mutex::new(ci),
        }
    }

    /// The configured repos' CI as GitHub last answered it.
    pub fn snapshot(&self) -> Ci {
        self.ci().clone()
    }

    fn ci(&self) -> MutexGuard<'_, Ci> {
        self.ci
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `pr` is in one of the watched repos.
    fn watches(&self, pr: &Watched) -> bool {
        let name = format!("{}/{}", pr.owner(), pr.repo());
        pr.host().eq_ignore_ascii_case(HOST)
            && self
                .ci()
                .repos
                .iter()
                .any(|repo| repo.repo.eq_ignore_ascii_case(&name))
    }

    fn needs_watching(&self) -> bool {
        self.ci().repos.iter().any(RepoCi::needs_watching)
    }

    /// Watches until the daemon stops; `watched` hears each pull request a wait watches,
    /// `leads` ticks on each change to the leads, and `live` names the repos, of those it is
    /// given, the factory has live work in.
    pub async fn run(
        self: Arc<Self>,
        mut watched: watch::Receiver<Option<Watched>>,
        leads: watch::Receiver<u64>,
        live: LiveWork,
    ) {
        let repos: Vec<String> = self.ci().repos.iter().map(|r| r.repo.clone()).collect();
        if repos.is_empty() {
            return;
        }
        let mut leads = Some(leads);
        // Every repo is read once as the daemon starts, so its runner fleet shows even on a
        // night with nothing to watch (#79); after that, only what needs watching is read.
        let mut asked = true;
        // The repos a wait woke the read for: their main is read too, as it is while a repo
        // needs watching or is busy, but not on the read at start of a factory with nothing
        // to watch, nor for a repo nothing watches.
        let mut woken: Vec<String> = Vec::new();
        let mut last: Option<i64> = None;
        // The repos with live work, as last looked; looked at again once `look` is set.
        let mut busy: Vec<String> = Vec::new();
        let mut look = false;
        let mut looked: Option<std::time::Instant> = None;
        loop {
            // A burst of changes to the leads is one look, at most one each tick.
            let stale =
                looked.is_none_or(|at| at.elapsed() >= LIVE_LOOK || (look && at.elapsed() >= TICK));
            if stale {
                look = false;
                looked = Some(std::time::Instant::now());
                let (live, repos) = (Arc::clone(&live), repos.clone());
                busy = tokio::task::spawn_blocking(move || live(&repos))
                    .await
                    .unwrap_or_default();
            }
            let wanted = asked || self.needs_watching() || !busy.is_empty();
            let due = last.is_none_or(|at| clock::lab_now_ms().saturating_sub(at) >= INTERVAL_MS);
            if wanted && due {
                let all = std::mem::take(&mut asked);
                let woken = std::mem::take(&mut woken);
                last = Some(clock::lab_now_ms());
                let (fleet, only) = (Arc::clone(&self), busy.clone());
                let _ =
                    tokio::task::spawn_blocking(move || fleet.read_all(all, &woken, &only)).await;
                let next = (self.needs_watching() || !busy.is_empty())
                    .then(|| last.map(|at| at + INTERVAL_MS));
                self.ci().next = next.flatten();
                continue;
            }
            let wait = if wanted || look { TICK } else { LIVE_LOOK };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = changed(&mut leads) => look = true,
                changed = watched.changed() => {
                    // The pollers are gone: the daemon is stopping.
                    if changed.is_err() {
                        return;
                    }
                    let pr = watched.borrow_and_update().clone();
                    if let Some(pr) = pr.filter(|pr| self.watches(pr)) {
                        asked = true;
                        woken.push(format!("{}/{}", pr.owner(), pr.repo()));
                    }
                }
            }
        }
    }

    /// Reads the repos once, keeping each answer, or the last good one marked with why: every
    /// repo with `all`, else each one that needs watching or is named in `busy`. Main is read
    /// only for a repo named in `woken` (a wait woke the read for it), busy or needing watching.
    fn read_all(&self, all: bool, woken: &[String], busy: &[String]) {
        let count = self.ci().repos.len();
        for index in 0..count {
            let Some(before) = self.ci().repos.get(index).cloned() else {
                return;
            };
            let watched = before.needs_watching()
                || busy
                    .iter()
                    .any(|repo| repo.eq_ignore_ascii_case(&before.repo));
            if !all && !watched {
                continue;
            }
            let woke = woken
                .iter()
                .any(|repo| repo.eq_ignore_ascii_case(&before.repo));
            let read = read(&*self.actions, &before, woke || watched);
            let after = match read {
                Ok((mut after, jobs)) => {
                    after.seen = Some(clock::now().unix_millis());
                    let mut store = self
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let kept = store
                        .record_ci(&after, &jobs)
                        .and_then(|()| store.ci(&after.repo));
                    match kept {
                        Ok(kept) => after.finished = kept.finished,
                        Err(e) => {
                            // Until the database keeps their end, the jobs it kept as not
                            // completed stay tracked: the next read reads their runs again.
                            for job in &before.jobs {
                                if after.jobs.iter().all(|read| read.id != job.id) {
                                    after.jobs.push(job.clone());
                                }
                            }
                            after.jobs.sort_by_key(|job| job.id);
                            after.finished.clone_from(&before.finished);
                            after.error = Some(format!(
                                "cannot keep GitHub's answer: {e}; check the disk and \
                                 FACTORY_STATE"
                            ));
                        }
                    }
                    after
                }
                Err(GhError(why)) => RepoCi {
                    error: Some(format!("{why}; {RETRY}")),
                    ..before
                },
            };
            if let Some(slot) = self.ci().repos.get_mut(index) {
                *slot = after;
            }
        }
        self.actions.sweep();
    }
}

/// One read of `before`'s repo: its runners, the jobs of its active runs, of the latest runs
/// updated since the last good answer (one that queued and completed between two reads, or a
/// re-run, among them) and of the runs whose jobs `before` still waited on, and its open
/// factory pull requests' checks, and its main's latest head's when the repo is `watched` or
/// the read finds it needs watching (else main stays as `before` saw it, so an idle factory
/// asks nothing of main). Also every job read, completed ones too, for the times the database
/// keeps.
fn read<A: Actions>(
    actions: &A,
    before: &RepoCi,
    watched: bool,
) -> Result<(RepoCi, Vec<Job>), GhError> {
    let repo = before.repo.as_str();
    let runners = actions.runners(repo)?;
    let now = clock::now().unix_millis();
    let since = before
        .seen
        .map_or(now - LOOKBACK_MS, |seen| seen - OVERLAP_MS);
    let mut runs = actions.active_runs(repo)?;
    runs.extend(actions.runs_since(repo, since)?);
    runs.extend(before.jobs.iter().map(|job| job.run_id));
    runs.sort_unstable();
    runs.dedup();
    let mut read = Vec::new();
    for run in runs {
        read.extend(actions.jobs(repo, run)?);
    }
    let mut pulls = Vec::new();
    for open in actions.open_pulls(repo)? {
        if !is_factory_branch(&open.branch) {
            continue;
        }
        let state = PullState::of(&actions.checks(repo, &open.head)?);
        pulls.push(Pull {
            number: open.number,
            branch: open.branch,
            url: open.url,
            state,
        });
    }
    pulls.sort_by_key(|pull| pull.number);
    let mut jobs: Vec<Job> = read
        .iter()
        .filter(|job| !job.completed())
        .cloned()
        .collect();
    jobs.sort_by_key(|job| job.id);
    let main = if watched || !pulls.is_empty() || !jobs.is_empty() {
        let sha = actions.branch_head(repo, MAIN)?;
        let state = PullState::of(&actions.checks(repo, &sha)?);
        Some(Main { sha, state })
    } else {
        before.main.clone()
    };
    Ok((
        RepoCi {
            repo: before.repo.clone(),
            seen: None,
            error: None,
            runners,
            jobs,
            pulls,
            finished: Vec::new(),
            main,
        },
        read,
    ))
}

/// The next tick of `leads`; never, once its sender is gone.
async fn changed(leads: &mut Option<watch::Receiver<u64>>) {
    if let Some(receiver) = leads.as_mut()
        && receiver.changed().await.is_ok()
    {
        return;
    }
    *leads = None;
    std::future::pending::<()>().await;
}

/// The repos among `repos` (`owner/name`, any case) that a checkout at one of `cwds` works
/// on: one whose `origin` is the repo on github.com, while one of the repo's worktrees is on a
/// `factory/*` branch. Asks git only.
pub fn live_repos(repos: &[String], cwds: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut live: Vec<String> = Vec::new();
    for cwd in cwds {
        let repo = GitRepo::at(std::path::Path::new(&cwd));
        let Some(named) = repo.origin_url().as_deref().and_then(github_repo) else {
            continue;
        };
        let Some(configured) = repos.iter().find(|r| r.eq_ignore_ascii_case(&named)) else {
            continue;
        };
        if live.contains(configured) {
            continue;
        }
        if repo
            .worktree_branches()
            .is_ok_and(|branches| branches.iter().any(|b| is_factory_branch(b)))
        {
            live.push(configured.clone());
        }
    }
    live
}
