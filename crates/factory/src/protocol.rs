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
/// Takes a slot on the first review backend with quota and room, waiting when the router says
/// to: [`AcquireParams`] in, [`AcquireResult`] out. The connection holds the slot until it
/// asks for another or closes.
pub const REVIEW_ACQUIRE: &str = "review.acquire";
/// Records what a review learned of a backend's quota: [`QuotaParams`] in, the same out.
pub const REVIEW_QUOTA: &str = "review.quota";

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
pub struct AcquireParams {
    /// The configured backends, in the order they are tried.
    pub backends: Vec<BackendSlots>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackendSlots {
    pub name: String,
    pub concurrency: u32,
    /// `queue` or `next`.
    pub when_busy: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcquireResult {
    /// The backend whose slot this connection now holds.
    pub backend: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QuotaParams {
    pub backend: String,
    /// `available` or `cooling`.
    pub quota: String,
    /// When a cooling backend resets, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}
