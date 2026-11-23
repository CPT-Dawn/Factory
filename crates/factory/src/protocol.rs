//! The daemon protocol: newline-delimited JSON over the unix socket, the shape herdr uses.
//!
//! A client writes one request per line, `{"id", "method", "params"}`, and reads one response
//! line per request, `{"id", "result"}` or `{"id", "error": {"code", "message"}}`, where
//! `code` is an exit code from [`factory_core::exit`].

use factory_core::event::Fields;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Appends one event to the ledger: [`LogParams`] in, [`LogResult`] out.
pub const LOG: &str = "log";
/// Every lead herdr knows: no params, [`LeadList`] out.
pub const LEAD_LIST: &str = "lead.list";
/// One lead: [`LeadParams`] in, [`LeadReport`] out.
pub const LEAD_STATUS: &str = "lead.status";
/// Answered once a lead finishes, blocks, asks or dies: [`WaitLeadParams`] in,
/// [`WaitLeadResult`] out; an error with exit 124 when the time runs out first.
pub const WAIT_LEAD: &str = "wait.lead";

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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: u8,
    pub message: String,
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
pub struct LeadParams {
    /// The lead's herdr agent name, `lead-<slug>`.
    pub lead: String,
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
