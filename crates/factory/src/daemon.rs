//! `factory daemon`: the one long-running process, and the ledger's only writer.
//!
//! It takes the daemon lock (so one runs), listens on the socket systemd passes it (socket
//! activation, `LISTEN_FDS`) or else binds the socket itself, writes its pid, prints one ready
//! line once it can answer, then answers requests until it is stopped.
//!
//! On SIGTERM (systemd stopping or restarting it) or SIGINT it stops accepting, answers every
//! request it has taken, cuts short the waits it holds, and exits 0. Calls that arrive meanwhile
//! wait in systemd's socket for the next daemon, so a restart loses no ledger line.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::net::UnixListener as StdListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use factory_adapters::gh::Gh;
use factory_core::duration;
use factory_core::event::Event;
use factory_core::exit::Exit;
use factory_core::timestamp::Timestamp;
use factory_core::wait::WaitOn;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinSet;

use factory_adapters::herdr::Herdr;
use factory_adapters::inbox::Inbox;
use listenfd::ListenFd;

use crate::clock;
use crate::fleet::Fleet;
use crate::lab::service::{self, Lab};
use crate::lab::wire::{ACQUIRE, RESERVE, RUN_RESUME};
use crate::leads::{Leads, Registered};
use crate::ledger::Ledger;
use crate::notifier::Notifier;
use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::pollers::Pollers;
use crate::protocol::{
    BAR, BarParams, CLAUDE_LIMIT, CLAUDE_QUOTA, CLAUDE_WINDOW, ClaudeLimitParams,
    ClaudeQuotaParams, ClaudeWindowParams, ErrorBody, LEAD_LIST, LEAD_SPAWN, LEAD_STATUS, LOG,
    LeadParams, LogParams, LogResult, MISMATCH, MONITOR_START, MONITOR_STATUS, MONITOR_STOP,
    NotifyParams, Outcome, REVIEW_ACQUIRE, REVIEW_QUOTA, Request, Response, ReviewRef, STATUS,
    SpawnParams, StatusParams, WAIT_CI, WAIT_CLAUDE, WAIT_LEAD, WAIT_LEASE, WAIT_NOTIFY,
    WAIT_REVIEW, WaitCiParams, WaitCiResult, WaitClaudeParams, WaitLeadParams, WaitLeaseParams,
    WaitReviewParams,
};
use crate::quotas::QuotaKeeper;
use crate::reviews::Reviews;
use crate::router::{Held as Slot, Router};
use crate::status;
use crate::steward::Steward;
use crate::store::{Store, WaitRow};

/// What every connection answers from: the ledger, the picture of the leads, the reviewer
/// router, the lab's lease store, the CI pollers, the waits that outlive their CLI and the quota
/// keeper.
struct Daemon {
    ledger: Arc<Ledger>,
    leads: Arc<Leads>,
    router: Arc<Router>,
    reviews: Arc<Reviews>,
    lab: Arc<Lab>,
    pollers: Arc<Pollers>,
    steward: Arc<Steward>,
    notifier: Arc<Notifier>,
    quotas: Arc<QuotaKeeper>,
    fleet: Arc<Fleet>,
}

/// How long a stopping daemon gives the connections it holds to finish their requests.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs the daemon until it is stopped; the ready line is all it prints.
pub fn run(json: bool) -> Result<(), Failure> {
    // First, while the daemon is one thread: taking the socket clears LISTEN_FDS.
    let activated = ListenFd::from_env().take_unix_listener(0).map_err(|e| {
        Failure::precondition(format!(
            "the socket systemd passed is not a unix socket: {e}; check factory.socket"
        ))
    })?;
    let paths = Paths::from_env()?;
    paths.create_runtime()?;
    fs::create_dir_all(&paths.state).map_err(|e| {
        Failure::precondition(format!(
            "cannot create the state dir {}: {e}; check FACTORY_STATE and XDG_STATE_HOME",
            paths.state.display()
        ))
    })?;
    let ledger = Arc::new(Ledger::open(paths.ledger()));
    let _lock = take_daemon_lock(&paths)?;
    let leads = Leads::new(
        paths.clone(),
        Herdr::new(paths.herdr.clone()),
        Arc::clone(&ledger),
        Store::open(&paths.database())?,
    )?;
    let pollers = Arc::new(Pollers::new(paths.clone(), Gh));
    let router = Arc::new(Router::open(Store::open(&paths.database())?)?);
    let quotas = Arc::new(QuotaKeeper::open(Store::open(&paths.database())?)?);
    let fleet = Arc::new(Fleet::open(
        crate::config::ci_watch(&paths),
        Store::open(&paths.database())?,
        Gh,
    ));
    let steward = Steward::new(
        paths.clone(),
        Arc::clone(&leads),
        Store::open(&paths.database())?,
    );
    let notifier = Notifier::new(
        paths.clone(),
        Store::open(&paths.database())?,
        Inbox,
        Arc::clone(&leads),
        Arc::clone(&pollers),
    );
    // Opened under the lock: it forgets the waiters a dead daemon left.
    let lab = Lab::open(&paths.lab_db(), Arc::clone(&ledger)).map_err(|e| {
        Failure::precondition(format!("{e}; check FACTORY_STATE and XDG_STATE_HOME"))
    })?;
    let listener = match activated {
        Some(listener) => listener,
        None => bind(&paths.socket())?,
    };
    let live = bind(&paths.live_socket())?;
    for (listener, socket) in [(&listener, paths.socket()), (&live, paths.live_socket())] {
        listener.set_nonblocking(true).map_err(|e| {
            Failure::precondition(format!("cannot listen on {}: {e}; retry", socket.display()))
        })?;
    }
    let pid = std::process::id();
    fs::write(paths.daemon_pid(), format!("{pid}\n")).map_err(|e| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check XDG_RUNTIME_DIR",
            paths.daemon_pid().display()
        ))
    })?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Failure::precondition(format!("cannot start the daemon's runtime: {e}; retry"))
        })?;
    let socket = paths.socket();
    runtime.block_on(async {
        let listen = |listener, socket: PathBuf| {
            UnixListener::from_std(listener).map_err(|e| {
                Failure::precondition(format!("cannot listen on {}: {e}; retry", socket.display()))
            })
        };
        let listener = listen(listener, socket.clone())?;
        let live = listen(live, paths.live_socket())?;
        // Before the ready line, so a stop that follows it drains, and a failure is the one line.
        let stop = stop_on_signal()?;
        // Ready only once requests can be answered, so this is the one line the daemon prints.
        output::print(
            &Done::new(
                format!("factory daemon ready on {}", socket.display()),
                json!({"socket": socket.to_string_lossy(), "pid": pid}),
            ),
            json,
        );
        tokio::spawn(Arc::clone(&leads).start());
        tokio::spawn(Arc::clone(&steward).resume());
        tokio::spawn(Arc::clone(&fleet).run(pollers.watched()));
        let lab = Arc::new(lab);
        lab.recover();
        notifier.resume();
        lab.keep();
        lab.watch_phones();
        let daemon = Daemon {
            ledger,
            leads,
            router,
            reviews: Arc::new(Reviews::default()),
            lab,
            pollers,
            steward,
            notifier,
            quotas,
            fleet,
        };
        serve([listener, live], Arc::new(daemon), stop).await;
        Ok(())
    })?;
    // Nothing listens on it any more: a call there is refused, as with no daemon.
    let _ = fs::remove_file(paths.live_socket());
    // Nothing is left to answer; the herdr watcher and any stray task may go.
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

/// A switch that flips once SIGTERM or SIGINT arrives.
fn stop_on_signal() -> Result<Stop, Failure> {
    let cannot =
        |e: io::Error| Failure::precondition(format!("cannot watch for SIGTERM: {e}; retry"));
    let mut term = signal(SignalKind::terminate()).map_err(cannot)?;
    let mut int = signal(SignalKind::interrupt()).map_err(cannot)?;
    let (flip, stop) = watch::channel(false);
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = flip.send(true);
        // Keep the sender, so the switch never reads as dropped.
        std::future::pending::<()>().await;
    });
    Ok(Stop(stop))
}

/// Whether the daemon is stopping.
#[derive(Clone)]
struct Stop(watch::Receiver<bool>);

impl Stop {
    fn is_stopping(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the daemon is stopping.
    async fn stopping(mut self) {
        if self.0.wait_for(|&stopping| stopping).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn take_daemon_lock(paths: &Paths) -> Result<File, Failure> {
    let path = paths.daemon_lock();
    let cannot = |e: &dyn std::fmt::Display| {
        Failure::precondition(format!(
            "cannot lock {}: {e}; check XDG_RUNTIME_DIR",
            path.display()
        ))
    };
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| cannot(&e))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(Failure::precondition(format!(
            "another daemon is running (it holds {}); stop it before starting one",
            path.display()
        ))),
        Err(fs::TryLockError::Error(e)) => Err(cannot(&e)),
    }
}

/// Binds `socket`, replacing one a dead daemon left behind (the daemon lock says none runs).
fn bind(socket: &Path) -> Result<StdListener, Failure> {
    let cannot = |e: io::Error| {
        Failure::precondition(format!(
            "cannot listen on {}: {e}; check XDG_RUNTIME_DIR (a socket path must be under 108 bytes)",
            socket.display()
        ))
    };
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(cannot(e)),
    }
    StdListener::bind(socket).map_err(cannot)
}

/// Answers connections on both sockets, systemd's or its own and the live one, taking each in a
/// fair turn, until the daemon is stopping, then lets the ones it took finish, for up to
/// [`DRAIN_TIMEOUT`]. Connections it never took on systemd's socket stay queued there for the
/// next daemon; those on a socket of its own are refused once it is gone.
async fn serve(listeners: [UnixListener; 2], daemon: Arc<Daemon>, stop: Stop) {
    let [listener, live] = listeners;
    // The connections hear of the stop only once the listener is gone, so a call made after
    // a cut-short answer queues for the next daemon instead of in this one's closing backlog.
    let (hang_up, hung_up) = watch::channel(false);
    let hung_up = Stop(hung_up);
    let mut connections = JoinSet::new();
    loop {
        // Unbiased: when both sockets have callers, each is as likely to go first.
        let accept = async {
            tokio::select! {
                accepted = listener.accept() => accepted,
                accepted = live.accept() => accepted,
            }
        };
        tokio::select! {
            biased;
            () = stop.clone().stopping() => break,
            accepted = accept => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(answer(stream, Arc::clone(&daemon), hung_up.clone()));
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "factory: accept failed: {e}");
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    drop(live);
    // Before the connections hear of it: the waits they hold stay for the next daemon.
    daemon.leads.stop();
    let _ = hang_up.send(true);
    let drained = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(DRAIN_TIMEOUT, drained).await.is_err() {
        let _ = writeln!(
            io::stderr(),
            "factory: stopped with {} connections unanswered after {}s",
            connections.len(),
            DRAIN_TIMEOUT.as_secs()
        );
    }
}

/// Answers each request line on one connection until the client hangs up, then frees the
/// review slot the connection holds. A client sends one request at a time; one that hangs up,
/// or sends more, before its answer abandons the request, so a wait whose CLI is gone stops
/// waiting. A `lab.acquire` holds the connection while it waits, writing notes before its
/// answer. A stopping daemon still answers the first request of a connection it took, then
/// hangs up.
async fn answer(stream: UnixStream, daemon: Arc<Daemon>, stop: Stop) {
    let mut held = None;
    converse(stream, &daemon, &stop, &mut held).await;
    daemon.router.release(&mut held);
}

/// The requests and answers of one connection; `held` names the review slot it holds.
async fn converse(stream: UnixStream, daemon: &Daemon, stop: &Stop, held: &mut Option<Slot>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut first = true;
    loop {
        let next = if first {
            lines.next_line().await
        } else {
            tokio::select! {
                next = lines.next_line() => next,
                () = stop.clone().stopping() => return,
            }
        };
        let Ok(Some(line)) = next else {
            return;
        };
        first = false;
        let request = serde_json::from_str::<Request>(&line);
        match request {
            Ok(request) if request.method == RESERVE => {
                service::reserve(Arc::clone(&daemon.lab), request, &mut writer, &mut lines).await;
                continue;
            }
            Ok(request) if WAITS.contains(&request.method.as_str()) => {
                if hold_wait(request, daemon, stop, &mut writer, &mut lines).await {
                    continue;
                }
                return;
            }
            Ok(request) if request.method == ACQUIRE => {
                // Not cut short by a stop: a lease granted as it was dropped would go unanswered.
                service::acquire(
                    Arc::clone(&daemon.lab),
                    request,
                    &mut writer,
                    &mut lines,
                    stop.clone().stopping(),
                )
                .await;
                continue;
            }
            Ok(request) if request.method == RUN_RESUME => {
                service::resume_run(
                    Arc::clone(&daemon.lab),
                    request,
                    &mut writer,
                    &mut lines,
                    stop.clone().stopping(),
                )
                .await;
                continue;
            }
            _ => {}
        }
        let response = tokio::select! {
            response = respond(request, daemon, stop, held) => response,
            _ = lines.next_line() => return,
        };
        let Ok(mut out) = serde_json::to_string(&response) else {
            return;
        };
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn respond(
    request: serde_json::Result<Request>,
    daemon: &Daemon,
    stop: &Stop,
    held: &mut Option<Slot>,
) -> Response {
    let request = match request {
        Ok(request) => request,
        Err(e) => {
            return Response {
                id: None,
                outcome: Outcome::Error(refusal(
                    Exit::Usage,
                    format!("unreadable request: {e}; send one JSON request per line"),
                )),
            };
        }
    };
    let outcome = match request.method.as_str() {
        LEAD_LIST => answered(daemon.leads.list().await),
        LEAD_STATUS => match params::<LeadParams>(LEAD_STATUS, request.params) {
            Ok(params) => answered(daemon.leads.status(&params.lead).await),
            Err(refused) => refused,
        },
        LEAD_SPAWN => match params::<SpawnParams>(LEAD_SPAWN, request.params) {
            // Its own task, so a CLI that hangs up mid-spawn doesn't leave a worktree without
            // its lead: the spawn runs to its end either way.
            Ok(params) => {
                let leads = Arc::clone(&daemon.leads);
                match tokio::spawn(async move { leads.spawn(params).await }).await {
                    Ok(result) => answered(result),
                    Err(e) => Outcome::Error(refusal(
                        Exit::Precondition,
                        format!("spawning a lead failed in the daemon: {e}; see factory lead list"),
                    )),
                }
            }
            Err(refused) => refused,
        },
        CLAUDE_WINDOW => match params::<ClaudeWindowParams>(CLAUDE_WINDOW, request.params) {
            Ok(params) => answered(daemon.quotas.see_claude(&params)),
            Err(refused) => refused,
        },
        CLAUDE_LIMIT => match params::<ClaudeLimitParams>(CLAUDE_LIMIT, request.params) {
            Ok(params) => answered(daemon.quotas.claude_limit(&params)),
            Err(refused) => refused,
        },
        CLAUDE_QUOTA => match params::<ClaudeQuotaParams>(CLAUDE_QUOTA, request.params) {
            Ok(params) => answered(Ok::<_, Failure>(
                daemon.quotas.claude_verdict(params.hold_at),
            )),
            Err(refused) => refused,
        },
        WAIT_NOTIFY => match params::<NotifyParams>(WAIT_NOTIFY, request.params) {
            // Biased, as a blocking wait: a stopping daemon registers no wait.
            Ok(params) => tokio::select! {
                biased;
                () = stop.clone().stopping() => stopped("wait"),
                registered = daemon.notifier.register(params) => answered(registered),
            },
            Err(refused) => refused,
        },
        MONITOR_START => answered(daemon.steward.start().await),
        MONITOR_STOP => answered(daemon.steward.stop().await),
        MONITOR_STATUS => answered(Ok::<_, Failure>(daemon.steward.status())),
        STATUS => match params::<StatusParams>(STATUS, request.params) {
            Ok(params) => answered(
                status::gather(
                    &params,
                    &daemon.router,
                    &daemon.lab,
                    &daemon.leads,
                    &daemon.steward,
                    &daemon.quotas,
                    &daemon.fleet,
                )
                .await,
            ),
            Err(refused) => refused,
        },
        BAR => match params::<BarParams>(BAR, request.params) {
            Ok(params) => answered(Ok::<_, Failure>(crate::bar::snapshot(
                &params,
                &daemon.leads,
                &daemon.steward,
                &daemon.quotas,
                &daemon.fleet,
            ))),
            Err(refused) => refused,
        },
        LOG => {
            let ledger = Arc::clone(&daemon.ledger);
            let params = request.params;
            let is_review = params["type"] == "review";
            match tokio::task::spawn_blocking(move || log(params, &ledger)).await {
                Ok(outcome) => {
                    if is_review {
                        daemon.reviews.landed();
                    }
                    outcome
                }
                Err(e) => Outcome::Error(refusal(
                    Exit::Precondition,
                    format!("log failed in the daemon: {e}; retry"),
                )),
            }
        }
        REVIEW_ACQUIRE => {
            // Under way from the moment it asks for a slot, queued or not.
            if let Some(review) = request
                .params
                .get("review")
                .and_then(|review| serde_json::from_value::<ReviewRef>(review.clone()).ok())
            {
                daemon.reviews.started(review);
            }
            let granted = tokio::select! {
                biased;
                () = stop.clone().stopping() => None,
                granted = daemon.router.acquire(request.params, held) => Some(granted),
            };
            match granted {
                // Checked again after the grant: a connection the stop hung up may have freed
                // the slot while this acquire ran on another thread.
                Some(granted) if !stop.is_stopping() => granted,
                // A stopping daemon grants no slot; one it just granted goes back.
                _ => {
                    daemon.router.release(held);
                    stopped("review")
                }
            }
        }
        REVIEW_QUOTA => {
            let router = Arc::clone(&daemon.router);
            let params = request.params;
            match tokio::task::spawn_blocking(move || router.record(params)).await {
                Ok(outcome) => outcome,
                Err(e) => Outcome::Error(refusal(
                    Exit::Precondition,
                    format!("recording a quota failed in the daemon: {e}; retry"),
                )),
            }
        }
        other => match service::respond(&daemon.lab, other, request.params).await {
            Some(outcome) => outcome,
            None => Outcome::Error(refusal(
                Exit::Usage,
                format!("unknown method {other:?}; {MISMATCH}"),
            )),
        },
    };
    Response {
        id: Some(request.id),
        outcome,
    }
}

/// The blocking waits: each is kept in the database while the daemon holds it, so its CLI can
/// carry on with it from the next daemon.
const WAITS: [&str; 5] = [WAIT_LEAD, WAIT_CI, WAIT_REVIEW, WAIT_LEASE, WAIT_CLAUDE];

/// A blocking wait's params.
enum Asked {
    Lead(WaitLeadParams),
    Ci(WaitCiParams),
    Review(WaitReviewParams),
    Lease(WaitLeaseParams),
    Claude(WaitClaudeParams),
}

impl Asked {
    fn read(method: &str, value: Value) -> Result<Asked, Outcome> {
        Ok(match method {
            WAIT_LEAD => Asked::Lead(params(method, value)?),
            WAIT_CI => Asked::Ci(params(method, value)?),
            WAIT_REVIEW => Asked::Review(params(method, value)?),
            WAIT_LEASE => Asked::Lease(params(method, value)?),
            _ => Asked::Claude(params(method, value)?),
        })
    }

    /// Who waits, on what, for how many seconds, and the kept wait it carries on with, if any.
    fn wait(&self) -> (Option<String>, WaitOn, u64, Option<i64>) {
        match self {
            Asked::Lead(p) => (
                p.waiter.clone(),
                WaitOn::Lead(p.lead.clone()),
                p.timeout_secs,
                p.resume,
            ),
            Asked::Ci(p) => (
                p.waiter.clone(),
                WaitOn::Ci(p.pr.clone()),
                p.timeout_secs,
                p.resume,
            ),
            Asked::Review(p) => (
                p.waiter.clone(),
                WaitOn::Review(p.label.clone()),
                p.timeout_secs,
                p.resume,
            ),
            Asked::Lease(p) => (
                p.waiter.clone(),
                WaitOn::Lease(p.lease),
                p.timeout_secs,
                p.resume,
            ),
            Asked::Claude(p) => (p.waiter.clone(), WaitOn::Claude, p.timeout_secs, p.resume),
        }
    }
}

/// A blocking wait the daemon holds: until when, how long it was given, for a review when the
/// run it waits on began, and for a lead its pane and report when the wait began.
struct Held {
    registered: Registered<Herdr>,
    deadline: tokio::time::Instant,
    limit: Duration,
    since: Option<Timestamp>,
    lead: (Option<String>, Option<String>),
}

/// Holds a blocking wait: keeps it (or picks up the one an earlier daemon kept, by its
/// `resume` id), says its id in a `queued` line, and answers once it ends. A stop leaves it
/// unanswered and kept, for its CLI to carry on with from the next daemon; a CLI that hangs up
/// abandons it. False once the conversation is over.
async fn hold_wait<W, R>(
    request: Request,
    daemon: &Daemon,
    stop: &Stop,
    writer: &mut W,
    lines: &mut Lines<R>,
) -> bool
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    // A stopping daemon takes no wait: its CLI asks the next one.
    if stop.is_stopping() {
        return false;
    }
    let id = Some(request.id);
    let asked = match Asked::read(&request.method, request.params) {
        Ok(asked) => asked,
        Err(outcome) => return send(writer, &Response { id, outcome }).await,
    };
    // A wait an earlier daemon held answers even for a lead gone since; a new one needs it.
    if let Asked::Lead(p) = &asked
        && p.resume.is_none()
        && let Err(failure) = daemon.leads.find(&p.lead).await
    {
        let outcome = answered::<()>(Err(failure));
        return send(writer, &Response { id, outcome }).await;
    }
    let held = match hold(daemon, &asked) {
        Ok(held) => held,
        Err(outcome) => return send(writer, &Response { id, outcome }).await,
    };
    let Held {
        mut registered,
        deadline,
        limit,
        since,
        lead: (pane, report),
    } = held;
    if let Some(row) = registered.row()
        && !send(
            writer,
            &Response {
                id,
                outcome: Outcome::Queued(row),
            },
        )
        .await
    {
        return false;
    }
    let waited = async {
        match &asked {
            Asked::Lead(p) => answered(match p.resume {
                // The link that found the lead is the one a new wait watches: losing it after
                // the wait is held fails the wait, as losing it later does, where checking the
                // link again here would bring it back up and wait on.
                None => daemon.leads.settled(&p.lead, deadline, limit).await,
                // A resumed wait's daemon is new: its link may not be up yet.
                Some(_) => {
                    daemon
                        .leads
                        .resettled(&p.lead, pane, report, deadline, limit)
                        .await
                }
            }),
            Asked::Ci(p) => answered(wait_ci(daemon, p, &registered, deadline, limit).await),
            Asked::Review(p) => answered(
                daemon
                    .reviews
                    .wait(&daemon.ledger, p, since, deadline, limit)
                    .await,
            ),
            Asked::Lease(p) => answered(service::wait_lease(&daemon.lab, p, deadline, limit).await),
            Asked::Claude(p) => answered(daemon.quotas.wait_claude(p, deadline, limit).await),
        }
    };
    // Biased: a stopping daemon answers no wait, even one that ends as it stops; the next
    // daemon answers it instead.
    let outcome = tokio::select! {
        biased;
        () = stop.clone().stopping() => return false,
        outcome = waited => outcome,
        _ = lines.next_line() => return false,
    };
    registered.answered();
    send(writer, &Response { id, outcome }).await
}

/// Keeps the wait `asked` names, or picks up the one it resumes; the refusal when it can't.
fn hold(daemon: &Daemon, asked: &Asked) -> Result<Held, Outcome> {
    let (waiter, on, timeout_secs, resume) = asked.wait();
    if let Some(row) = resume {
        let Some((registered, kept)) = daemon.leads.resume(row) else {
            return Err(Outcome::Error(refusal(
                Exit::Precondition,
                format!("the daemon restarted and no longer holds wait {row}; run the wait again"),
            )));
        };
        let left = kept
            .deadline_ms
            .saturating_sub(clock::now().unix_millis())
            .max(0);
        return Ok(Held {
            registered,
            deadline: tokio::time::Instant::now()
                + Duration::from_millis(u64::try_from(left).unwrap_or(0)),
            limit: Duration::from_secs(kept.limit_secs),
            since: kept.since,
            lead: (kept.lead_pane, kept.lead_report),
        });
    }
    let limit = Duration::from_secs(timeout_secs);
    // Both clocks from one moment, so a resumed wait keeps the deadline it was given.
    let deadline = tokio::time::Instant::now() + limit;
    let deadline_ms = clock::now()
        .unix_millis()
        .saturating_add(i64::try_from(limit.as_millis()).unwrap_or(i64::MAX));
    let since = match asked {
        Asked::Review(p) => daemon.reviews.began(p.repo.as_deref(), &p.label),
        _ => None,
    };
    let (lead_pane, lead_report) = match asked {
        Asked::Lead(p) => daemon
            .leads
            .whereabouts(&p.lead)
            .map_or((None, None), |(pane, report)| (Some(pane), report)),
        _ => (None, None),
    };
    let registered = daemon
        .leads
        .hold(&WaitRow {
            id: 0,
            on,
            waiter,
            deadline_ms,
            limit_secs: timeout_secs,
            since,
            lead_pane: lead_pane.clone(),
            lead_report: lead_report.clone(),
        })
        .map_err(|failure| answered::<()>(Err(failure)))?;
    Ok(Held {
        registered,
        deadline,
        limit,
        since,
        lead: (lead_pane, lead_report),
    })
}

/// Writes one response line; false when the CLI is gone.
async fn send<W: AsyncWrite + Unpin>(writer: &mut W, response: &Response) -> bool {
    let Ok(mut out) = serde_json::to_string(response) else {
        return false;
    };
    out.push('\n');
    writer.write_all(out.as_bytes()).await.is_ok()
}

/// Joins the pull request's poller, then shows the waiter's lead as waiting on the pull request
/// it found until the checks finish.
async fn wait_ci(
    daemon: &Daemon,
    params: &WaitCiParams,
    registered: &Registered<Herdr>,
    deadline: tokio::time::Instant,
    limit: Duration,
) -> Result<WaitCiResult, Failure> {
    let poll = Duration::from_secs(params.poll_secs.max(1));
    let found = daemon
        .pollers
        .watch(&params.pr, PathBuf::from(&params.cwd), poll);
    // The timeout counts from the request, finding the pull request included.
    let watch = tokio::time::timeout_at(deadline, found)
        .await
        .map_err(|_| {
            Failure::new(
                Exit::Timeout,
                format!(
                    "gh found no pull request {} within {}; wait again",
                    params.pr,
                    duration::show(limit)
                ),
            )
        })??;
    registered.retarget(WaitOn::Ci(watch.pr().to_string()));
    watch.finished(deadline, limit).await
}

fn log(params: Value, ledger: &Ledger) -> Outcome {
    let params: LogParams = match self::params(LOG, params) {
        Ok(params) => params,
        Err(refused) => return refused,
    };
    let event = match Event::from_fields(&params.kind, params.fields) {
        Ok(event) => event,
        Err(e) => return Outcome::Error(refusal(Exit::Usage, e.to_string())),
    };
    match ledger.append(&event) {
        Ok(landed) => {
            let result = LogResult {
                ledger: ledger.path.to_string_lossy().into_owned(),
                event: landed,
            };
            Outcome::Result(serde_json::to_value(result).unwrap_or(Value::Null))
        }
        Err(e) => Outcome::Error(refusal(
            Exit::Precondition,
            format!(
                "cannot append to {}: {e}; check the disk and FACTORY_STATE, then retry",
                ledger.path.display()
            ),
        )),
    }
}

/// The answer to a request the daemon stopped under: `factory <command>` again.
fn stopped(command: &str) -> Outcome {
    Outcome::Error(refusal(
        Exit::Precondition,
        format!("the daemon stopped (a restart or an upgrade); run the {command} again"),
    ))
}

/// The params of a `method` request, or the refusal of params that don't fit it.
fn params<P: DeserializeOwned>(method: &str, params: Value) -> Result<P, Outcome> {
    serde_json::from_value(params).map_err(|e| {
        Outcome::Error(refusal(
            Exit::Usage,
            format!("bad {method} params: {e}; {MISMATCH}"),
        ))
    })
}

fn answered<R: Serialize>(outcome: Result<R, Failure>) -> Outcome {
    match outcome {
        Ok(result) => Outcome::Result(serde_json::to_value(result).unwrap_or(Value::Null)),
        Err(failure) => Outcome::Error(refusal(failure.exit, failure.message)),
    }
}

fn refusal(exit: Exit, message: String) -> ErrorBody {
    ErrorBody {
        code: exit.code(),
        message,
    }
}
