//! The daemon protocol: newline-delimited JSON over the unix socket, the shape herdr uses.
//!
//! A client writes one request per line, `{"id", "method", "params"}`, and reads one response
//! line per request, `{"id", "result"}` or `{"id", "error": {"code", "message"}}`, where
//! `code` is an exit code from [`factory_core::exit`]. A request that waits (`lab.acquire`) may
//! send `{"id", "note"}` and `{"id", "queued"}` lines before its answer.

use factory_core::event::Fields;
use factory_core::exit::Exit;
use factory_core::quota::{Quota, WhenBusy};
use factory_core::timestamp::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What to do when the CLI and the daemon don't speak the same protocol.
pub const MISMATCH: &str = "the daemon may be older than this factory: restart it";

/// Appends one event to the ledger: [`LogParams`] in, [`LogResult`] out.
pub const LOG: &str = "log";
/// Takes a slot on the first review backend with quota and room, waiting when the router says
/// to: [`AcquireParams`] in, [`AcquireResult`] out. The connection holds the slot until it
/// asks for another or closes.
pub const REVIEW_ACQUIRE: &str = "review.acquire";
/// Records what a review learned of a backend's quota: [`QuotaParams`] in, the same out.
pub const REVIEW_QUOTA: &str = "review.quota";
/// Every lead herdr knows: no params, [`LeadList`] out.
pub const LEAD_LIST: &str = "lead.list";
/// One lead: [`LeadParams`] in, [`LeadReport`] out.
pub const LEAD_STATUS: &str = "lead.status";
/// Starts a lead in a new herdr workspace: [`SpawnParams`] in, [`SpawnResult`] out.
pub const LEAD_SPAWN: &str = "lead.spawn";
/// Answered once a lead finishes, blocks, asks or dies: [`WaitLeadParams`] in,
/// [`WaitLeadResult`] out; an error with exit 124 when the time runs out first.
pub const WAIT_LEAD: &str = "wait.lead";
/// Answered once every check on a pull request's head has finished: [`WaitCiParams`] in,
/// [`WaitCiResult`] out; an error with exit 124 when the time runs out first.
pub const WAIT_CI: &str = "wait.ci";

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// The request's id; none when the request couldn't be read.
    pub id: Option<u64>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Result(Value),
    Error(ErrorBody),
    /// Progress before the result, `{"id", "note"}`: a request that waits may send any number.
    Note(String),
    /// `{"id", "queued": n}`: a request that waits in a queue the daemon keeps across restarts
    /// (`lab.acquire`) holds place `n`, which it names to keep it when it asks again.
    Queued(i64),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: u8,
    pub message: String,
}

impl Outcome {
    /// A refusal with `exit`'s code and a one-line `message`.
    pub fn refused(exit: Exit, message: String) -> Outcome {
        Outcome::Error(ErrorBody {
            code: exit.code(),
            message,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogParams {
    #[serde(rename = "type")]
    pub kind: String,
    pub fields: Fields,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogResult {
    /// The ledger's path, lossily UTF-8 so any path can be answered.
    pub ledger: String,
    /// The line as it landed.
    pub event: Fields,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcquireParams {
    /// The configured backends, in the order they are tried.
    pub backends: Vec<BackendSlots>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackendSlots {
    pub name: String,
    pub concurrency: u32,
    pub when_busy: WhenBusyParam,
}

/// [`WhenBusy`] on the wire: `queue` or `next`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenBusyParam {
    Queue,
    Next,
}

impl From<WhenBusy> for WhenBusyParam {
    fn from(when_busy: WhenBusy) -> WhenBusyParam {
        match when_busy {
            WhenBusy::Queue => WhenBusyParam::Queue,
            WhenBusy::Next => WhenBusyParam::Next,
        }
    }
}

impl From<WhenBusyParam> for WhenBusy {
    fn from(when_busy: WhenBusyParam) -> WhenBusy {
        match when_busy {
            WhenBusyParam::Queue => WhenBusy::Queue,
            WhenBusyParam::Next => WhenBusy::Next,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcquireResult {
    /// The backend whose slot this connection now holds.
    pub backend: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QuotaParams {
    pub backend: String,
    #[serde(flatten)]
    pub quota: QuotaParam,
}

/// What a review learned of a backend's quota: `{"quota": "available"}`, or
/// `{"quota": "cooling", "until": <milliseconds since the epoch>}`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "quota", rename_all = "lowercase")]
pub enum QuotaParam {
    Available,
    Cooling { until: i64 },
}

impl From<QuotaParam> for Quota {
    fn from(quota: QuotaParam) -> Quota {
        match quota {
            QuotaParam::Available => Quota::Available,
            QuotaParam::Cooling { until } => Quota::CoolingDown {
                until: Timestamp::from_unix_millis(until),
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LeadParams {
    /// The lead's herdr agent name, `lead-<slug>`.
    pub lead: String,
}

/// What the daemon needs to start a lead, checked by the CLI against the repo already.
#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnParams {
    /// The lead's herdr agent name, `lead-<slug>`.
    pub lead: String,
    /// The repository to cut the lead's worktree from.
    pub repo_path: String,
    pub repo_name: String,
    pub base: String,
    pub skill: String,
    pub spec: String,
    /// What Claude starts with, after its own name.
    pub claude_args: Vec<String>,
    pub model: String,
    pub effort: String,
    pub retry_pause_ms: u64,
    /// The Claude Code session that asked, for the ledger.
    pub session: Option<String>,
}

/// The lead a spawn started, as the night page takes it.
#[derive(Debug, Serialize, Deserialize)]
pub struct SpawnResult {
    pub lead: String,
    pub workspace: String,
    pub branch: String,
    pub worktree: String,
    pub report: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitLeadParams {
    pub lead: String,
    /// The herdr pane the wait comes from, so the lead in it shows as waiting.
    pub waiter: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LeadList {
    pub leads: Vec<LeadReport>,
}

/// What the daemon knows of one lead.
#[derive(Debug, Serialize, Deserialize)]
pub struct LeadReport {
    pub lead: String,
    /// One word: working, waiting, blocked, asking, finished or dead.
    pub state: String,
    /// The state as the line shows it, such as `waiting: lead-b`.
    pub status: String,
    pub pane: String,
    /// Where the lead writes its final report, when its repo is known.
    pub report: Option<String>,
    /// The pane's last lines, when the lead is blocked or asking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitLeadResult {
    #[serde(flatten)]
    pub lead: LeadReport,
    /// 0 when the lead finished, 10 when it needs attention.
    pub exit: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitCiParams {
    /// The pull request as the caller named it: a number, a URL or a branch.
    pub pr: String,
    /// The caller's working directory, where `gh` finds the repo a number belongs to.
    pub cwd: String,
    /// The herdr pane the wait comes from, so the lead in it shows as waiting.
    pub waiter: Option<String>,
    pub timeout_secs: u64,
    /// How often to poll GitHub when it can't listen for events; the first wait on a pull
    /// request sets it for all.
    pub poll_secs: u64,
}

/// A pull request whose checks all finished.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitCiResult {
    /// `<owner>/<repo>#<number>`.
    pub pr: String,
    pub url: String,
    /// The head commit the checks ran on.
    pub head: String,
    pub passed: usize,
    pub skipped: usize,
    /// The failed checks' names.
    pub failed: Vec<String>,
    /// The file holding each failure's URL and log tail; none when nothing failed.
    pub file: Option<String>,
    /// 0 when nothing failed, 10 when something did.
    pub exit: u8,
}
