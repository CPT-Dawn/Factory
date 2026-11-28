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
/// Answered once the review of a label has ended: [`WaitReviewParams`] in, the `review` event
/// as it landed in the ledger out; an error with exit 124 when the time runs out first.
pub const WAIT_REVIEW: &str = "wait.review";
/// Answered once a lab lease is held no more (released, its owner exited, or expired):
/// [`WaitLeaseParams`] in, [`WaitLeaseResult`] out; an error with exit 4 for an id never
/// granted, and with exit 124 when the time runs out first.
pub const WAIT_LEASE: &str = "wait.lease";
/// Registers a wait that outlives its CLI (`--notify`) and answers at once: [`NotifyParams`]
/// in, [`NotifyResult`] out. When the wait ends, the daemon posts one line into the session
/// the params address; a refusal with exit 3 when event delivery is switched off.
pub const WAIT_NOTIFY: &str = "wait.notify";
/// Starts the context steward unless it runs: no params, [`MonitorSwitch`] out.
pub const MONITOR_START: &str = "monitor.start";
/// Stops the context steward if it runs: no params, [`MonitorSwitch`] out.
pub const MONITOR_STOP: &str = "monitor.stop";
/// Whether the context steward runs, and what it watches: no params, [`MonitorStatus`] out.
pub const MONITOR_STATUS: &str = "monitor.status";

/// Records the Claude plan's usage windows a status line saw: [`ClaudeWindowParams`] in,
/// nothing out.
pub const CLAUDE_WINDOW: &str = "claude.window";
/// Records that a turn ended on the plan's limit: [`ClaudeLimitParams`] in, [`ClaudeVerdict`]
/// out.
pub const CLAUDE_LIMIT: &str = "claude.limit";
/// Whether a lead may dispatch now: [`ClaudeQuotaParams`] in, [`ClaudeVerdict`] out.
pub const CLAUDE_QUOTA: &str = "claude.quota";
/// Whether a review may dispatch on Codex now, asking `codex app-server` when its reading is
/// older than 5 minutes: [`CodexQuotaParams`] in, [`CodexVerdict`] out.
pub const CODEX_QUOTA: &str = "codex.quota";

/// The usage-window history, oldest first: [`HistoryParams`] in, [`HistoryRows`] out.
pub const QUOTA_HISTORY: &str = "quota.history";
/// agy's quota groups and their windows, from the keeper's reading of its usage query: no
/// params in, [`factory_core::status::Agy`] out. It waits for a query that is under way.
pub const AGY_QUOTA: &str = "agy.quota";
/// Answered once a lead may dispatch: [`WaitClaudeParams`] in, [`ClaudeVerdict`] out; an error
/// with exit 124 when the time runs out first.
pub const WAIT_CLAUDE: &str = "wait.claude";

/// What the daemon holds, as one record, read from its own state; it runs no command but, when
/// agy's reading is stale, the one usage query the quota keeper makes in the background:
/// [`StatusParams`] in, [`factory_core::status::Status`] out.
pub const STATUS: &str = "status";
/// Everything the bar shows, in one answer: [`BarParams`] in,
/// [`factory_core::bar::Snapshot`] out.
pub const BAR: &str = "bar";

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
    /// (`lab.acquire`) holds place `n`, which it names to keep it when it asks again; a
    /// blocking wait (`wait.lead`, `wait.ci`, `wait.review`, `wait.lease`, `wait.claude`) is
    /// kept as wait `n`, which it names as `resume` to carry on with it.
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
pub struct StatusParams {
    /// The configured review backends, in the order they are tried: the daemon holds quotas, the
    /// config says which backends there are.
    pub backends: Vec<BackendSlots>,
    /// The config's mark for holding Claude dispatches, in percent of either window.
    #[serde(default = "hold_at_default")]
    pub claude_hold_at: f64,
    /// The config's say on Codex: its hold mark, and how long its app-server may take.
    #[serde(default)]
    pub codex: CodexParams,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryParams {
    /// The oldest reading wanted, in milliseconds since the epoch; none asks for all.
    #[serde(default)]
    pub since: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryRows {
    pub rows: Vec<factory_core::history::HistoryRow>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BarParams {
    /// The config's mark for holding Claude dispatches, in percent of either window.
    #[serde(default = "hold_at_default")]
    pub claude_hold_at: f64,
    /// The config's say on Codex: its hold mark, and how long its app-server may take.
    #[serde(default)]
    pub codex: CodexParams,
    /// The config's mark for holding a quota group of agy's, in percent of either window.
    #[serde(default = "agy_hold_at_default")]
    pub agy_hold_at: f64,
}

fn hold_at_default() -> f64 {
    factory_core::claude::HOLD_AT_PERCENT
}

fn agy_hold_at_default() -> f64 {
    factory_core::quota::AGY_HOLD_AT_PERCENT
}

/// What the config says of Codex (`[codex]`): the daemon holds the quota, the config holds the
/// marks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexParams {
    /// Hold reviews once this share of either window is spent, in percent.
    #[serde(default = "codex_hold_at_default")]
    pub hold_at: f64,
    /// How long `codex app-server` may take to answer, in seconds, before it is killed.
    #[serde(default = "codex_timeout_default")]
    pub timeout_secs: u64,
}

impl Default for CodexParams {
    fn default() -> Self {
        CodexParams {
            hold_at: codex_hold_at_default(),
            timeout_secs: codex_timeout_default(),
        }
    }
}

fn codex_hold_at_default() -> f64 {
    factory_core::codex::HOLD_AT_PERCENT
}

fn codex_timeout_default() -> u64 {
    factory_adapters::codex::TIMEOUT.as_secs()
}

/// The windows a status line reported; one it left out keeps its last reading.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeWindowParams {
    #[serde(default)]
    pub five_hour: Option<WindowReading>,
    #[serde(default)]
    pub seven_day: Option<WindowReading>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WindowReading {
    pub used_percent: f64,
    /// When the window resets, in milliseconds since the epoch.
    pub resets: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeLimitParams {
    /// The reset the limit's message names, in milliseconds since the epoch; none when it names
    /// none.
    pub reset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodexQuotaParams {
    #[serde(default)]
    pub codex: CodexParams,
}

/// Whether a review may dispatch on Codex: `unknown`, `go` or `hold`, with the windows, the plan
/// and what holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct CodexVerdict {
    #[serde(flatten)]
    pub codex: factory_core::status::Codex,
    pub hold_at: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeQuotaParams {
    /// Hold once this share of either window is spent, in percent.
    pub hold_at: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitClaudeParams {
    pub hold_at: f64,
    /// The herdr pane the wait comes from, so the lead in it shows as waiting.
    pub waiter: Option<String>,
    pub timeout_secs: u64,
    /// The wait an earlier daemon held for this CLI, by the id its `queued` line gave: the
    /// wait carries on with what is left of its time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<i64>,
}

/// Whether a lead may dispatch: `unknown`, `go` or `hold`, with the windows and what holds.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeVerdict {
    #[serde(flatten)]
    pub claude: factory_core::status::Claude,
    pub hold_at: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcquireParams {
    /// The configured backends, in the order they are tried.
    pub backends: Vec<BackendSlots>,
    /// The review asking, so the daemon can tell a wait on its label that it is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewRef>,
    /// The review's tier, which `factory status` names while it holds a slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// The config's say on Codex, which holds the backends that run on it.
    #[serde(default)]
    pub codex: CodexParams,
}

/// A review by the repo it runs in and its label.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewRef {
    pub repo: String,
    pub label: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackendSlots {
    pub name: String,
    /// The CLI it runs on (`agy`, `codex`, `claude`); a backend of the CLI Codex is held by
    /// the Codex plan's windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
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

/// What a review learned of a backend's quota: `{"quota": "available"}`,
/// `{"quota": "cooling", "until": <milliseconds since the epoch>}`, or
/// `{"quota": "unknown", "reason": <why>}` when it could not be read.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "quota", rename_all = "lowercase")]
pub enum QuotaParam {
    Available,
    Cooling { until: i64 },
    Unknown { reason: String },
}

impl From<&QuotaParam> for Quota {
    fn from(quota: &QuotaParam) -> Quota {
        match *quota {
            QuotaParam::Available => Quota::Available,
            QuotaParam::Cooling { until } => Quota::CoolingDown {
                until: Timestamp::from_unix_millis(until),
            },
            QuotaParam::Unknown { .. } => Quota::Unknown,
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
    /// Whether a person is at the lead's pane, so its prompt does not say it runs unattended.
    #[serde(default)]
    pub attended: bool,
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
    /// The wait an earlier daemon held for this CLI, by the id its `queued` line gave: the
    /// wait carries on with what is left of its time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<i64>,
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
pub struct WaitReviewParams {
    pub label: String,
    /// The repo the caller works in; none when its working directory is no repo, which waits
    /// for the label in any.
    pub repo: Option<String>,
    /// The herdr pane the wait comes from, so the lead in it shows as waiting.
    pub waiter: Option<String>,
    pub timeout_secs: u64,
    /// The wait an earlier daemon held for this CLI, by the id its `queued` line gave: the
    /// wait carries on with what is left of its time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitLeaseParams {
    pub lease: i64,
    /// The herdr pane the wait comes from, so the lead in it shows as waiting.
    pub waiter: Option<String>,
    pub timeout_secs: u64,
    /// The wait an earlier daemon held for this CLI, by the id its `queued` line gave: the
    /// wait carries on with what is left of its time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitLeaseResult {
    pub lease: i64,
    /// Always false: the wait is answered once the lease is held no more.
    pub held: bool,
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
    /// The wait an earlier daemon held for this CLI, by the id its `queued` line gave: the
    /// wait carries on with what is left of its time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NotifyParams {
    pub on: NotifyOn,
    /// The herdr pane the wait comes from, so the lead in it shows as waiting.
    pub waiter: Option<String>,
    pub timeout_secs: u64,
    pub session: SessionParam,
}

/// What a `--notify` wait waits on.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NotifyOn {
    /// A pull request's checks, as [`WaitCiParams`] names them.
    Ci {
        pr: String,
        cwd: String,
        poll_secs: u64,
    },
    /// A lead, by its herdr agent name.
    Lead { lead: String },
}

/// The Claude Code session a `--notify` wait posts into when it ends.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionParam {
    /// Its inbox socket, `CLAUDE_CODE_MESSAGING_SOCKET`.
    pub socket: String,
    /// The token its inbox takes, `CLAUDE_CODE_MESSAGING_TOKEN`.
    pub token: String,
    /// The session's process, `CLAUDE_PID`, and its start time (field 22 of
    /// `/proc/<pid>/stat`): the daemon drops the wait when that process exits.
    pub pid: Option<i64>,
    pub start: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NotifyResult {
    /// The wait's number, which its line names when it ends.
    pub id: i64,
    /// What it waits on, as `factory lead status` shows it: `ci acme/shop#12` or a lead.
    pub on: String,
    /// When it gives up, in milliseconds since the epoch.
    pub deadline: i64,
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
    /// The cancelled checks' names: the newest run of each, so none a newer run replaced.
    #[serde(default)]
    pub cancelled: Vec<String>,
    /// The file holding each failed and cancelled check's URL and log tail; none when every
    /// check passed or was skipped.
    pub file: Option<String>,
    /// 0 when every check passed or was skipped, 10 when one failed or was cancelled.
    pub exit: u8,
}

/// What a start or stop of the context steward did.
#[derive(Debug, Serialize, Deserialize)]
pub struct MonitorSwitch {
    pub running: bool,
    /// False when the steward already ran (start) or already stood still (stop).
    pub changed: bool,
}

/// The context steward and its context table.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MonitorStatus {
    pub running: bool,
    /// Every lead it watches, by name.
    pub leads: Vec<LeadContext>,
}

/// One lead's context, and its subagents'.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeadContext {
    pub lead: String,
    /// The Claude Code session herdr reports for it.
    pub session: Option<String>,
    /// The session log the context is read from, once it is found.
    pub log: Option<String>,
    /// The latest context; none before the log has said.
    pub tokens: Option<u64>,
    /// Whether the steward has asked it to stop and not yet compacted it.
    pub asked: bool,
    /// Whether a compaction of it is under way.
    pub compacting: bool,
    pub subagents: Vec<SubagentContext>,
}

/// One subagent of a lead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentContext {
    /// Its session log's name, `agent-<id>`.
    pub agent: String,
    /// What it was started to do, when Claude Code recorded it.
    pub description: Option<String>,
    pub tokens: Option<u64>,
    /// Whether its lead has been told it passed the threshold.
    pub told: bool,
}
