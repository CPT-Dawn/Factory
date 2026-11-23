//! `factory daemon`: the one long-running process, and the ledger's only writer.
//!
//! It takes the daemon lock (so one runs), binds the socket, writes its pid, prints one ready
//! line once it can answer, then answers requests until it is stopped.

use std::convert::Infallible;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_adapters::gh::Gh;
use factory_adapters::herdr::Herdr;
use factory_core::event::{Event, Fields};
use factory_core::exit::Exit;
use factory_core::wait::{Wait, WaitOn};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::clock;
use crate::lab::service::{self, Lab};
use crate::lab::wire::ACQUIRE;
use crate::leads::Leads;
use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::pollers::Pollers;
use crate::protocol::{
    ErrorBody, LEAD_LIST, LEAD_STATUS, LOG, LeadParams, LogParams, LogResult, MISMATCH, Outcome,
    REVIEW_ACQUIRE, REVIEW_QUOTA, Request, Response, WAIT_CI, WAIT_LEAD, WaitCiParams,
    WaitCiResult, WaitLeadParams,
};
use crate::router::Router;
use crate::store::Store;

/// What every connection answers from: the ledger, the picture of the leads, the reviewer
/// router, the lab's lease store and the CI pollers.
struct Daemon {
    ledger: Arc<Ledger>,
    leads: Arc<Leads>,
    router: Arc<Router>,
    lab: Arc<Lab>,
    pollers: Pollers,
}

pub fn run(json: bool) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    paths.create_runtime()?;
    fs::create_dir_all(&paths.state).map_err(|e| {
        Failure::precondition(format!(
            "cannot create the state dir {}: {e}; check FACTORY_STATE and XDG_STATE_HOME",
            paths.state.display()
        ))
    })?;
    let ledger = Arc::new(Ledger::open(paths.ledger())?);
    let leads = Leads::new(paths.clone(), Herdr::new(paths.herdr.clone()));
    let pollers = Pollers::new(paths.clone(), Gh);
    let _lock = take_daemon_lock(&paths)?;
    let router = Router::open(Store::open(&paths.database())?)?;
    // Opened under the lock: it forgets the waiters a dead daemon left.
    let lab = Lab::open(&paths.lab_db()).map_err(|e| {
        Failure::precondition(format!("{e}; check FACTORY_STATE and XDG_STATE_HOME"))
    })?;
    let listener = bind(&paths)?;
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
        // Ready only once requests can be answered, so this is the one line the daemon prints.
        output::print(
            &Done::new(
                format!("factory daemon ready on {}", socket.display()),
                json!({"socket": socket.to_string_lossy(), "pid": pid}),
            ),
            json,
        );
        tokio::spawn(Arc::clone(&leads).start());
        let router = Arc::new(router);
        match serve(
            listener,
            Arc::new(Daemon {
                ledger,
                leads,
                router,
                lab: Arc::new(lab),
                pollers,
            }),
        )
        .await {}
    })
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
    let listener = StdListener::bind(&socket).map_err(cannot)?;
    listener.set_nonblocking(true).map_err(cannot)?;
    Ok(listener)
}

async fn serve(listener: UnixListener, daemon: Arc<Daemon>) -> Infallible {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(stream, Arc::clone(&daemon)));
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "factory: accept failed: {e}");
            }
        }
    }
}

/// Answers each request line on one connection until the client hangs up, then frees the
/// review slot the connection holds. A client sends one request at a time; one that hangs up,
/// or sends more, before its answer abandons the request, so a wait whose CLI is gone stops
/// waiting. A `lab.acquire` holds the connection while it waits, writing notes before its answer.
async fn answer(stream: UnixStream, daemon: Arc<Daemon>) {
    let mut held = None;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(request) = serde_json::from_str::<Request>(&line)
            && request.method == ACQUIRE
        {
            service::acquire(Arc::clone(&daemon.lab), request, &mut writer, &mut lines).await;
            continue;
        }
        let response = tokio::select! {
            response = respond(&line, &daemon, &mut held) => response,
            _ = lines.next_line() => break,
        };
        let Ok(mut out) = serde_json::to_string(&response) else {
            break;
        };
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
    daemon.router.release(&mut held);
}

async fn respond(line: &str, daemon: &Daemon, held: &mut Option<String>) -> Response {
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
        WAIT_LEAD => match params::<WaitLeadParams>(WAIT_LEAD, request.params) {
            Ok(params) => answered(
                daemon
                    .leads
                    .wait(
                        &params.lead,
                        params.waiter,
                        Duration::from_secs(params.timeout_secs),
                    )
                    .await,
            ),
            Err(refused) => refused,
        },
        WAIT_CI => match params::<WaitCiParams>(WAIT_CI, request.params) {
            Ok(params) => answered(wait_ci(daemon, params).await),
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
        REVIEW_ACQUIRE => daemon.router.acquire(request.params, held).await,
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
    let poll = Duration::from_secs(params.poll_secs.max(1));
    let watch = daemon
        .pollers
        .watch(&params.pr, PathBuf::from(params.cwd), poll)
        .await?;
    let _wait = daemon.leads.register(Wait {
        waiter: params.waiter,
        on: WaitOn::Ci(watch.pr().to_string()),
    });
    watch
        .finished(Duration::from_secs(params.timeout_secs))
        .await
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

/// The append-only event ledger. Each event is stamped and written as one line under one lock,
/// so lines never interleave.
struct Ledger {
    path: PathBuf,
    file: Mutex<File>,
}

impl Ledger {
    fn open(path: PathBuf) -> Result<Ledger, Failure> {
        let file = File::options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot open {}: {e}; check FACTORY_STATE and XDG_STATE_HOME",
                    path.display()
                ))
            })?;
        Ok(Ledger {
            path,
            file: Mutex::new(file),
        })
    }

    fn append(&self, event: &Event) -> io::Result<Fields> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let landed = event.stamped(clock::now());
        let mut line = Value::Object(landed.clone()).to_string();
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(landed)
    }
}
