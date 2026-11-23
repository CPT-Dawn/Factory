//! A Claude Code session's inbox: the unix socket each session binds for cross-session messages
//! and exports to its shells as `CLAUDE_CODE_MESSAGING_SOCKET`, with the token a sender presents
//! in `CLAUDE_CODE_MESSAGING_TOKEN`.
//!
//! A message is newline-delimited JSON on a connection of its own: an `auth` line with the
//! token, then one `user` line, which the session queues as a prompt (or holds, or refuses, as
//! its `crossSessionInbound` setting says). The session answers nothing on the connection.
//!
//! A session that has exited leaves no listener: connecting finds no socket, or is refused.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

pub mod fake;

/// How long connecting to, writing to and hearing the hang-up of a session may take.
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a session takes messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub socket: PathBuf,
    pub token: String,
}

/// Posts messages into sessions. [`Inbox`] posts on the session's socket;
/// [`fake::FakeInbox`] records them.
pub trait InboxApi: Send + Sync + 'static {
    /// Posts `text` into the session at `to` as one user message from `from`.
    fn post(
        &self,
        to: &Address,
        from: &str,
        text: &str,
    ) -> impl Future<Output = Result<(), InboxError>> + Send;
}

/// Why a post failed.
#[derive(Debug)]
pub enum InboxError {
    /// No session listens there any more.
    Gone(io::Error),
    /// The session's socket is there but the post broke or timed out.
    Failed(String),
}

impl fmt::Display for InboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InboxError::Gone(e) => write!(f, "no session listens: {e}"),
            InboxError::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for InboxError {}

/// Claude Code's own inbox sockets.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inbox;

impl InboxApi for Inbox {
    async fn post(&self, to: &Address, from: &str, text: &str) -> Result<(), InboxError> {
        let timed_out =
            || InboxError::Failed(format!("timed out after {}s", POST_TIMEOUT.as_secs()));
        let mut stream = match timeout(POST_TIMEOUT, UnixStream::connect(&to.socket)).await {
            Err(_) => return Err(timed_out()),
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) if gone(&e) => return Err(InboxError::Gone(e)),
            Ok(Err(e)) => return Err(InboxError::Failed(format!("cannot connect: {e}"))),
        };
        let message = frames(&to.token, from, text);
        let sent = async {
            stream.write_all(message.as_bytes()).await?;
            stream.shutdown().await?;
            // The session hangs up once it has read the lines; nothing it sends matters.
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).await.map(|_| ())
        };
        match timeout(POST_TIMEOUT, sent).await {
            Err(_) => Err(timed_out()),
            Ok(Err(e)) => Err(InboxError::Failed(format!("the post broke: {e}"))),
            Ok(Ok(())) => Ok(()),
        }
    }
}

/// The lines of one message: the token, then the text as a user message.
pub fn frames(token: &str, from: &str, text: &str) -> String {
    let auth = json!({"type": "auth", "token": token});
    let user = json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "from": from,
    });
    format!("{auth}\n{user}\n")
}

/// Whether a failed connect means no session listens on the socket.
fn gone(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}
