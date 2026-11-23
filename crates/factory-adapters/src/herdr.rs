//! herdr's socket API, protocol [`PROTOCOL`]: newline-delimited JSON over a unix socket.
//!
//! herdr answers one request per connection, `{"id", "method", "params"}` in and
//! `{"id", "result"}` or `{"id", "error": {"code", "message"}}` out, then hangs up. The one
//! exception is `events.subscribe`: its connection streams `{"event", "data"}` lines until either
//! side hangs up, and herdr resets it if anything more is written, so a changed subscription is a
//! new connection.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;

pub mod fake;

/// The herdr protocol this adapter speaks (herdr 0.9.5). Another one is refused, not guessed at.
pub const PROTOCOL: u32 = 22;

/// How long herdr may take to answer one request.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// What the factory asks of herdr. [`Herdr`] asks the server on a socket; [`fake::FakeHerdr`]
/// answers from a script.
pub trait HerdrApi: Send + Sync + 'static {
    /// Where this herdr listens, for messages.
    fn socket(&self) -> &Path;

    fn ping(&self) -> impl Future<Output = Result<Pong, HerdrError>> + Send;

    fn agents(&self) -> impl Future<Output = Result<Vec<Agent>, HerdrError>> + Send;

    fn workspaces(&self) -> impl Future<Output = Result<Vec<Workspace>, HerdrError>> + Send;

    /// The last `lines` lines of a pane's recent output, without ANSI escapes.
    fn read_pane(
        &self,
        pane_id: &str,
        lines: u32,
    ) -> impl Future<Output = Result<String, HerdrError>> + Send;

    /// Types `text` into the agent `target` (its name) and submits it; with `wait`, answers once
    /// the agent reaches one of its statuses, or fails when its timeout passes first.
    fn prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<&PromptWait>,
    ) -> impl Future<Output = Result<(), HerdrError>> + Send;

    /// Opens a subscription; it streams events until it is dropped or herdr hangs up.
    fn subscribe(
        &self,
        watches: &[Watch],
    ) -> impl Future<Output = Result<Events, HerdrError>> + Send;
}

/// The herdr server listening on one socket.
#[derive(Clone, Debug)]
pub struct Herdr {
    socket: PathBuf,
}

/// Why a herdr request failed.
#[derive(Debug)]
pub enum HerdrError {
    /// Nothing answered on the socket.
    Unreachable(io::Error),
    /// herdr answered with an error.
    Refused { code: String, message: String },
    /// The connection broke, timed out or carried something unreadable.
    Broken(String),
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HerdrError::Unreachable(e) => write!(f, "nothing answers: {e}"),
            HerdrError::Refused { code, message } => write!(f, "herdr refused ({code}): {message}"),
            HerdrError::Broken(why) => f.write_str(why),
        }
    }
}

/// What `ping` answers.
#[derive(Clone, Debug, Deserialize)]
pub struct Pong {
    pub version: String,
    pub protocol: u32,
}

/// One agent herdr detects in a pane.
#[derive(Clone, Debug, Deserialize)]
pub struct Agent {
    /// The name it was started under, such as `lead-<slug>`; none for an unnamed agent.
    pub name: Option<String>,
    /// The agent program herdr detects in the pane; none once it has left.
    pub agent: Option<String>,
    /// One of idle, working, blocked, done, unknown.
    pub agent_status: String,
    pub pane_id: String,
    pub workspace_id: String,
    /// The agent's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The agent's own session, as its integration reported it.
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
}

/// The session an agent reported: an id (Claude Code's session id) or the path of its log.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AgentSession {
    /// `id` or `path`.
    pub kind: String,
    pub value: String,
}

/// What a prompt waits for: the agent reaching one of `until`, at most `timeout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptWait {
    /// herdr's statuses: idle, working, blocked, done, unknown.
    pub until: Vec<String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    /// The git worktree the workspace was opened on, if any.
    pub worktree: Option<Worktree>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Worktree {
    /// The folder name of the main repository.
    pub repo_name: String,
}

/// Something a subscription asks herdr to report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// The agent status of one pane.
    AgentStatus(String),
    /// Any pane closing or its process exiting.
    PaneGone,
    /// An agent appearing in or leaving any pane.
    AgentDetected,
}

/// One event a subscription delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    AgentStatus {
        pane_id: String,
        agent_status: String,
        /// None when the agent has left the pane.
        agent: Option<String>,
    },
    PaneGone {
        pane_id: String,
    },
    AgentDetected {
        pane_id: String,
    },
    /// An event of a kind this adapter doesn't read.
    Other(String),
}

/// The events of one subscription.
pub struct Events {
    source: Source,
}

enum Source {
    /// A subscribed connection to herdr.
    Socket {
        lines: Lines<BufReader<OwnedReadHalf>>,
        /// Kept so the connection stays open while the stream is read.
        _writer: OwnedWriteHalf,
    },
    /// What a [`fake::FakeHerdr`] tells its subscribers.
    Script(mpsc::UnboundedReceiver<Event>),
}

type Connection = (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf);

impl Herdr {
    pub fn new(socket: PathBuf) -> Herdr {
        Herdr { socket }
    }
}

impl HerdrApi for Herdr {
    fn socket(&self) -> &Path {
        &self.socket
    }

    async fn ping(&self) -> Result<Pong, HerdrError> {
        self.call("ping", json!({})).await
    }

    async fn agents(&self) -> Result<Vec<Agent>, HerdrError> {
        #[derive(Deserialize)]
        struct List {
            agents: Vec<Agent>,
        }
        let list: List = self.call("agent.list", json!({})).await?;
        Ok(list.agents)
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, HerdrError> {
        #[derive(Deserialize)]
        struct List {
            workspaces: Vec<Workspace>,
        }
        let list: List = self.call("workspace.list", json!({})).await?;
        Ok(list.workspaces)
    }

    async fn read_pane(&self, pane_id: &str, lines: u32) -> Result<String, HerdrError> {
        #[derive(Deserialize)]
        struct Read {
            read: Text,
        }
        #[derive(Deserialize)]
        struct Text {
            text: String,
        }
        let params = json!({
            "pane_id": pane_id,
            "source": "recent",
            "lines": lines,
            "strip_ansi": true,
        });
        let read: Read = self.call("pane.read", params).await?;
        Ok(read.read.text)
    }

    async fn prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<&PromptWait>,
    ) -> Result<(), HerdrError> {
        let mut params = json!({"target": target, "text": text});
        let mut limit = ANSWER_TIMEOUT;
        if let Some(wait) = wait {
            let timeout_ms = u64::try_from(wait.timeout.as_millis()).unwrap_or(u64::MAX);
            params["wait"] = json!({"until": wait.until, "timeout_ms": timeout_ms});
            limit += wait.timeout;
        }
        let (mut lines, _writer) = self.send("agent.prompt", params).await?;
        answer(&mut lines, limit).await?;
        Ok(())
    }

    async fn subscribe(&self, watches: &[Watch]) -> Result<Events, HerdrError> {
        let subscriptions: Vec<Value> = watches
            .iter()
            .flat_map(|watch| match watch {
                Watch::AgentStatus(pane) => {
                    vec![json!({"type": "pane.agent_status_changed", "pane_id": pane})]
                }
                Watch::PaneGone => vec![
                    json!({"type": "pane.closed"}),
                    json!({"type": "pane.exited"}),
                ],
                Watch::AgentDetected => vec![json!({"type": "pane.agent_detected"})],
            })
            .collect();
        let (mut lines, writer) = self
            .send("events.subscribe", json!({"subscriptions": subscriptions}))
            .await?;
        answer(&mut lines, ANSWER_TIMEOUT).await?;
        Ok(Events {
            source: Source::Socket {
                lines,
                _writer: writer,
            },
        })
    }
}

impl Herdr {
    /// Sends one request on a connection of its own and reads its answer.
    async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<R, HerdrError> {
        let (mut lines, _writer) = self.send(method, params).await?;
        let result = answer(&mut lines, ANSWER_TIMEOUT).await?;
        serde_json::from_value(result)
            .map_err(|e| HerdrError::Broken(format!("unreadable answer to {method}: {e}")))
    }

    async fn send(&self, method: &str, params: Value) -> Result<Connection, HerdrError> {
        let stream = timeout(ANSWER_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| {
                HerdrError::Broken(format!(
                    "connecting timed out after {}s",
                    ANSWER_TIMEOUT.as_secs()
                ))
            })?
            .map_err(HerdrError::Unreachable)?;
        let (reader, mut writer) = stream.into_split();
        let mut line = json!({"id": "factory", "method": method, "params": params}).to_string();
        line.push('\n');
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| HerdrError::Broken(format!("lost herdr sending {method}: {e}")))?;
        Ok((BufReader::new(reader).lines(), writer))
    }
}

/// Reads the answer line every connection starts with, waiting at most `limit`.
async fn answer(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    limit: Duration,
) -> Result<Value, HerdrError> {
    let line = timeout(limit, lines.next_line())
        .await
        .map_err(|_| HerdrError::Broken(format!("no answer within {}s", limit.as_secs())))?
        .map_err(|e| HerdrError::Broken(format!("lost herdr: {e}")))?
        .ok_or_else(|| HerdrError::Broken("herdr hung up without answering".to_owned()))?;
    let mut answer: Value = serde_json::from_str(&line)
        .map_err(|e| HerdrError::Broken(format!("unreadable answer: {e}")))?;
    if let Some(error) = answer.get("error") {
        let text = |key: &str| {
            error
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned()
        };
        return Err(HerdrError::Refused {
            code: text("code"),
            message: text("message"),
        });
    }
    Ok(answer
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

impl Events {
    /// The next event; none once herdr hangs up.
    pub async fn next(&mut self) -> Option<Event> {
        match &mut self.source {
            Source::Socket { lines, .. } => loop {
                let line = lines.next_line().await.ok()??;
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    return Some(event(&value));
                }
            },
            Source::Script(events) => events.recv().await,
        }
    }
}

/// Reads one event line. herdr names a kind `pane.closed` in subscriptions and `pane_closed` in
/// event data; both spellings are read.
fn event(value: &Value) -> Event {
    let kind = value
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace('.', "_");
    let data = value.get("data").unwrap_or(&Value::Null);
    let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    let pane_id = text("pane_id").unwrap_or_default();
    match kind.as_str() {
        "pane_agent_status_changed" => Event::AgentStatus {
            pane_id,
            agent_status: text("agent_status").unwrap_or_default(),
            agent: text("agent"),
        },
        "pane_closed" | "pane_exited" => Event::PaneGone { pane_id },
        "pane_agent_detected" => Event::AgentDetected { pane_id },
        _ => Event::Other(kind),
    }
}
