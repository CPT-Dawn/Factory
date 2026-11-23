//! Session inboxes in memory, for code that runs against [`InboxApi`] inside one process.
//!
//! The binary-seam tests can't reach into the daemon, so they run a fake inbox server on a socket
//! instead (`crates/factory/tests/cli/support/inbox.rs`).

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::{Address, InboxApi, InboxError};

/// Inboxes that record every post; a socket marked gone refuses them.
#[derive(Debug, Default)]
pub struct FakeInbox {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    gone: HashSet<PathBuf>,
    posts: Vec<(Address, String, String)>,
}

impl FakeInbox {
    pub fn new() -> FakeInbox {
        FakeInbox::default()
    }

    /// Makes the session at `socket` gone: posts to it fail with [`InboxError::Gone`].
    pub fn gone(&self, socket: impl Into<PathBuf>) {
        self.lock().gone.insert(socket.into());
    }

    /// Every post that landed, oldest first: its address, sender and text.
    pub fn posts(&self) -> Vec<(Address, String, String)> {
        self.lock().posts.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl InboxApi for FakeInbox {
    async fn post(&self, to: &Address, from: &str, text: &str) -> Result<(), InboxError> {
        let mut state = self.lock();
        if state.gone.contains(&to.socket) {
            return Err(InboxError::Gone(io::Error::from(io::ErrorKind::NotFound)));
        }
        state
            .posts
            .push((to.clone(), from.to_owned(), text.to_owned()));
        Ok(())
    }
}
