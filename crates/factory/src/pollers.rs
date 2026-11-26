//! The daemon's waits on CI: one poller per pull request, shared by every wait on it.
//!
//! A poller first starts `gh event listen` on its pull request and reads the checks each time
//! the event stream attaches and after each event, so it never polls while the stream runs.
//! Where events can't be had (the feature is in preview, capped per account, and refused on
//! accounts without it), or once the listener ends, it polls GitHub every interval instead,
//! with conditional requests: each read sends back the ETag of the last answer, and GitHub
//! answers an unchanged pull request or check list with `304 Not Modified`. A read is the pull
//! request (for its head), then the head's check runs and its commit statuses. While a
//! listener is attached, a poller still reads every two minutes, in case the stream stalled.
//!
//! Waits hold the poller; when the last one ends, the poller stops and its listener is killed.
//!
//! A wait on CI is kept in the database while its CLI waits, like every blocking wait (see
//! [`crate::leads`]): a restarted daemon starts the poller again when the CLI comes back for
//! it. A wait that outlives its CLI (`--notify`) is kept by [`crate::notifier`] instead.

use std::collections::HashMap;
use std::future::pending;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use factory_adapters::gh::{Fetched, Gh, Heard, Listener, PullRequests};
use factory_core::ci::{self, CheckRun, PullRequest, Tally, Verdict};
use factory_core::duration;
use factory_core::exit::Exit;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::output::Failure;
use crate::paths::Paths;
use crate::protocol::WaitCiResult;

/// How many lines of a failed job's log the file keeps.
const TAIL_LINES: usize = 40;
/// How often a poller with an attached listener reads anyway.
const ATTACHED_CHECK: Duration = Duration::from_secs(120);
/// How many reads in a row may fail before the waits on a pull request fail with them.
const READ_FAILURES: u32 = 3;

pub struct Pollers<G: PullRequests = Gh> {
    gh: Arc<G>,
    paths: Paths,
    running: Mutex<HashMap<PullRequest, Weak<Poller>>>,
    /// The pull request a wait last joined a poller on, for the CI watcher it wakes.
    watched: watch::Sender<Option<PullRequest>>,
}

/// One pull request's poller; it stops when the last wait holding it drops it.
struct Poller {
    seen: watch::Receiver<Seen>,
    task: AbortHandle,
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What a poller has seen of its pull request's checks.
#[derive(Clone, Debug)]
enum Seen {
    Nothing,
    Pending { pending: usize },
    Finished(Arc<WaitCiResult>),
    Broken(String),
}

/// A wait's hold on its pull request's poller.
pub struct Watch {
    pr: PullRequest,
    poller: Arc<Poller>,
}

impl<G: PullRequests> Pollers<G> {
    pub fn new(paths: Paths, gh: G) -> Pollers<G> {
        Pollers {
            gh: Arc::new(gh),
            paths,
            running: Mutex::new(HashMap::new()),
            watched: watch::Sender::new(None),
        }
    }

    /// Hears each pull request a wait starts watching.
    pub fn watched(&self) -> watch::Receiver<Option<PullRequest>> {
        self.watched.subscribe()
    }

    /// Finds the pull request `pr` names from `cwd`, and joins its poller, starting one that
    /// polls every `interval` when none runs.
    pub async fn watch(
        &self,
        pr: &str,
        cwd: PathBuf,
        interval: Duration,
    ) -> Result<Watch, Failure> {
        let gh = Arc::clone(&self.gh);
        let named = pr.to_owned();
        let url = blocking(move || gh.url(&named, &cwd))
            .await?
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot find pull request {pr}: {e}; name it by number from its repo's checkout, or by URL"
                ))
            })?;
        let pr = PullRequest::from_url(&url).map_err(|e| {
            Failure::precondition(format!("gh pr view answered {e}; check gh's version"))
        })?;
        self.watched.send_replace(Some(pr.clone()));
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        running.retain(|_, poller| poller.strong_count() > 0);
        if let Some(poller) = running.get(&pr).and_then(Weak::upgrade) {
            return Ok(Watch { pr, poller });
        }
        let (tx, seen) = watch::channel(Seen::Nothing);
        let reader = Reader::new(Arc::clone(&self.gh), self.paths.clone(), pr.clone());
        let task = tokio::spawn(run(reader, interval, tx));
        let poller = Arc::new(Poller {
            seen,
            task: task.abort_handle(),
        });
        running.insert(pr.clone(), Arc::downgrade(&poller));
        Ok(Watch { pr, poller })
    }
}

impl Watch {
    pub fn pr(&self) -> &PullRequest {
        &self.pr
    }

    /// Answers once every check on the head has finished, or fails at `deadline`, `limit`
    /// after the wait began.
    pub async fn finished(
        self,
        deadline: Instant,
        limit: Duration,
    ) -> Result<WaitCiResult, Failure> {
        let mut seen = self.poller.seen.clone();
        loop {
            let now = seen.borrow_and_update().clone();
            match now {
                Seen::Finished(answer) => return Ok((*answer).clone()),
                Seen::Broken(why) => return Err(Failure::precondition(why)),
                Seen::Nothing | Seen::Pending { .. } => {}
            }
            tokio::select! {
                changed = seen.changed() => {
                    if changed.is_err() {
                        return Err(Failure::precondition(format!(
                            "the poller of {} stopped; wait again", self.pr
                        )));
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Err(Failure::new(Exit::Timeout, still(&self.pr, &now, limit)));
                }
            }
        }
    }
}

/// What a wait that ran out of time says.
fn still(pr: &PullRequest, seen: &Seen, limit: Duration) -> String {
    let after = duration::show(limit);
    match seen {
        Seen::Pending { pending: 0 } => {
            format!("{pr} has no checks after {after}; wait again, or see whether CI runs on it")
        }
        Seen::Pending { pending: 1 } => {
            format!("{pr} still has 1 check running after {after}; wait again")
        }
        Seen::Pending { pending } => {
            format!("{pr} still has {pending} checks running after {after}; wait again")
        }
        _ => format!("{pr}'s checks were not read within {after}; wait again"),
    }
}

/// A poller's life: listen when it can, poll when it can't, and publish what each read saw.
async fn run<G: PullRequests>(
    mut reader: Reader<G>,
    interval: Duration,
    seen: watch::Sender<Seen>,
) {
    let pr = reader.pr.clone();
    let mut listener = match reader.gh.listen(&pr) {
        Ok(listener) => Some(listener),
        Err(e) => {
            eprintln!(
                "factory: cannot listen for {pr}'s events: {e}; polling it every {}",
                duration::show(interval)
            );
            None
        }
    };
    let mut attached = false;
    let mut last_read: Option<Instant> = None;
    // A listener gets one interval to attach before the first poll.
    let mut next_poll = Some(if listener.is_some() {
        Instant::now() + interval
    } else {
        Instant::now()
    });
    let mut failures = 0;
    loop {
        let heard = tokio::select! {
            heard = hear(&mut listener) => Some(heard),
            () = sleep_until(next_poll) => None,
        };
        if let Some(first) = heard {
            let mut heard = vec![first];
            if let Some(listener) = &mut listener {
                heard.extend(listener.ready());
            }
            let ended = heard.iter().find_map(|heard| match heard {
                Heard::Ended(why) => Some(why.clone()),
                Heard::Attached | Heard::Changed => None,
            });
            if let Some(why) = ended {
                eprintln!(
                    "factory: {why}; polling {pr} every {}",
                    duration::show(interval)
                );
                listener = None;
                attached = false;
                let now = Instant::now();
                next_poll = Some(last_read.map_or(now, |at| (at + interval).max(now)));
                continue;
            }
            attached |= heard.contains(&Heard::Attached);
        }
        let (back, read) = match tokio::task::spawn_blocking(move || {
            let read = reader.read();
            (reader, read)
        })
        .await
        {
            Ok(done) => done,
            Err(e) => {
                seen.send_replace(Seen::Broken(format!(
                    "reading {pr}'s checks failed in the daemon: {e}; wait again"
                )));
                return;
            }
        };
        reader = back;
        let now = Instant::now();
        last_read = Some(now);
        // An attached listener reads on each event, and now and then besides, in case it
        // stalled without a word.
        next_poll = Some(
            now + if attached {
                interval.max(ATTACHED_CHECK)
            } else {
                interval
            },
        );
        match read {
            Ok(Some(fresh)) => {
                failures = 0;
                seen.send_replace(fresh);
            }
            Ok(None) => failures = 0,
            Err(e) => {
                failures += 1;
                eprintln!("factory: reading {pr}'s checks failed: {e}");
                next_poll = Some(now + interval);
                if failures >= READ_FAILURES {
                    // The next read that works is published, unchanged or not.
                    reader.cache = Cache::default();
                    seen.send_replace(Seen::Broken(format!(
                        "cannot read {pr}'s checks: {e}; check gh auth status, then wait again"
                    )));
                }
            }
        }
    }
}

/// What the listener says next; never, without one. A listener that stops without a word has
/// ended.
async fn hear(listener: &mut Option<Listener>) -> Heard {
    match listener {
        Some(listener) => listener
            .next()
            .await
            .unwrap_or_else(|| Heard::Ended("gh event listen stopped".to_owned())),
        None => pending().await,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => pending().await,
    }
}

/// A response and the ETag it came with, to send back on the next read.
#[derive(Clone)]
struct Cached<T> {
    value: T,
    etag: Option<String>,
}

/// What a read last saw of one pull request: its head, and that head's check runs and commit
/// statuses.
#[derive(Default)]
struct Cache {
    head: Option<Cached<String>>,
    runs: Option<Cached<Vec<CheckRun>>>,
    statuses: Option<Cached<Vec<CheckRun>>>,
}

/// The ETag a cached response came with.
fn etag<T>(cached: Option<&Cached<T>>) -> Option<&str> {
    cached.and_then(|cached| cached.etag.as_deref())
}

/// What a conditional GET leaves cached, and whether it changed: the fresh response, or the
/// one before it.
fn refreshed<T: Clone>(
    fetched: Fetched<T>,
    before: Option<&Cached<T>>,
) -> (Option<Cached<T>>, bool) {
    match fetched {
        Fetched::Fresh { etag, value } => (Some(Cached { value, etag }), true),
        Fetched::NotModified => (before.cloned(), false),
    }
}

/// Reads one pull request's head and checks, remembering each answer's ETag.
struct Reader<G: PullRequests> {
    gh: Arc<G>,
    paths: Paths,
    pr: PullRequest,
    cache: Cache,
}

impl<G: PullRequests> Reader<G> {
    fn new(gh: Arc<G>, paths: Paths, pr: PullRequest) -> Reader<G> {
        Reader {
            gh,
            paths,
            pr,
            cache: Cache::default(),
        }
    }

    /// Reads the head and its checks: what they show now, or none when nothing changed. The
    /// cache only moves on once the whole read, failures' file and all, has worked, so a read
    /// that fails is tried again in full.
    fn read(&mut self) -> Result<Option<Seen>, String> {
        let (cache, seen) = self.read_on()?;
        self.cache = cache;
        Ok(seen)
    }

    /// A read on from the cache: the cache it leaves, and what it saw when anything changed.
    fn read_on(&self) -> Result<(Cache, Option<Seen>), String> {
        let before = &self.cache;
        let fetched = self
            .gh
            .head(&self.pr, etag(before.head.as_ref()))
            .map_err(|e| e.0)?;
        let (head, mut changed) = refreshed(fetched, before.head.as_ref());
        let Some(sha) = head.as_ref().map(|head| head.value.clone()) else {
            return Err("GitHub said the pull request is unchanged before it was read".to_owned());
        };
        // A new head has checks of its own.
        let same_head = before.head.as_ref().is_some_and(|head| head.value == sha);
        let (runs, statuses) = if same_head {
            (before.runs.as_ref(), before.statuses.as_ref())
        } else {
            (None, None)
        };
        let fetched = self
            .gh
            .check_runs(&self.pr, &sha, etag(runs))
            .map_err(|e| e.0)?;
        let (runs, fresh) = refreshed(fetched, runs);
        changed |= fresh;
        let fetched = self
            .gh
            .statuses(&self.pr, &sha, etag(statuses))
            .map_err(|e| e.0)?;
        let (statuses, fresh) = refreshed(fetched, statuses);
        changed |= fresh;
        let cache = Cache {
            head,
            runs,
            statuses,
        };
        if !changed {
            return Ok((cache, None));
        }
        let all: Vec<CheckRun> = [&cache.runs, &cache.statuses]
            .into_iter()
            .flatten()
            .flat_map(|cached| cached.value.iter().cloned())
            .collect();
        let seen = match ci::verdict(&all) {
            Verdict::Pending { pending } => Seen::Pending { pending },
            Verdict::Finished(tally) => Seen::Finished(Arc::new(self.finished(&sha, &tally)?)),
        };
        Ok((cache, Some(seen)))
    }

    /// The answer for a head whose checks all finished, writing the failures' file.
    fn finished(&self, head: &str, tally: &Tally) -> Result<WaitCiResult, String> {
        let file = if tally.failed.is_empty() {
            None
        } else {
            Some(self.write_failures(head, &tally.failed)?)
        };
        Ok(WaitCiResult {
            pr: self.pr.to_string(),
            url: self.pr.url(),
            head: head.to_owned(),
            passed: tally.passed,
            skipped: tally.skipped,
            failed: tally.failed.iter().map(|run| run.name.clone()).collect(),
            file: file.map(|path| path.to_string_lossy().into_owned()),
            exit: tally.exit().code(),
        })
    }

    /// Writes each failed run's URL and log tail to
    /// `<state>/ci/<host>/<owner>/<repo>/<number>-<head>.md`,
    /// readable by its owner only: the logs can be a private repo's.
    fn write_failures(&self, head: &str, failed: &[CheckRun]) -> Result<PathBuf, String> {
        let path = self.paths.ci_failures(&self.pr, head);
        let mut text = format!(
            "# {} at {}: {} failed\n",
            self.pr,
            head.get(..12).unwrap_or(head),
            failed.len()
        );
        for run in failed {
            let conclusion = run.conclusion.as_deref().unwrap_or("unknown");
            text.push_str(&format!(
                "\n## {} ({conclusion})\n\n{}\n\n",
                run.name, run.url
            ));
            match run.app.as_deref() {
                Some(ACTIONS) => match self.gh.job_log(&self.pr, run.id) {
                    Ok(log) => {
                        text.push_str("```text\n");
                        for line in ci::tail(&log, TAIL_LINES) {
                            text.push_str(line);
                            text.push('\n');
                        }
                        text.push_str("```\n");
                    }
                    Err(e) => text.push_str(&format!("No log: {e}\n")),
                },
                Some(app) => text.push_str(&format!("No log here: {app} keeps it at the URL.\n")),
                None => text.push_str("No log here: a commit status keeps it at the URL.\n"),
            }
        }
        private::write(&path, &text).map_err(|e| {
            format!(
                "cannot write {}: {e}; check the disk and FACTORY_STATE",
                path.display()
            )
        })?;
        Ok(path)
    }
}

/// The app whose check runs are GitHub Actions jobs, with logs behind the jobs API.
const ACTIONS: &str = "github-actions";

mod private {
    //! Files only their owner reads.

    use std::fs::{self, DirBuilder, OpenOptions};
    use std::io::{self, Write};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::path::Path;

    /// Replaces `path` with `text`, as a file only its owner can read, in dirs only its owner
    /// can open.
    pub fn write(path: &Path, text: &str) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        // A fresh staging file, so no mode an old one had carries over.
        let tmp = path.with_extension(format!("md.{}.tmp", std::process::id()));
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        drop(file);
        fs::rename(&tmp, path)
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        Failure::precondition(format!("a wait on CI failed in the daemon: {e}; retry"))
    })
}
