//! A fake Claude Code session inbox: a server on a unix socket, as Claude Code binds one for each
//! session and exports to its shells as `CLAUDE_CODE_MESSAGING_SOCKET`, with the token a sender
//! presents in `CLAUDE_CODE_MESSAGING_TOKEN`.
//!
//! The fake keeps the lines of every connection it accepts, each read as JSON, so a test can read
//! what the session would have been told. Claude Code answers nothing on the connection; neither
//! does the fake.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

use serde_json::Value;

use super::{DEADLINE, Sandbox};

/// The token the fake's session hands its shells.
pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";
/// The fake session's id.
pub const SESSION: &str = "5e55104e-0000-4000-8000-000000000001";

pub struct FakeInbox {
    socket: PathBuf,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    posts: Mutex<Vec<Vec<Value>>>,
    changed: Condvar,
}

impl FakeInbox {
    /// Starts a fake inbox on `<sandbox>/cc-socks/<name>.sock`.
    pub fn start(sandbox: &Sandbox, name: &str) -> FakeInbox {
        let dir = sandbox.path("cc-socks");
        fs::create_dir_all(&dir).expect("create the inbox dir");
        let socket = dir.join(format!("{name}.sock"));
        let listener = UnixListener::bind(&socket).expect("bind the fake inbox");
        let shared = Arc::new(Shared::default());
        let serving = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&serving);
                thread::spawn(move || {
                    let lines: Vec<Value> = BufReader::new(stream)
                        .lines()
                        .map_while(Result::ok)
                        .map(|line| serde_json::from_str(&line).unwrap_or(Value::String(line)))
                        .collect();
                    shared.lock().push(lines);
                    shared.changed.notify_all();
                });
            }
        });
        FakeInbox { socket, shared }
    }

    /// Runs `cmd` as a shell of this session does: with the session's socket, token and id.
    pub fn inside(&self, cmd: &mut Command) {
        cmd.env("CLAUDE_CODE_MESSAGING_SOCKET", &self.socket)
            .env("CLAUDE_CODE_MESSAGING_TOKEN", TOKEN)
            .env("CLAUDE_CODE_SESSION_ID", SESSION);
    }

    /// The lines of every connection so far, oldest first.
    pub fn posts(&self) -> Vec<Vec<Value>> {
        self.shared.lock().clone()
    }

    /// Waits until `n` connections have ended, failing the test after [`DEADLINE`].
    pub fn await_posts(&self, n: usize) -> Vec<Vec<Value>> {
        let start = Instant::now();
        let mut posts = self.shared.lock();
        while posts.len() < n {
            let left = DEADLINE
                .checked_sub(start.elapsed())
                .unwrap_or_else(|| panic!("{} posts within {DEADLINE:?}, not {n}", posts.len()));
            posts = self
                .shared
                .changed
                .wait_timeout(posts, left)
                .expect("wait on the fake inbox")
                .0;
        }
        posts.clone()
    }

    /// Takes the socket away, as a session that exits does: nothing connects any more.
    pub fn close(&self) {
        fs::remove_file(&self.socket).expect("remove the fake inbox socket");
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Vec<Vec<Value>>> {
        self.posts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
