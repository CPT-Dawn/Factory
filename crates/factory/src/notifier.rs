//! `factory wait ci|lead --notify`, the daemon's half: waits that outlive their CLI, each kept in
//! the database until it ends, then reported in one line posted into the Claude Code session that
//! registered it ([`factory_adapters::inbox`]).
//!
//! A wait runs as the blocking one does, registered in the wait registry so the lead it comes
//! from shows as waiting, while a pidfd watches the session's process: a session that exits
//! clears its wait at once and is told nothing. One that leaves no pid is found gone when the
//! post finds no listener. A daemon that restarts resumes every wait it kept, against its first
//! deadline.
//!
//! Event delivery is the owner's switch ([`config::notify_enabled`]): with it off, a wait is
//! refused, and one that ends after it was switched off is dropped unsaid.

use std::fs;
use std::future::{Future, pending};
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use factory_adapters::herdr::Herdr;
use factory_adapters::inbox::{Address, Inbox, InboxApi, InboxError};
use factory_core::ci::PullRequest;
use factory_core::exit::Exit;
use factory_core::wait::{Wait, WaitOn};
use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::ci;
use crate::clock;
use crate::config;
use crate::lab::host;
use crate::lead;
use crate::leads::{Leads, Registered};
use crate::output::Failure;
use crate::paths::Paths;
use crate::pollers::Pollers;
use crate::protocol::{NotifyOn, NotifyParams, NotifyResult, SessionParam};
use crate::store::{NotifyRow, Store};

/// How long finding a pull request with `gh` may take when a wait registers or resumes.
const FIND_TIMEOUT: Duration = Duration::from_secs(25);
/// Who the posted lines are from, as the session shows it.
const FROM: &str = "factory";

pub struct Notifier<I: InboxApi = Inbox> {
    paths: Paths,
    store: Mutex<Store>,
    inbox: I,
    leads: Arc<Leads>,
    pollers: Arc<Pollers>,
}

/// How a wait ends: its line and exit, or its failure.
type Ending = Pin<Box<dyn Future<Output = Result<(String, Exit), Failure>> + Send>>;

/// A wait under way: its place in the registry, and the line and exit it ends with.
struct Running {
    _registered: Registered<Herdr>,
    ended: Ending,
}

impl<I: InboxApi> Notifier<I> {
    pub fn new(
        paths: Paths,
        store: Store,
        inbox: I,
        leads: Arc<Leads>,
        pollers: Arc<Pollers>,
    ) -> Arc<Notifier<I>> {
        Arc::new(Notifier {
            paths,
            store: Mutex::new(store),
            inbox,
            leads,
            pollers,
        })
    }

    /// Starts a wait that reports into `params.session` when it ends, and answers at once.
    pub async fn register(self: &Arc<Self>, params: NotifyParams) -> Result<NotifyResult, Failure> {
        let (enabled, config) = config::notify_enabled(&self.paths)?;
        if !enabled {
            return Err(config::notify_off(&config));
        }
        let limit = Duration::from_secs(params.timeout_secs);
        let deadline = Instant::now() + limit;
        let (on, running) = self
            .start(&params.on, params.waiter.clone(), deadline, limit)
            .await?;
        let mut row = NotifyRow {
            id: 0,
            on,
            waiter: params.waiter,
            session: params.session,
            deadline_ms: clock::now()
                .unix_millis()
                .saturating_add(i64::try_from(limit.as_millis()).unwrap_or(i64::MAX)),
            limit_secs: params.timeout_secs,
        };
        row.id = self.lock().add_notify(&row).map_err(|e| {
            Failure::precondition(format!(
                "cannot keep the wait in {}: {e}; check the disk, then retry",
                self.paths.database().display()
            ))
        })?;
        let answer = NotifyResult {
            id: row.id,
            on: shown(&row.on),
            deadline: row.deadline_ms,
        };
        tokio::spawn(Arc::clone(self).follow(row, running));
        Ok(answer)
    }

    /// Resumes every wait a stopped daemon kept, forgetting those whose session is gone. Each
    /// shows as waiting before this returns.
    pub fn resume(self: &Arc<Self>) {
        let rows = match self.lock().notifies() {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!(
                    "factory: cannot read the --notify waits in {}: {e}; they stay there",
                    self.paths.database().display()
                );
                return;
            }
        };
        for row in rows {
            if gone(&row.session) {
                eprintln!("factory: wait #{} dropped: its session is gone", row.id);
                self.forget(row.id);
                continue;
            }
            let early = self.leads.register(Wait {
                waiter: row.waiter.clone(),
                on: wait_on(&row.on),
            });
            tokio::spawn(Arc::clone(self).resumed(row, early));
        }
    }

    async fn resumed(self: Arc<Self>, row: NotifyRow, early: Registered<Herdr>) {
        let left = row.deadline_ms.saturating_sub(clock::now().unix_millis());
        let deadline = Instant::now() + Duration::from_millis(u64::try_from(left).unwrap_or(0));
        let limit = Duration::from_secs(row.limit_secs);
        let started = self
            .start(&row.on, row.waiter.clone(), deadline, limit)
            .await;
        drop(early);
        match started {
            Ok((_, running)) => self.follow(row, running).await,
            Err(failure) => {
                self.deliver(&row, Err(failure)).await;
                self.forget(row.id);
            }
        }
    }

    /// Finds what `on` names and starts waiting on it: the pull request's URL, or the lead, and
    /// the wait under way.
    async fn start(
        &self,
        on: &NotifyOn,
        waiter: Option<String>,
        deadline: Instant,
        limit: Duration,
    ) -> Result<(NotifyOn, Running), Failure> {
        match on {
            NotifyOn::Ci { pr, cwd, poll_secs } => {
                let poll = Duration::from_secs((*poll_secs).max(1));
                let found = self.pollers.watch(pr, PathBuf::from(cwd), poll);
                let watch = tokio::time::timeout(FIND_TIMEOUT, found)
                    .await
                    .map_err(|_| {
                        Failure::precondition(format!(
                            "gh found no pull request {pr} within {}s; retry",
                            FIND_TIMEOUT.as_secs()
                        ))
                    })??;
                let found = watch.pr().clone();
                let registered = self.leads.register(Wait {
                    waiter,
                    on: WaitOn::Ci(found.to_string()),
                });
                let ended = async move {
                    let answer = watch.finished(deadline, limit).await?;
                    let exit = Exit::from_code(answer.exit).unwrap_or(Exit::NeedsAttention);
                    Ok((ci::line(&answer), exit))
                };
                let on = NotifyOn::Ci {
                    pr: found.url(),
                    cwd: cwd.clone(),
                    poll_secs: *poll_secs,
                };
                Ok((on, running(registered, ended)))
            }
            NotifyOn::Lead { lead: name } => {
                self.leads.find(name).await?;
                let registered = self.leads.register(Wait {
                    waiter,
                    on: WaitOn::Lead(name.clone()),
                });
                let leads = Arc::clone(&self.leads);
                let named = name.clone();
                let ended = async move {
                    let answer = leads.settled(&named, deadline, limit).await?;
                    let exit = Exit::from_code(answer.exit).unwrap_or(Exit::NeedsAttention);
                    Ok((lead::line(&answer.lead), exit))
                };
                Ok((on.clone(), running(registered, ended)))
            }
        }
    }

    /// Waits until the wait ends, then posts its line; drops it unsaid if its session exits first.
    async fn follow(self: Arc<Self>, row: NotifyRow, running: Running) {
        let Running {
            _registered: registered,
            ended,
        } = running;
        let ended = tokio::select! {
            () = exited(&row.session) => None,
            ended = ended => Some(ended),
        };
        // Cleared before the post: the session that hears of it is no longer waiting.
        drop(registered);
        match ended {
            Some(ended) => self.deliver(&row, ended).await,
            None => eprintln!("factory: wait #{} dropped: its session exited", row.id),
        }
        self.forget(row.id);
    }

    /// Posts a wait's line into its session, unless delivery was switched off meanwhile.
    async fn deliver(&self, row: &NotifyRow, ended: Result<(String, Exit), Failure>) {
        let line = ended_line(row.id, ended);
        match config::notify_enabled(&self.paths) {
            Ok((true, _)) => {}
            Ok((false, config)) => {
                eprintln!(
                    "factory: wait #{} dropped unsaid: event delivery is off in {}",
                    row.id,
                    config.display()
                );
                return;
            }
            Err(failure) => {
                eprintln!(
                    "factory: wait #{} dropped unsaid: {}",
                    row.id, failure.message
                );
                return;
            }
        }
        let to = Address {
            socket: PathBuf::from(&row.session.socket),
            token: row.session.token.clone(),
        };
        match self.inbox.post(&to, FROM, &line).await {
            Ok(()) => {}
            Err(InboxError::Gone(_)) => {
                eprintln!("factory: wait #{} dropped: its session is gone", row.id);
            }
            Err(e) => eprintln!(
                "factory: wait #{} ended, but posting into {} failed: {e}",
                row.id,
                to.socket.display()
            ),
        }
    }

    fn forget(&self, id: i64) {
        if let Err(e) = self.lock().remove_notify(id) {
            eprintln!(
                "factory: cannot forget wait #{id} in {}: {e}; a restart resumes it",
                self.paths.database().display()
            );
        }
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn running(
    registered: Registered<Herdr>,
    ended: impl Future<Output = Result<(String, Exit), Failure>> + Send + 'static,
) -> Running {
    Running {
        _registered: registered,
        ended: Box::pin(ended),
    }
}

/// The line a session is told: `factory wait #<id> ended (exit <n>): <the wait's line>`.
fn ended_line(id: i64, ended: Result<(String, Exit), Failure>) -> String {
    let (line, exit) = match ended {
        Ok((line, exit)) => (line, exit),
        Err(failure) => (failure.message, failure.exit),
    };
    format!("factory wait #{id} ended (exit {}): {line}", exit.code())
}

/// What a wait waits on, as the registry holds it.
fn wait_on(on: &NotifyOn) -> WaitOn {
    match on {
        NotifyOn::Ci { pr, .. } => {
            WaitOn::Ci(PullRequest::from_url(pr).map_or_else(|_| pr.clone(), |pr| pr.to_string()))
        }
        NotifyOn::Lead { lead } => WaitOn::Lead(lead.clone()),
    }
}

/// What a wait waits on, as `factory lead status` shows it.
fn shown(on: &NotifyOn) -> String {
    wait_on(on).to_string()
}

/// Whether the session is gone already: its process is, or its socket is.
fn gone(session: &SessionParam) -> bool {
    if let (Some(pid), Some(start)) = (session.pid, &session.start)
        && !host::alive(pid, start)
    {
        return true;
    }
    matches!(fs::symlink_metadata(Path::new(&session.socket)), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Resolves once the session's process exits; never, for a session that gave no pid.
async fn exited(session: &SessionParam) {
    let (Some(pid), Some(start)) = (session.pid, session.start.as_deref()) else {
        return pending().await;
    };
    let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return pending().await;
    };
    let pidfd = match pidfd_open(raw, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(Errno::SRCH) => return,
        Err(_) => return pending().await,
    };
    // Checked once the pidfd holds the pid: a pid reused since the session left is not it.
    if !host::alive(pid, start) {
        return;
    }
    let Ok(watch) = ExitWatch::start(pidfd) else {
        return pending().await;
    };
    watch.exited().await;
}

/// A thread blocked in `poll` on a pidfd, which turns readable when its process exits, and on
/// an eventfd that wakes it to stop once nobody waits for the exit any more.
struct ExitWatch {
    exit: oneshot::Receiver<()>,
    stop: OwnedFd,
}

impl ExitWatch {
    fn start(pidfd: OwnedFd) -> io::Result<ExitWatch> {
        let stop = eventfd(0, EventfdFlags::CLOEXEC)?;
        let stopped = stop.try_clone()?;
        let (tx, exit) = oneshot::channel();
        thread::Builder::new()
            .name("factory-session-exit".to_owned())
            .spawn(move || {
                let mut fds = [
                    PollFd::new(&pidfd, PollFlags::IN),
                    PollFd::new(&stopped, PollFlags::IN),
                ];
                loop {
                    match poll(&mut fds, None) {
                        Ok(_) => break,
                        Err(Errno::INTR) => {}
                        Err(_) => return,
                    }
                }
                if !fds[0].revents().is_empty() {
                    let _ = tx.send(());
                }
            })?;
        Ok(ExitWatch { exit, stop })
    }

    /// Resolves once the process exits; never, if the watch broke.
    async fn exited(mut self) {
        if (&mut self.exit).await.is_err() {
            pending::<()>().await;
        }
    }
}

impl Drop for ExitWatch {
    fn drop(&mut self) {
        let _ = rustix::io::write(&self.stop, &1u64.to_ne_bytes());
    }
}
