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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use factory_adapters::gh::Gh;
use factory_core::duration;
use factory_core::event::Event;
use factory_core::exit::Exit;
use factory_core::wait::{Wait, WaitOn};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinSet;

use factory_adapters::herdr::Herdr;
use factory_adapters::inbox::Inbox;
use listenfd::ListenFd;

use crate::lab::service::{self, Lab};
use crate::lab::wire::{ACQUIRE, RESERVE};
use crate::leads::Leads;
use crate::ledger::Ledger;
use crate::notifier::Notifier;
use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::pollers::Pollers;
use crate::protocol::{
    ErrorBody, LEAD_LIST, LEAD_SPAWN, LEAD_STATUS, LOG, LeadParams, LogParams, LogResult, MISMATCH,
    MONITOR_START, MONITOR_STATUS, MONITOR_STOP, NotifyParams, Outcome, REVIEW_ACQUIRE,
    REVIEW_QUOTA, Request, Response, STATUS, SpawnParams, StatusParams, WAIT_CI, WAIT_LEAD,
    WAIT_NOTIFY, WaitCiParams, WaitCiResult, WaitLeadParams,
};
use crate::router::Router;
use crate::status;
use crate::steward::Steward;
use crate::store::Store;

/// What every connection answers from: the ledger, the picture of the leads, the reviewer
/// router, the lab's lease store, the CI pollers and the waits that outlive their CLI.
struct Daemon {
    ledger: Arc<Ledger>,
    leads: Arc<Leads>,
    router: Arc<Router>,
    lab: Arc<Lab>,
    pollers: Arc<Pollers>,
    steward: Arc<Steward>,
    notifier: Arc<Notifier>,
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
    let ledger = Arc::new(Ledger::open(paths.ledger())?);
    let _lock = take_daemon_lock(&paths)?;
    let leads = Leads::new(
        paths.clone(),
        Herdr::new(paths.herdr.clone()),
        Arc::clone(&ledger),
        Store::open(&paths.database())?,
    )?;
    let pollers = Arc::new(Pollers::new(paths.clone(), Gh));
    let router = Arc::new(Router::open(Store::open(&paths.database())?)?);
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
    let lab = Lab::open(&paths.lab_db()).map_err(|e| {
        Failure::precondition(format!("{e}; check FACTORY_STATE and XDG_STATE_HOME"))
    })?;
    let listener = match activated {
        Some(listener) => listener,
        None => bind(&paths)?,
    };
    listener.set_nonblocking(true).map_err(|e| {
        Failure::precondition(format!(
            "cannot listen on {}: {e}; retry",
            paths.socket().display()
        ))
    })?;
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
        let listener = UnixListener::from_std(listener).map_err(|e| {
            Failure::precondition(format!("cannot listen on {}: {e}; retry", socket.display()))
        })?;
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
        let lab = Arc::new(lab);
        lab.recover();
        notifier.resume();
        let daemon = Daemon {
            ledger,
            leads,
            router,
            lab,
            pollers,
            steward,
            notifier,
        };
        serve(listener, Arc::new(daemon), stop).await;
        Ok(())
    })?;
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

/// Binds the socket, replacing one a dead daemon left behind (the daemon lock says none runs).
fn bind(paths: &Paths) -> Result<StdListener, Failure> {
    let socket = paths.socket();
    let cannot = |e: io::Error| {
        Failure::precondition(format!(
            "cannot listen on {}: {e}; check XDG_RUNTIME_DIR (a socket path must be under 108 bytes)",
            socket.display()
        ))
    };
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(cannot(e)),
    }
    StdListener::bind(&socket).map_err(cannot)
}

/// Answers connections until the daemon is stopping, then lets the ones it took finish, for
/// up to [`DRAIN_TIMEOUT`]. Connections it never took stay queued in the socket.
async fn serve(listener: UnixListener, daemon: Arc<Daemon>, stop: Stop) {
    // The connections hear of the stop only once the listener is gone, so a call made after
    // a cut-short answer queues for the next daemon instead of in this one's closing backlog.
    let (hang_up, hung_up) = watch::channel(false);
    let hung_up = Stop(hung_up);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = stop.clone().stopping() => break,
            accepted = listener.accept() => match accepted {
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
async fn converse(stream: UnixStream, daemon: &Daemon, stop: &Stop, held: &mut Option<String>) {
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
        if let Ok(request) = serde_json::from_str::<Request>(&line)
            && request.method == RESERVE
        {
            service::reserve(Arc::clone(&daemon.lab), request, &mut writer, &mut lines).await;
            continue;
        }
        if let Ok(request) = serde_json::from_str::<Request>(&line)
            && request.method == ACQUIRE
        {
            // Not cut short by a stop: a lease granted as it was dropped would go unanswered.
            service::acquire(
                Arc::clone(&daemon.lab),
                Arc::clone(&daemon.ledger),
                request,
                &mut writer,
                &mut lines,
                stop.clone().stopping(),
            )
            .await;
            continue;
        }
        let response = tokio::select! {
            response = respond(&line, daemon, stop, held) => response,
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

async fn respond(line: &str, daemon: &Daemon, stop: &Stop, held: &mut Option<String>) -> Response {
    let request: Request = match serde_json::from_str(line) {
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
        WAIT_LEAD => match params::<WaitLeadParams>(WAIT_LEAD, request.params) {
            Ok(params) => {
                let wait = daemon.leads.wait(
                    &params.lead,
                    params.waiter,
                    Duration::from_secs(params.timeout_secs),
                );
                // Biased: a stopping daemon answers no wait, even one that ends as it stops.
                tokio::select! {
                    biased;
                    () = stop.clone().stopping() => stopped("wait"),
                    waited = wait => answered(waited),
                }
            }
            Err(refused) => refused,
        },
        WAIT_CI => match params::<WaitCiParams>(WAIT_CI, request.params) {
            // Biased, as a lead wait: a stopping daemon answers no wait.
            Ok(params) => tokio::select! {
                biased;
                () = stop.clone().stopping() => stopped("wait"),
                waited = wait_ci(daemon, params) => answered(waited),
            },
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
                )
                .await,
            ),
            Err(refused) => refused,
        },
        LOG => {
            let ledger = Arc::clone(&daemon.ledger);
            let params = request.params;
            match tokio::task::spawn_blocking(move || log(params, &ledger)).await {
                Ok(outcome) => outcome,
                Err(e) => Outcome::Error(refusal(
                    Exit::Precondition,
                    format!("log failed in the daemon: {e}; retry"),
                )),
            }
        }
        REVIEW_ACQUIRE => {
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

/// Joins the pull request's poller, then shows the waiter's lead as waiting on it until the
/// checks finish.
async fn wait_ci(daemon: &Daemon, params: WaitCiParams) -> Result<WaitCiResult, Failure> {
    let limit = Duration::from_secs(params.timeout_secs);
    // The timeout counts from the request, finding the pull request included.
    let deadline = tokio::time::Instant::now() + limit;
    let poll = Duration::from_secs(params.poll_secs.max(1));
    let found = daemon
        .pollers
        .watch(&params.pr, PathBuf::from(params.cwd), poll);
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
    let _wait = daemon.leads.register(Wait {
        waiter: params.waiter,
        on: WaitOn::Ci(watch.pr().to_string()),
    });
    watch.finished(deadline, limit).await
}

fn log(params: Value, ledger: &Ledger) -> Outcome {
    let params: LogParams = match self::params(LOG, params) {
        Ok(params) => params,
        Err(refused) => return refused,
    };
    let event = match Event::new(&params.kind, params.fields) {
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
