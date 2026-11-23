//! `factory daemon`: the one long-running process, and the ledger's only writer.
//!
//! It takes the daemon lock (so one runs), binds the socket, writes its pid, prints one ready
//! line, then answers requests until it is stopped.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::net::UnixListener as StdListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use factory_core::event::{Event, Fields};
use factory_core::exit::Exit;
use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::output::{self, Done, Failure};
use crate::paths::Paths;
use crate::protocol::{ErrorBody, LOG, LogParams, LogResult, Outcome, Request, Response};

pub fn run(json: bool) -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    paths.create_runtime()?;
    fs::create_dir_all(&paths.state).map_err(|e| {
        Failure::precondition(format!(
            "cannot create the state dir {}: {e}; check FACTORY_STATE and XDG_STATE_HOME",
            paths.state.display()
        ))
    })?;
    let ledger = Ledger::open(paths.ledger())?;
    let _lock = take_daemon_lock(&paths)?;
    let listener = bind(&paths)?;
    let pid = std::process::id();
    fs::write(paths.daemon_pid(), format!("{pid}\n")).map_err(|e| {
        Failure::precondition(format!(
            "cannot write {}: {e}",
            paths.daemon_pid().display()
        ))
    })?;
    let socket = paths.socket();
    output::print(
        &Done {
            line: format!("factory daemon ready on {}", socket.display()),
            json: json!({"socket": socket, "pid": pid}),
        },
        json,
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .build()
        .map_err(|e| Failure::precondition(format!("cannot start the daemon's runtime: {e}")))?;
    let stopped = runtime.block_on(serve(listener, Arc::new(ledger)));
    Err(Failure::precondition(format!(
        "the daemon stopped: {stopped}"
    )))
}

fn take_daemon_lock(paths: &Paths) -> Result<File, Failure> {
    let path = paths.daemon_lock();
    let cannot = |e: &dyn std::fmt::Display| {
        Failure::precondition(format!("cannot lock {}: {e}", path.display()))
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
    let cannot =
        |e: io::Error| Failure::precondition(format!("cannot listen on {}: {e}", socket.display()));
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(cannot(e)),
    }
    let listener = StdListener::bind(&socket).map_err(cannot)?;
    listener.set_nonblocking(true).map_err(cannot)?;
    Ok(listener)
}

async fn serve(listener: StdListener, ledger: Arc<Ledger>) -> io::Error {
    let listener = match UnixListener::from_std(listener) {
        Ok(listener) => listener,
        Err(e) => return e,
    };
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(stream, Arc::clone(&ledger)));
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "factory: accept failed: {e}");
            }
        }
    }
}

/// Answers each request line on one connection until the client hangs up.
async fn answer(stream: UnixStream, ledger: Arc<Ledger>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let response = respond(&line, &ledger).await;
        let Ok(mut out) = serde_json::to_string(&response) else {
            return;
        };
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn respond(line: &str, ledger: &Arc<Ledger>) -> Response {
    let request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(e) => {
            return Response {
                id: None,
                outcome: Outcome::Error(refusal(Exit::Usage, format!("unreadable request: {e}"))),
            };
        }
    };
    let outcome = match request.method.as_str() {
        LOG => {
            let ledger = Arc::clone(ledger);
            let params = request.params;
            match tokio::task::spawn_blocking(move || log(params, &ledger)).await {
                Ok(outcome) => outcome,
                Err(e) => Outcome::Error(refusal(Exit::Precondition, format!("log failed: {e}"))),
            }
        }
        other => Outcome::Error(refusal(Exit::Usage, format!("unknown method {other:?}"))),
    };
    Response {
        id: Some(request.id),
        outcome,
    }
}

fn log(params: Value, ledger: &Ledger) -> Outcome {
    let params: LogParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(e) => return Outcome::Error(refusal(Exit::Usage, format!("bad log params: {e}"))),
    };
    let event = match Event::new(&params.kind, params.fields) {
        Ok(event) => event,
        Err(e) => return Outcome::Error(refusal(Exit::Usage, e.to_string())),
    };
    match ledger.append(&event) {
        Ok(landed) => {
            let result = LogResult {
                ledger: ledger.path.clone(),
                event: landed,
            };
            Outcome::Result(serde_json::to_value(result).unwrap_or(Value::Null))
        }
        Err(e) => Outcome::Error(refusal(
            Exit::Precondition,
            format!("cannot append to {}: {e}", ledger.path.display()),
        )),
    }
}

fn refusal(exit: Exit, message: String) -> ErrorBody {
    ErrorBody {
        code: exit.code(),
        message,
    }
}

/// The append-only event ledger. Each event is stamped and written as one line under one lock,
/// so lines never interleave and their timestamps never go backwards.
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
            .map_err(|e| Failure::precondition(format!("cannot open {}: {e}", path.display())))?;
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
