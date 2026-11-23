//! The lab's daemon methods and what they carry. The CLI reads configs, finds the owner and runs
//! stacks; the daemon keeps the lease store and decides grants.

use serde::{Deserialize, Serialize};

/// Waits in the queue until a lease can be granted: [`AcquireParams`] in, notes while it waits
/// (`{"id", "note"}`), then [`Granted`] or an error.
pub const ACQUIRE: &str = "lab.acquire";
/// The caller's lease, touched: [`FindParams`] in, `Option<Lease>` out.
pub const LEASE: &str = "lab.lease";
/// Drops the caller's lease: [`FindParams`] in, the lease it dropped (`Option<Lease>`) out.
pub const RELEASE: &str = "lab.release";
/// Drops the leases of owners that exited: no params, a line per lease out.
pub const GC: &str = "lab.gc";
/// The whole board: no params, [`Board`] out.
pub const STATUS: &str = "lab.status";
/// Records whether devlab started a slot's stack: [`SlotParams`] in, null out.
pub const SLOT: &str = "lab.slot";

/// Who a lease belongs to: a process by pid and start time (pids get reused).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: i64,
    /// Field 22 of `/proc/<pid>/stat`.
    pub start: String,
    /// `<comm>:<pid>`.
    pub label: String,
    /// It is a claude or codex ancestor; else the caller's session leader stands in.
    pub found: bool,
}

impl Owner {
    pub fn key(&self) -> String {
        format!("{}:{}", self.pid, self.start)
    }
}

/// The caller's project, as the grant needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAsk {
    pub name: String,
    pub max_slots: u32,
    pub stride: u32,
    /// The per-owner cap on this machine.
    pub cap: u32,
    /// The project's own slot-0 ports.
    pub ports: Vec<u32>,
    pub stack: Option<StackAsk>,
}

/// The project's local stack in slot 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackAsk {
    pub project_id: String,
    pub ports: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcquireParams {
    pub worktree: String,
    pub owner: Owner,
    /// `DEVLAB_LEASE`: the lease to find instead of this worktree and owner's.
    pub lease: Option<i64>,
    pub project: Option<ProjectAsk>,
    /// How long to queue; 0 gives up at once.
    pub wait_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindParams {
    pub worktree: String,
    pub owner: Owner,
    pub lease: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: i64,
    pub project: Option<String>,
    pub worktree: String,
    pub owner_pid: i64,
    pub owner_start: String,
    pub owner_label: String,
    pub slot: Option<u32>,
    /// Seconds since the epoch.
    pub created: i64,
    pub touched: i64,
}

impl Lease {
    pub fn owner_key(&self) -> String {
        format!("{}:{}", self.owner_pid, self.owner_start)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Granted {
    pub lease: Lease,
    /// The worktree the slot ran before this grant, if it ran one.
    pub previous: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRow {
    pub project: String,
    pub slot: u32,
    pub last_worktree: Option<String>,
    /// devlab started its stack, so the idle timer may stop it (#18).
    pub stack_ours: bool,
    pub idle_since: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiter {
    pub id: i64,
    pub created: i64,
    /// `<owner label> @ <worktree name>`.
    pub label: String,
    pub owner: String,
    pub owner_label: String,
    pub project: Option<String>,
    pub slot_cap: Option<u32>,
    pub max_slots: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// The daemon's clock, seconds since the epoch.
    pub now: i64,
    /// By id.
    pub leases: Vec<Lease>,
    /// By project, then slot.
    pub slots: Vec<SlotRow>,
    /// Oldest first.
    pub waiters: Vec<Waiter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotParams {
    pub project: String,
    pub slot: u32,
    pub stack_ours: bool,
}
