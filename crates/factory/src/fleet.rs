//! The daemon's CI watcher (spec #67, stories 25 to 29 and 42): it reads each `[ci]` repo's
//! self-hosted runners, its queued and running workflow jobs and the checks of its open
//! `factory/*` pull requests, for the status screen and the bar.
//!
//! It reads only while something needs watching: a `factory wait ci` on one of the repos'
//! pull requests wakes it, and it reads again every minute while the last answer shows an open
//! factory pull request or a job not yet completed. Otherwise it waits without a timer, so an
//! idle factory costs GitHub nothing. A read that fails keeps the last good answer, marked with
//! why, and every answer is kept in the database, so a restarted daemon resumes the watch.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use factory_adapters::gh::{Actions, Gh, GhError, HOST};
use factory_core::ci::PullRequest;
use factory_core::fleet::{Ci, Job, Pull, PullState, RepoCi, is_factory_branch};
use tokio::sync::watch;

use crate::clock;
use crate::config::CiWatch;
use crate::output::Failure;
use crate::store::Store;

/// How long the watcher leaves between reads, on the lab's clock.
const INTERVAL_MS: i64 = 60_000;
/// How often a watcher with something to watch looks at the clock.
const TICK: Duration = Duration::from_secs(1);

pub struct Fleet<A: Actions = Gh> {
    actions: Arc<A>,
    store: Mutex<Store>,
    ci: Mutex<Ci>,
}

impl<A: Actions> Fleet<A> {
    /// The watcher of `watch`'s repos, from the answers the database kept; when the config
    /// can't be read (`watch` is that failure), one that watches nothing and says why.
    pub fn open(watch: Result<CiWatch, Failure>, store: Store, actions: A) -> Fleet<A> {
        let ci = match watch {
            Ok(watch) => {
                let mut error = None;
                let repos = watch
                    .repos
                    .iter()
                    .map(|repo| {
                        store.ci(repo).unwrap_or_else(|e| {
                            error = Some(format!("cannot read the CI the database keeps: {e}"));
                            RepoCi {
                                repo: repo.clone(),
                                ..RepoCi::default()
                            }
                        })
                    })
                    .collect();
                Ci {
                    expected: watch.expected,
                    error,
                    repos,
                }
            }
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
    fn watches(&self, pr: &PullRequest) -> bool {
        let name = format!("{}/{}", pr.owner, pr.repo);
        pr.host.eq_ignore_ascii_case(HOST)
            && self
                .ci()
                .repos
                .iter()
                .any(|repo| repo.repo.eq_ignore_ascii_case(&name))
    }

    fn needs_watching(&self) -> bool {
        self.ci().repos.iter().any(RepoCi::needs_watching)
    }

    /// Watches until the daemon stops; `watched` hears each pull request a wait watches.
    pub async fn run(self: Arc<Self>, mut watched: watch::Receiver<Option<PullRequest>>) {
        if self.ci().repos.is_empty() {
            return;
        }
        let mut asked = false;
        let mut last: Option<i64> = None;
        loop {
            let wanted = asked || self.needs_watching();
            let due = last.is_none_or(|at| clock::lab_now_ms().saturating_sub(at) >= INTERVAL_MS);
            if wanted && due {
                asked = false;
                last = Some(clock::lab_now_ms());
                let fleet = Arc::clone(&self);
                let _ = tokio::task::spawn_blocking(move || fleet.read_all()).await;
                continue;
            }
            let heard = if wanted {
                tokio::select! {
                    () = tokio::time::sleep(TICK) => None,
                    changed = watched.changed() => Some(changed),
                }
            } else {
                Some(watched.changed().await)
            };
            match heard {
                // The pollers are gone: the daemon is stopping.
                Some(Err(_)) => return,
                Some(Ok(())) => {
                    let pr = watched.borrow_and_update().clone();
                    asked |= pr.is_some_and(|pr| self.watches(&pr));
                }
                None => {}
            }
        }
    }

    /// Reads every repo once, keeping each answer, or the last good one marked with why.
    fn read_all(&self) {
        let count = self.ci().repos.len();
        for index in 0..count {
            let Some(before) = self.ci().repos.get(index).cloned() else {
                return;
            };
            let read = read(&*self.actions, &before);
            let after = match read {
                Ok((mut after, jobs)) => {
                    after.seen = Some(clock::now().unix_millis());
                    let store = self
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .record_ci(&after, &jobs);
                    if let Err(e) = store {
                        after.error = Some(format!("cannot keep GitHub's answer: {e}"));
                    }
                    after
                }
                Err(GhError(why)) => RepoCi {
                    error: Some(why),
                    ..before
                },
            };
            if let Some(slot) = self.ci().repos.get_mut(index) {
                *slot = after;
            }
        }
    }
}

/// One read of `before`'s repo: its runners, the jobs of its active runs and of the runs
/// whose jobs `before` still waited on, and its open factory pull requests' checks. Also
/// every job read, completed ones too, for the times the database keeps.
fn read<A: Actions>(actions: &A, before: &RepoCi) -> Result<(RepoCi, Vec<Job>), GhError> {
    let repo = before.repo.as_str();
    let runners = actions.runners(repo)?;
    let mut runs = actions.active_runs(repo)?;
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
        let state = PullState::of(&actions.checks(repo, &open)?);
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
    Ok((
        RepoCi {
            repo: before.repo.clone(),
            seen: None,
            error: None,
            runners,
            jobs,
            pulls,
        },
        read,
    ))
}

#[cfg(test)]
mod tests {
    use factory_adapters::gh::{FakeActions, OpenPull};
    use factory_core::ci::CheckRun;
    use factory_core::fleet::{Runner, RunnerState};

    use super::*;

    fn job(status: &str) -> Job {
        Job {
            id: 70,
            run_id: 7,
            name: "test".to_owned(),
            status: status.to_owned(),
            conclusion: (status == "completed").then(|| "success".to_owned()),
            runner: Some("ci-1".to_owned()),
            queued: Some(1_000),
            started: Some(2_000),
            finished: (status == "completed").then_some(3_000),
        }
    }

    fn open(dir: &tempfile::TempDir, fake: FakeActions) -> Fleet<FakeActions> {
        let watch = CiWatch {
            repos: vec!["acme/shop".to_owned()],
            expected: 2,
        };
        let store = Store::open(&dir.path().join("factory.db")).expect("open the store");
        Fleet::open(Ok(watch), store, fake)
    }

    fn fake() -> FakeActions {
        let fake = FakeActions::default();
        {
            let mut state = fake.state.lock().expect("the fake");
            state.runners = vec![Runner {
                id: 1,
                name: "ci-1".to_owned(),
                state: RunnerState::Busy,
            }];
            state.runs = vec![(7, vec![job("in_progress")])];
            state.pulls = vec![(
                OpenPull {
                    number: 12,
                    branch: "factory/s1-t2".to_owned(),
                    head: "c0ffee".to_owned(),
                    url: "https://github.com/acme/shop/pull/12".to_owned(),
                },
                vec![CheckRun {
                    id: 1,
                    name: "test".to_owned(),
                    status: "in_progress".to_owned(),
                    conclusion: None,
                    url: String::new(),
                    app: Some("github-actions".to_owned()),
                }],
            )];
        }
        fake
    }

    #[test]
    fn a_failed_read_keeps_the_last_good_answer_marked_with_why() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let fleet = open(&dir, fake());
        fleet.read_all();
        let good = fleet.snapshot().repos[0].clone();
        assert_eq!(good.error, None);
        assert_eq!(good.jobs, vec![job("in_progress")]);

        fleet.actions.state.lock().expect("the fake").failing = Some("gh: HTTP 502".to_owned());
        fleet.read_all();
        let stale = fleet.snapshot().repos[0].clone();
        assert_eq!(stale.error.as_deref(), Some("gh: HTTP 502"));
        assert_eq!(
            RepoCi {
                error: None,
                ..stale
            },
            good
        );
    }

    #[test]
    fn a_restarted_watcher_resumes_from_what_it_kept_and_reads_a_tracked_jobs_end() {
        let dir = tempfile::tempdir().expect("a temp dir");
        open(&dir, fake()).read_all();

        // The run finished while no daemon ran: it is no longer active, but its job was.
        let finished = fake();
        finished.state.lock().expect("the fake").runs = vec![(7, vec![job("completed")])];
        let fleet = open(&dir, finished);
        let kept = fleet.snapshot().repos[0].clone();
        assert!(kept.needs_watching(), "{kept:?}");
        assert_eq!(kept.pulls[0].number, 12);

        fleet.read_all();
        let read = fleet.snapshot().repos[0].clone();
        assert_eq!(read.jobs, Vec::new());
        let reopened = open(&dir, FakeActions::default());
        assert_eq!(
            reopened.snapshot().repos[0].jobs,
            Vec::new(),
            "the end was kept"
        );
    }
}
