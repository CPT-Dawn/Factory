//! Desktop notifications: `notify-send`, as devlab's TypeScript watcher sent them. The daemon
//! runs as a systemd user service, whose environment may lack the session bus, so a post
//! names it from `$XDG_RUNTIME_DIR` when it is missing.

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process::{self, RunError};

/// How long one post may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Why a post failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostError {
    /// There is no `notify-send` to run.
    Missing,
    /// It ran and failed, or could not be run: what went wrong.
    Failed(String),
}

pub trait Desktop: Send + Sync {
    /// Posts one notification from `app`, with `urgency` `low`, `normal` or `critical`.
    fn post(&self, app: &str, urgency: &str, summary: &str, body: &str) -> Result<(), PostError>;
}

/// `notify-send` from PATH.
pub struct NotifySend;

impl Desktop for NotifySend {
    fn post(&self, app: &str, urgency: &str, summary: &str, body: &str) -> Result<(), PostError> {
        let mut cmd = Command::new("notify-send");
        cmd.args(["-a", app, "-u", urgency, summary, body])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none_or(|v| v.is_empty())
            && let Some(bus) = session_bus()
        {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", bus);
        }
        match process::run(cmd, TIMEOUT) {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(PostError::Failed(format!(
                "notify-send: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
            Err(RunError::Spawn(e)) if e.kind() == io::ErrorKind::NotFound => {
                Err(PostError::Missing)
            }
            Err(e) => Err(PostError::Failed(format!("notify-send: {e}"))),
        }
    }
}

/// The user's session bus: `$XDG_RUNTIME_DIR/bus`, as systemd's user manager sets it up.
fn session_bus() -> Option<String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    Some(format!(
        "unix:path={}",
        PathBuf::from(runtime).join("bus").display()
    ))
}

/// A desktop that keeps what was posted to it, or has no `notify-send`.
#[derive(Default)]
pub struct FakeDesktop {
    /// Each post: app, urgency, summary and body.
    pub posts: std::sync::Mutex<Vec<[String; 4]>>,
    pub missing: bool,
}

impl Desktop for FakeDesktop {
    fn post(&self, app: &str, urgency: &str, summary: &str, body: &str) -> Result<(), PostError> {
        if self.missing {
            return Err(PostError::Missing);
        }
        self.posts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push([app, urgency, summary, body].map(str::to_owned));
        Ok(())
    }
}
