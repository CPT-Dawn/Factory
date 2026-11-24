//! The lab's daemon methods and what they carry. The CLI reads configs, finds the owner and runs
//! stacks; the daemon keeps the lease store and decides grants.

use factory_core::lab::devices::{Held, Reclaimed, lost, reclaimed_lost};
use factory_core::lab::queue::{Holding, QueueEntry};
use serde::{Deserialize, Serialize};

/// Waits in the queue until a lease can be granted: [`AcquireParams`] in, its place in the
/// queue (`{"id", "queued"}`), notes while it waits (`{"id", "note"}`), then [`Granted`] or an
/// error. The queue outlives the daemon: a caller that lost it asks again with `resume`. A
/// caller sends [`RECEIVED`] on the same connection once it has its grant.
///
/// For `devlab run` ([`AcquireParams::run`]) the connection then follows the run: the client
/// sends [`RUN_START`] when its command starts and [`RUN_END`] when the run ends, and the
/// daemon writes one `lab-run` ledger event. A run that closes the connection without a
/// [`RUN_END`] is `lost`.
pub const ACQUIRE: &str = "lab.acquire";
/// The caller of [`ACQUIRE`] has its grant: no params, `true` out once the daemon let its place
/// go. Until then a daemon that restarts hands the caller that grant again, and gives it back
/// once the caller exits.
pub const RECEIVED: &str = "lab.received";
/// A notification on a run's connection, unanswered: its command started. No params.
pub const RUN_START: &str = "lab.run.start";
/// How a run ended, [`RunEnd`], on a run's connection. Sent while queued, it takes the run out
/// of the queue, and the acquire is answered with an error. Sent as [`RUN_END_ID`] once the run
/// has its grant, it is answered `true` once recorded; sent as a notification, it is not.
pub const RUN_END: &str = "lab.run.end";
/// A notification on a run's connection, unanswered: a signal stopped devlab while its command
/// runs, [`RunEnd`] saying how. The command, still running, keeps the run's lease until
/// [`RUN_END`] says it is gone; a connection that closes before then is recorded with this end,
/// not `lost`.
pub const RUN_STOPPING: &str = "lab.run.stopping";
/// Picks up the run of a devlab process whose daemon went away: [`RunResume`] in, `true` out
/// once the daemon follows it on this connection, as it did on the acquire's. A run the daemon
/// doesn't keep, or that another connection follows, is refused.
pub const RUN_RESUME: &str = "lab.run.resume";
/// The id a run sends [`RUN_END`] with: the daemon answers it, `true`, once the end is in the
/// ledger.
pub const RUN_END_ID: u64 = u64::MAX;
/// The caller's lease, touched: [`FindParams`] in, `Option<Lease>` out.
pub const LEASE: &str = "lab.lease";
/// The caller's lease for `devlab exec`, touched: [`ExecParams`] in, `Option<Lease>` out. A lease
/// none of whose devices is lost counts the exec as its running command until [`EXEC_END`], or
/// until the exec's process exits, so no waiter takes its devices meanwhile.
pub const EXEC: &str = "lab.exec";
/// The exec [`EXEC`] counted has ended: [`ExecEnd`] in, null out. Its lease is touched.
pub const EXEC_END: &str = "lab.exec-end";
/// Drops the caller's lease, or all of them: [`ReleaseParams`] in, what it dropped and what the
/// caller still holds ([`Released`]) out. A lease named by id that another owner holds is refused.
pub const RELEASE: &str = "lab.release-leases";
/// Marks the caller's slot as having its stack stopped, so it stays held whatever happens to
/// the lease until the stopper's release: [`StopParams`] in, the caller's lease out. Named
/// with [`RELEASE`], so a daemon that lacks that release refuses this before a stack stops.
pub const STOPPING: &str = "lab.stopping-lease";
/// Drops the leases of owners that exited: no params, out a line per lease and reservation
/// this call freed (the daemon frees them as their owners exit, and says nothing of those).
pub const GC: &str = "lab.gc";
/// The whole board: no params, [`Board`] out.
pub const STATUS: &str = "lab.status";
/// Records whether devlab started a slot's stack: [`SlotParams`] in, null out.
pub const SLOT: &str = "lab.slot";
/// Sets slots aside for the caller's implementers: [`ReserveParams`] in, one note when it starts
/// to wait (`{"id", "note"}`), then [`Reserved`] or an error. An owner with no room under its
/// cap is refused at once with exit 75 and an ETA.
pub const RESERVE: &str = "lab.reserve";
/// Gives the caller's reservations of a project back: [`UnreserveParams`] in, how many out.
pub const UNRESERVE: &str = "lab.unreserve";

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
    /// The devices to lease with the slot; none for `--no-device`.
    #[serde(default)]
    pub devices: Option<DeviceAsk>,
    /// The place in the queue the daemon gave this request before the caller lost it.
    #[serde(default)]
    pub resume: Option<i64>,
    /// The devlab call that asks: its place goes once it exits.
    #[serde(default)]
    pub client: Option<Process>,
    /// The acquire is a `devlab run`'s: the daemon records it in the ledger.
    #[serde(default)]
    pub run: Option<RunAsk>,
    /// `DEVLAB_RESERVATION`: a reservation of the caller's project to turn into this lease.
    #[serde(default)]
    pub reservation: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReserveParams {
    /// The worktree of the lead that reserves.
    pub worktree: String,
    pub owner: Owner,
    pub project: ProjectAsk,
    /// How many slots it wants.
    pub slots: u32,
    /// How long to queue for the first slot; 0 gives up at once.
    pub wait_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreserveParams {
    pub owner: Owner,
    pub project: String,
}

/// A slot set aside for one of an owner's implementers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: i64,
    pub project: String,
    pub slot: u32,
    /// The lead's worktree.
    pub worktree: String,
    pub owner_pid: i64,
    pub owner_start: String,
    pub owner_label: String,
    /// Seconds since the epoch.
    pub created: i64,
    /// When a run last gave its slot back, in seconds since the epoch; none before that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<i64>,
}

impl Reservation {
    pub fn owner_key(&self) -> String {
        format!("{}:{}", self.owner_pid, self.owner_start)
    }

    /// The reservation as the planner sees it: it holds a slot like a lease.
    pub fn holding(&self) -> Holding {
        Holding {
            // Never a lease id (those start at 1), and no worktree: no waiter is served by it.
            id: 0,
            project: Some(self.project.clone()),
            slot: Some(self.slot),
            devices: 0,
            worktree: String::new(),
            owner: self.owner_key(),
            owner_label: self.owner_label.clone(),
        }
    }
}

/// What a reserve was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reserved {
    pub project: String,
    pub requested: u32,
    /// The owner's cap on the project.
    pub cap: u32,
    pub reservations: Vec<Reservation>,
}

/// The devices a request asks for, and the machine settings their grant and boot follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAsk {
    pub want: u32,
    /// `any`, `phone` or `emulator`.
    pub mode: String,
    pub max_emulators: u32,
    pub emulator_ram_mb: i64,
    pub host_floor_mb: i64,
    pub port_base: u32,
    pub image: String,
    pub device: String,
    pub gpu: String,
    pub stay_awake: bool,
    pub animations_off: bool,
    /// `maxDevicesPerOwner` as the configs set it; none: all phones but one.
    #[serde(default)]
    pub max_per_owner: Option<u32>,
}

/// A device a lease holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseDevice {
    pub serial: String,
    /// `phone` or `emulator`.
    pub kind: String,
    pub abi: Option<String>,
    pub model: String,
    /// What adb says of it now (`device`, `offline`, ...); none when adb doesn't list it.
    #[serde(default)]
    pub state: Option<String>,
}

/// The run an acquire is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAsk {
    /// The command line, its words joined by spaces.
    pub command: String,
    /// The Claude Code session it runs in, `CLAUDE_CODE_SESSION_ID`.
    pub session: Option<String>,
    /// `--keep`: a lease the run makes outlives it. Without it, that lease ends with the run.
    #[serde(default)]
    pub keep: bool,
}

/// How a run ended, in the ledger's words: [`factory_core::lab::run::End::outcome`] and
/// [`factory_core::lab::run::Phase::word`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnd {
    pub outcome: String,
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindParams {
    pub worktree: String,
    pub owner: Owner,
    pub lease: Option<i64>,
}

/// `devlab exec`'s lease, and the command it runs on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecParams {
    #[serde(flatten)]
    pub find: FindParams,
    /// The devlab exec process.
    pub client: Process,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecEnd {
    pub client: Process,
}

/// The devlab process whose run to pick up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResume {
    pub client: Process,
}

/// One process by pid and start time: the devlab call that stops a stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: i64,
    /// Field 22 of `/proc/<pid>/stat`.
    pub start: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopParams {
    #[serde(flatten)]
    pub find: FindParams,
    pub stopper: Process,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseParams {
    #[serde(flatten)]
    pub find: FindParams,
    /// The stopper whose marks [`STOPPING`] set: they go with the lease.
    #[serde(default)]
    pub stopper: Option<Process>,
    /// Every lease of the caller's owner goes, not one.
    #[serde(default)]
    pub all: bool,
}

/// The lease a `release --stop-stack` gives back, its slot held for the stop meanwhile, and the
/// stack it was granted (its Supabase project id): the one stopped, whatever the worktree's
/// config names now (#63). None from a daemon that kept none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stopping {
    #[serde(flatten)]
    pub lease: Lease,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// What a release did: the leases it dropped, and the caller's leases still held after it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Released {
    pub dropped: Vec<Lease>,
    pub still_held: Vec<Lease>,
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
    /// In the order they were granted.
    #[serde(default)]
    pub devices: Vec<LeaseDevice>,
    /// The devices a waiter took from it while it ran nothing (#57).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reclaimed: Vec<ReclaimedDevice>,
}

/// A device taken from a lease for a waiter, after the lease ran nothing for `idle_min` minutes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimedDevice {
    pub serial: String,
    pub kind: String,
    pub model: String,
    pub idle_min: i64,
}

impl Lease {
    /// Why the lease can't be used because a waiter took one of its devices, or a device it
    /// holds is gone or not usable, by the states its devices were filled with; none when
    /// nothing was taken and each is attached as `device`.
    pub fn lost(&self) -> Option<String> {
        let taken: Vec<Reclaimed> = self
            .reclaimed
            .iter()
            .map(|d| Reclaimed {
                serial: d.serial.clone(),
                kind: d.kind.clone(),
                model: d.model.clone(),
                idle_min: d.idle_min,
            })
            .collect();
        if let Some(why) = reclaimed_lost(&taken) {
            return Some(why);
        }
        let held: Vec<Held> = self
            .devices
            .iter()
            .map(|d| Held {
                serial: d.serial.clone(),
                kind: d.kind.clone(),
                model: d.model.clone(),
                state: d.state.clone(),
            })
            .collect();
        lost(&held)
    }

    pub fn owner_key(&self) -> String {
        format!("{}:{}", self.owner_pid, self.owner_start)
    }

    /// The lease as the planner sees it.
    pub fn holding(&self) -> Holding {
        Holding {
            id: self.id,
            project: self.project.clone(),
            slot: self.slot,
            devices: u32::try_from(self.devices.len()).unwrap_or(u32::MAX),
            worktree: self.worktree.clone(),
            owner: self.owner_key(),
            owner_label: self.owner_label.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Granted {
    pub lease: Lease,
    /// The worktree the slot ran before this grant, if it ran one.
    pub previous: Option<String>,
    /// The schema the slot's stack last ran, when the daemon has a record of it.
    #[serde(default)]
    pub schema: Option<SlotSchema>,
}

/// The schema a slot's stack last ran, whether or not it runs now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotSchema {
    /// The worktree whose lease last started the slot's stack.
    pub worktree: String,
    /// That worktree's newest migration when the record was made, if it had one.
    pub head: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRow {
    pub project: String,
    pub slot: u32,
    pub last_worktree: Option<String>,
    /// devlab started its stack, so the idle timer may stop it (#18).
    pub stack_ours: bool,
    pub idle_since: Option<i64>,
    /// A `release --stop-stack` is stopping its stack: the slot is held until it is done.
    #[serde(default)]
    pub stopping: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiter {
    pub id: i64,
    pub created: i64,
    /// `<owner label> @ <worktree name>`.
    pub label: String,
    pub owner: String,
    pub owner_label: String,
    pub worktree: String,
    /// `DEVLAB_LEASE`, when the request names its lease.
    pub lease: Option<i64>,
    pub project: Option<String>,
    /// The owner's cap in its project; the planner sets it aside while a lease serves it.
    pub slot_cap: Option<u32>,
    pub max_slots: u32,
    /// When the request gives up, in milliseconds since the epoch; none waits as long as it takes.
    pub deadline_ms: Option<i64>,
    /// The devlab call that waits, by pid and start time.
    pub client_pid: Option<i64>,
    pub client_start: Option<String>,
}

impl Waiter {
    /// The waiter as the planner sees it.
    pub fn entry(&self) -> QueueEntry {
        QueueEntry {
            id: self.id,
            label: self.label.clone(),
            created: self.created,
            owner: self.owner.clone(),
            owner_label: self.owner_label.clone(),
            worktree: self.worktree.clone(),
            lease: self.lease,
            project: self.project.clone(),
            cap: self.slot_cap,
            max_slots: self.max_slots,
            // What it asks of devices is kept apart, in `activity`.
            devices: None,
        }
    }
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
    /// By id.
    #[serde(default)]
    pub reservations: Vec<Reservation>,
    /// Every device adb lists, a lease holds or devlab runs.
    #[serde(default)]
    pub devices: Vec<DeviceLine>,
    /// The command each lease runs, from `devlab run` or `devlab exec`; a lease that runs
    /// nothing has none.
    #[serde(default)]
    pub running: Vec<LeaseRunning>,
}

/// What lease `lease` runs now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRunning {
    pub lease: i64,
    pub command: String,
}

/// One device as status shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLine {
    pub serial: String,
    pub kind: String,
    pub model: String,
    /// `booting`, `ready`, adb's state, or `gone`.
    pub state: String,
    /// `lease N <worktree> (<owner>)`, `idle 3m` or `free`.
    pub holder: String,
    /// A phone's readiness as the daemon last probed it (#55): `ready`, `locked`, `asleep`,
    /// …; none for an emulator or a phone not yet probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotParams {
    pub project: String,
    pub slot: u32,
    pub stack_ours: bool,
}
