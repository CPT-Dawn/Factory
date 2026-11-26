//! Codex's usage windows: `codex app-server`, spoken to over stdio in JSON-RPC lines.
//!
//! The exchange is `initialize` (id 1), the `initialized` notification, then
//! `account/rateLimits/read` (id 2). The server may print notifications of its own between the
//! replies, so lines are read until the reply with id 2. The server keeps running until it is
//! told to stop, and the exchange spends no model tokens: the process is killed, with its group,
//! once the answer is in or its time is up.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

/// How long the app-server may take to answer, by default.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// The three messages of the exchange, one JSON object a line. There is no `jsonrpc` field:
/// the app-server does not speak the envelope.
const REQUESTS: [&str; 3] = [
    r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"factory","title":null,"version":"0.1.0"}}}"#,
    r#"{"method":"initialized"}"#,
    r#"{"id":2,"method":"account/rateLimits/read"}"#,
];

/// Why the app-server gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexError {
    /// There is no `codex` to run.
    Missing,
    /// It could not be started: why.
    Spawn(String),
    /// It did not answer in time, so it was killed with its process group.
    TimedOut(Duration),
    /// It ended before it answered, with this exit status when it had one.
    Closed(Option<i32>),
    /// Reading its output failed.
    Read(String),
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodexError::Missing => write!(f, "codex is not installed"),
            CodexError::Spawn(why) => write!(f, "it could not be started: {why}"),
            CodexError::TimedOut(timeout) => {
                write!(f, "it did not answer within {}s", timeout.as_secs().max(1))
            }
            CodexError::Closed(Some(code)) => {
                write!(f, "it exited with status {code} before it answered")
            }
            CodexError::Closed(None) => write!(f, "it ended before it answered"),
            CodexError::Read(why) => write!(f, "its output could not be read: {why}"),
        }
    }
}

impl std::error::Error for CodexError {}

pub trait CodexUsage: Send + Sync {
    /// The app-server's reply to `account/rateLimits/read`: the one line whose id is 2. The
    /// server is killed, with its process group, once it has answered or `timeout` has passed.
    fn rate_limits(&self, timeout: Duration) -> Result<String, CodexError>;
}

/// `codex app-server` from PATH, or the program named.
pub struct AppServer {
    program: String,
}

impl AppServer {
    /// `codex` from PATH.
    pub fn new() -> Self {
        Self::with("codex")
    }

    /// `program app-server`.
    pub fn with(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Default for AppServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `line` is the reply with id 2: a JSON object with that id that is no request of the
/// server's own (those carry a `method`).
fn is_reply_2(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .is_some_and(|value| {
            value.get("id").and_then(|id| id.as_i64()) == Some(2) && value.get("method").is_none()
        })
}

impl CodexUsage for AppServer {
    fn rate_limits(&self, timeout: Duration) -> Result<String, CodexError> {
        // A time too long to count is refused before there is a child to strand.
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| CodexError::Spawn("its time is too long to count".to_owned()))?;
        let mut cmd = Command::new(&self.program);
        cmd.arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => CodexError::Missing,
            _ => CodexError::Spawn(e.to_string()),
        })?;
        let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
        let outcome = (|| {
            let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                return (Err(CodexError::Spawn("no pipes".to_owned())), None);
            };
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let end = line.is_err();
                    if tx.send(line).is_err() || end {
                        break;
                    }
                }
            });
            // A server that ended already refuses the writes: its exit is what to report, and
            // reading its output says so.
            for request in REQUESTS {
                if writeln!(stdin, "{request}")
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            let answer = loop {
                // The time is up whatever is still buffered: a server that never stops
                // chattering must not outlast it.
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break Err(CodexError::TimedOut(timeout));
                }
                match rx.recv_timeout(left) {
                    Ok(Ok(line)) if is_reply_2(&line) => break Ok(line),
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => break Err(CodexError::Read(e.to_string())),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        break Err(CodexError::TimedOut(timeout));
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(CodexError::Closed(None));
                    }
                }
            };
            // Stdin stays open until the answer is in: a server that sees it close may stop.
            (answer, Some(stdin))
        })();
        if let Some(group) = group {
            let _ = kill_process_group(group, Signal::KILL);
        }
        let (answer, stdin) = outcome;
        drop(stdin);
        let status = child.wait().ok().and_then(|status| status.code());
        match answer {
            Err(CodexError::Closed(None)) => Err(CodexError::Closed(status)),
            other => other,
        }
    }
}

/// A Codex that answers from a script: each ask takes the next answer, and the last one
/// stands once the rest are used. It counts the asks.
#[derive(Default)]
pub struct FakeCodex {
    answers: Mutex<VecDeque<Result<String, CodexError>>>,
    asked: Mutex<u32>,
}

impl FakeCodex {
    pub fn new(answers: impl IntoIterator<Item = Result<String, CodexError>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            asked: Mutex::new(0),
        }
    }

    /// How many times it was asked.
    pub fn asked(&self) -> u32 {
        self.asked.lock().map_or(0, |asked| *asked)
    }
}

impl CodexUsage for FakeCodex {
    fn rate_limits(&self, _timeout: Duration) -> Result<String, CodexError> {
        if let Ok(mut asked) = self.asked.lock() {
            *asked += 1;
        }
        let Ok(mut answers) = self.answers.lock() else {
            return Err(CodexError::Missing);
        };
        if answers.len() > 1 {
            answers.pop_front()
        } else {
            answers.front().cloned()
        }
        .unwrap_or(Err(CodexError::Missing))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    const REPLY: &str = r#"{"id":2,"result":{"rateLimits":{"primary":null}}}"#;

    /// An executable shell script in `dir`, standing in for `codex`.
    fn script(dir: &Path, body: &str) -> String {
        let path = dir.join("codex");
        let _ = std::fs::write(&path, format!("#!/bin/sh\n{body}\n"));
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        path.display().to_string()
    }

    fn app_server(body: &str, timeout: Duration) -> Result<String, CodexError> {
        let dir = tempfile::tempdir().map_err(|e| CodexError::Spawn(e.to_string()))?;
        let server = AppServer::with(script(dir.path(), body));
        // A script just written can be held open for a moment by a sibling test's fork
        // ("Text file busy"): the start is tried again, a few times, before it counts.
        let mut answer = server.rate_limits(timeout);
        for _ in 0..5 {
            match &answer {
                Err(CodexError::Spawn(why)) if why.contains("Text file busy") => {
                    answer = server.rate_limits(timeout);
                }
                _ => break,
            }
        }
        answer
    }

    #[test]
    fn it_reads_lines_until_the_reply_with_id_2_and_stops_the_server() {
        // Notifications and the reply to id 1 come first, and it never ends by itself.
        let body = format!(
            "read a; read b; read c\n\
             echo '{{\"method\":\"remoteControl/status/changed\",\"params\":{{}}}}'\n\
             echo '{{\"id\":1,\"result\":{{}}}}'\n\
             echo 'not json'\n\
             echo '{REPLY}'\n\
             echo '{{\"method\":\"account/updated\"}}'\n\
             sleep 30"
        );
        let started = Instant::now();

        let answer = app_server(&body, Duration::from_secs(10));

        assert_eq!(answer, Ok(REPLY.to_owned()));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it waited for the server"
        );
    }

    #[test]
    fn it_sends_the_three_messages_without_a_jsonrpc_field() {
        let dir = tempfile::tempdir().map_err(|e| e.to_string());
        let Ok(dir) = dir else { return };
        let log = dir.path().join("received");
        let body = format!(
            "read a; read b; read c; printf '%s\\n%s\\n%s\\n' \"$a\" \"$b\" \"$c\" > {}; echo '{REPLY}'; sleep 30",
            log.display()
        );
        let program = script(dir.path(), &body);

        let answer = AppServer::with(program).rate_limits(Duration::from_secs(10));

        assert_eq!(answer, Ok(REPLY.to_owned()));
        let received = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = received.lines().collect();
        assert_eq!(lines.len(), 3, "{received}");
        assert!(lines[0].contains(r#""method":"initialize""#) && lines[0].contains(r#""id":1"#));
        assert_eq!(lines[1], r#"{"method":"initialized"}"#);
        assert_eq!(lines[2], r#"{"id":2,"method":"account/rateLimits/read"}"#);
        assert!(!received.contains("jsonrpc"));
    }

    #[test]
    fn a_server_that_never_answers_is_killed_at_its_time() {
        let started = Instant::now();

        let answer = app_server("sleep 30", Duration::from_secs(1));

        assert_eq!(answer, Err(CodexError::TimedOut(Duration::from_secs(1))));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            CodexError::TimedOut(Duration::from_secs(20)).to_string(),
            "it did not answer within 20s"
        );
    }

    #[test]
    fn a_server_that_never_stops_chattering_is_killed_at_its_time() {
        let started = Instant::now();

        let answer = app_server(
            "while :; do echo '{\"method\":\"account/updated\"}'; done",
            Duration::from_secs(1),
        );

        assert_eq!(answer, Err(CodexError::TimedOut(Duration::from_secs(1))));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_time_too_long_to_count_is_refused_before_a_child_starts() {
        let answer = AppServer::with("/nonexistent/codex").rate_limits(Duration::MAX);
        assert!(matches!(answer, Err(CodexError::Spawn(_))), "{answer:?}");
    }

    #[test]
    fn a_server_that_ends_first_says_how() {
        assert_eq!(
            app_server("exit 127", Duration::from_secs(10)),
            Err(CodexError::Closed(Some(127)))
        );
        assert_eq!(
            CodexError::Closed(Some(127)).to_string(),
            "it exited with status 127 before it answered"
        );
    }

    #[test]
    fn a_request_of_the_servers_own_with_id_2_is_not_the_reply() {
        let body = "read a; read b; read c\necho '{\"id\":2,\"method\":\"x\"}'\nsleep 30";
        assert_eq!(
            app_server(body, Duration::from_secs(1)),
            Err(CodexError::TimedOut(Duration::from_secs(1)))
        );
    }

    #[test]
    fn no_codex_is_missing() {
        let answer = AppServer::with("/nonexistent/codex").rate_limits(Duration::from_secs(1));
        assert_eq!(answer, Err(CodexError::Missing));
    }

    #[test]
    fn the_fake_answers_in_turn_and_counts_the_asks() {
        let fake = FakeCodex::new([Ok("a".to_owned()), Err(CodexError::Missing)]);
        assert_eq!(fake.rate_limits(TIMEOUT), Ok("a".to_owned()));
        assert_eq!(fake.rate_limits(TIMEOUT), Err(CodexError::Missing));
        assert_eq!(fake.rate_limits(TIMEOUT), Err(CodexError::Missing));
        assert_eq!(fake.asked(), 3);
    }
}
