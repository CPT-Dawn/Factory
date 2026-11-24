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
    /// A lab lease, by its id.
    Lease(i64),
    /// The Claude plan's window, until a lead may dispatch again.
    Claude,
}

impl fmt::Display for WaitOn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitOn::Lead(name) => f.write_str(name),
            WaitOn::Ci(pr) => write!(f, "ci {pr}"),
            WaitOn::Review(label) => write!(f, "review {label}"),
            WaitOn::Lease(id) => write!(f, "lease {id}"),
            WaitOn::Claude => f.write_str("claude quota"),
        }
    }
}

impl WaitOn {
    /// Its kind and what it is on, as a wait is stored and listed: `("ci", "acme/shop#12")`.
    pub fn parts(&self) -> (&'static str, String) {
        match self {
            WaitOn::Lead(name) => ("lead", name.clone()),
            WaitOn::Ci(pr) => ("ci", pr.clone()),
            WaitOn::Review(label) => ("review", label.clone()),
            WaitOn::Lease(id) => ("lease", id.to_string()),
            WaitOn::Claude => ("claude", "quota".to_owned()),
        }
    }

    /// The wait [`WaitOn::parts`] gave `kind` and `target`; none for parts it never gives.
    pub fn from_parts(kind: &str, target: &str) -> Option<WaitOn> {
        match kind {
            "lead" => Some(WaitOn::Lead(target.to_owned())),
            "ci" => Some(WaitOn::Ci(target.to_owned())),
            "review" => Some(WaitOn::Review(target.to_owned())),
            "lease" => target.parse().ok().map(WaitOn::Lease),
            "claude" => Some(WaitOn::Claude),
            _ => None,
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

    /// Points the wait `id` at `on` instead, as when the pull request a wait named is found.
    pub fn retarget(&mut self, id: WaitId, on: WaitOn) {
        if let Some(wait) = self.waits.get_mut(&id) {
            wait.on = on;
        }
    }

    pub fn clear(&mut self, id: WaitId) {
        self.waits.remove(&id);
    }

    /// Every registered wait, oldest first.
    pub fn all(&self) -> impl Iterator<Item = &Wait> {
        self.waits.values()
    }

    /// What the oldest wait registered from `pane` is on, if any wait is.
    pub fn waiting_on(&self, pane: &str) -> Option<&WaitOn> {
        self.waits
            .values()
            .find(|wait| wait.waiter.as_deref() == Some(pane))
            .map(|wait| &wait.on)
    }
}
