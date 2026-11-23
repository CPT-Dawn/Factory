//! The wait registry: who waits on what.
//!
//! A wait is registered when it starts and cleared when it ends, so a lead that waits shows as
//! `waiting: <what>` instead of idle or working.

use std::collections::BTreeMap;
use std::fmt;

/// What a wait is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitOn {
    /// A lead, by its herdr agent name.
    Lead(String),
    /// A pull request's checks, by `<owner>/<repo>#<number>`.
    Ci(String),
    /// A review, by its label.
    Review(String),
}

impl fmt::Display for WaitOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitOn::Lead(name) => f.write_str(name),
            WaitOn::Ci(pr) => write!(f, "ci {pr}"),
            WaitOn::Review(label) => write!(f, "review {label}"),
        }
    }
}

/// One registered wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    /// The herdr pane the wait was registered from; none outside herdr.
    pub waiter: Option<String>,
    pub on: WaitOn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WaitId(u64);

/// Every registered wait. Ids are never reused.
#[derive(Clone, Debug, Default)]
pub struct Waits {
    next: u64,
    waits: BTreeMap<WaitId, Wait>,
}

impl Waits {
    pub fn register(&mut self, wait: Wait) -> WaitId {
        let id = WaitId(self.next);
        self.next += 1;
        self.waits.insert(id, wait);
        id
    }

    pub fn clear(&mut self, id: WaitId) {
        self.waits.remove(&id);
    }

    /// What the oldest wait registered from `pane` is on, if any wait is.
    pub fn waiting_on(&self, pane: &str) -> Option<&WaitOn> {
        self.waits
            .values()
            .find(|wait| wait.waiter.as_deref() == Some(pane))
            .map(|wait| &wait.on)
    }
}
