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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use factory_core::event::{Event, Fields};
use factory_core::exit::Exit;
use factory_core::timestamp::Timestamp;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::leads::Leads;
use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::protocol::{
    ErrorBody, LEAD_LIST, LEAD_STATUS, LOG, LeadParams, LogParams, LogResult, Outcome, Request,
    Response, WAIT_LEAD, WaitLeadParams,
};

/// What every connection answers from: the ledger and the picture of the leads.
struct Daemon {
    ledger: Arc<Ledger>,
    leads: Arc<Leads>,
}

/// What to do when the CLI and the daemon don't speak the same protocol.
const MISMATCH: &str = "the daemon may be older than this factory: restart it";

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
    let leads = Leads::new(paths.clone());
    let _lock = take_daemon_lock(&paths)?;
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
        match serve(listener, Arc::new(Daemon { ledger, leads })).await {}
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

/// Answers each request line on one connection until the client hangs up. A client sends one
/// request at a time; one that hangs up, or sends more, before its answer abandons the request,
/// so a wait whose CLI is gone stops waiting.
async fn answer(stream: UnixStream, daemon: Arc<Daemon>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let response = tokio::select! {
            response = respond(&line, &daemon) => response,
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

async fn respond(line: &str, daemon: &Daemon) -> Response {
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
        LEAD_STATUS => match params::<LeadParams>(request.params) {
            Ok(params) => answered(daemon.leads.status(&params.lead).await),
            Err(refused) => refused,
        },
        WAIT_LEAD => match params::<WaitLeadParams>(request.params) {
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
        other => Outcome::Error(refusal(
            Exit::Usage,
            format!("unknown method {other:?}; {MISMATCH}"),
        )),
    };
    Response {
        id: Some(request.id),
        outcome,
    }
}

fn log(params: Value, ledger: &Ledger) -> Outcome {
    let params: LogParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(e) => {
            return Outcome::Error(refusal(
                Exit::Usage,
                format!("bad log params: {e}; {MISMATCH}"),
            ));
        }
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

fn params<P: DeserializeOwned>(params: Value) -> Result<P, Outcome> {
    serde_json::from_value(params)
        .map_err(|e| Outcome::Error(refusal(Exit::Usage, format!("bad params: {e}; {MISMATCH}"))))
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
        let landed = event.stamped(now());
        let mut line = Value::Object(landed.clone()).to_string();
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(landed)
    }
}

fn now() -> Timestamp {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp::from_unix_millis(i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX))
}
