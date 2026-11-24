//! The lab's events in the stats ledger (#62): what the daemon learns as it happens, one line
//! each, so factory-stats needn't piece the lab together from agents' transcripts.
//!
//! - `lab-lease`: a lease starts ([`lease_start`]) or ends ([`lease_end`]).
//! - `lab-wait`: a request is granted or gives up ([`wait_granted`], [`wait_gave_up`]).
//! - `lab-reclaim`: a device is taken from an idle lease for a waiter ([`reclaim`]).
//! - `lab-readiness`: a phone's readiness changes ([`readiness`]).
//!
//! Every field is a flat value; the numbers are all in the ledger's numeric list. No field names
//! a device serial ([`short_id`] stands in for it), a home path (a worktree is its basename) or
//! a user (the home dir's name is shown as `~`). A wait's reason is kept as its kind and counts ([`wait_reason`]), never its text.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::base_name;
use super::ready::Readiness;
use crate::event::Fields;

/// What the ledger says of a lease, from its start to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseFacts {
    pub id: i64,
    pub project: Option<String>,
    pub owner_label: String,
    /// The worktree's path; the event keeps its basename.
    pub worktree: String,
    /// The home dir, whose name (most often the user's) no event shows.
    pub home: Option<String>,
    /// `(kind, model)` of each device it holds; a model may be empty.
    pub devices: Vec<(String, String)>,
    pub slot: Option<u32>,
    pub reservation: Option<i64>,
}

/// How a lease ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseEnd {
    /// The `devlab run` it was made for ended.
    RunEnd,
    /// Its owner released it.
    Release,
    /// Its owner exited.
    OwnerExit,
    /// The keeper took it back for a waiter.
    Reclaim,
    /// Its grant never reached its caller, or failed as it was handed over.
    SettleFailed,
}

impl LeaseEnd {
    pub fn word(self) -> &'static str {
        match self {
            LeaseEnd::RunEnd => "run-end",
            LeaseEnd::Release => "release",
            LeaseEnd::OwnerExit => "owner-exit",
            LeaseEnd::Reclaim => "reclaim",
            LeaseEnd::SettleFailed => "settle-failed",
        }
    }
}

fn put(fields: &mut Fields, key: &str, value: impl Into<Value>) {
    fields.insert(key.to_owned(), value.into());
}

fn lease_fields(f: &LeaseFacts, action: &str) -> Fields {
    let mut out = Fields::new();
    put(&mut out, "action", action);
    put(&mut out, "lease", f.id);
    if let Some(project) = &f.project {
        put(&mut out, "project", project.as_str());
    }
    put(&mut out, "owner", f.owner_label.as_str());
    put(
        &mut out,
        "worktree",
        worktree_name(&f.worktree, f.home.as_deref()),
    );
    if !f.devices.is_empty() {
        let devices: Vec<String> = f
            .devices
            .iter()
            .map(|(kind, model)| {
                if model.is_empty() {
                    kind.clone()
                } else {
                    format!("{kind}:{model}")
                }
            })
            .collect();
        put(&mut out, "devices", devices.join(","));
    }
    put(&mut out, "count", f.devices.len());
    if let Some(slot) = f.slot {
        put(&mut out, "slot", slot);
    }
    if let Some(reservation) = f.reservation {
        put(&mut out, "reservation", reservation);
    }
    out
}

/// A worktree as an event names it: its basename, or `~` for the home dir and for any dir
/// named as the home dir is, which is most often the user's name.
fn worktree_name<'a>(path: &'a str, home: Option<&str>) -> &'a str {
    let name = base_name(path);
    match home.map(base_name) {
        Some(home) if !home.is_empty() && name == home => "~",
        _ => name,
    }
}

/// The `lab-lease` event of a lease granted.
pub fn lease_start(f: &LeaseFacts) -> Fields {
    lease_fields(f, "start")
}

/// The `lab-lease` event of a lease ended after `seconds`, `idle_seconds` of them with no
/// command running on it.
pub fn lease_end(f: &LeaseFacts, seconds: u64, idle_seconds: u64, reason: LeaseEnd) -> Fields {
    let mut out = lease_fields(f, "end");
    put(&mut out, "seconds", seconds);
    put(&mut out, "idle_seconds", idle_seconds);
    put(&mut out, "reason", reason.word());
    out
}

/// A lease's `(seconds, idle_seconds)` at `now_ms`: held since `start_ms`, with commands
/// having run on it `busy_ms` in all and, when `busy_since_ms` is set, one running since then.
/// All in ms since the epoch; spans are whole seconds, never negative.
pub fn spans(start_ms: i64, busy_ms: i64, busy_since_ms: Option<i64>, now_ms: i64) -> (u64, u64) {
    let held = now_ms.saturating_sub(start_ms).max(0);
    let running = busy_since_ms.map_or(0, |since| now_ms.saturating_sub(since).max(0));
    let busy = busy_ms.max(0).saturating_add(running);
    let secs = |ms: i64| u64::try_from(ms / 1000).unwrap_or(0);
    (secs(held), secs(held.saturating_sub(busy).max(0)))
}

/// What a request asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// `phone`, `emulator` or `any`; none: no device.
    pub mode: Option<String>,
    /// How many devices.
    pub count: u32,
    /// A stack slot of a project.
    pub stack: bool,
}

fn wait_fields(
    action: &str,
    waiter: i64,
    asked: &Asked,
    seconds: u64,
    why: Option<&str>,
) -> Fields {
    let mut out = Fields::new();
    put(&mut out, "action", action);
    put(&mut out, "waiter", waiter);
    put(&mut out, "asked", asked.mode.as_deref().unwrap_or("none"));
    put(&mut out, "count", asked.count);
    put(&mut out, "stack", if asked.stack { "yes" } else { "no" });
    put(&mut out, "seconds", seconds);
    out.extend(wait_reason(why.unwrap_or_default()));
    out
}

/// The `lab-wait` event of request `waiter` granted after `seconds`; `why`, the last reason
/// it waited for, if it waited. `waiter` is what a `lab-reclaim` names.
pub fn wait_granted(waiter: i64, asked: &Asked, seconds: u64, why: Option<&str>) -> Fields {
    wait_fields("granted", waiter, asked, seconds, why)
}

/// The `lab-wait` event of request `waiter` that gave up after `seconds`, waiting for `why`.
pub fn wait_gave_up(waiter: i64, asked: &Asked, seconds: u64, why: &str) -> Fields {
    wait_fields("gave-up", waiter, asked, seconds, Some(why))
}

/// The counts a wait on phones names (#56), by the field that keeps each.
static COUNTS: LazyLock<[(&str, Regex); 6]> = LazyLock::new(|| {
    let count = |pattern: &str| Regex::new(pattern).unwrap_or_else(|_| unreachable!());
    [
        ("busy", count(r"(\d+) phone\(s\) busy")),
        ("locked", count(r", (\d+) locked")),
        ("asleep", count(r", (\d+) asleep")),
        ("stay_awake_off", count(r", (\d+) stay-awake off")),
        ("offline", count(r", (\d+) offline")),
        ("unauthorized", count(r", (\d+) unauthorized")),
    ]
});

/// The caps a request can wait at, each with the text that names its size.
static CAPS: LazyLock<[(&str, Regex); 3]> = LazyLock::new(|| {
    let cap = |pattern: &str| Regex::new(pattern).unwrap_or_else(|_| unreachable!());
    [
        (
            "emulator-cap",
            cap(r"the emulator cap \((\d+)\) is reached"),
        ),
        ("device-cap", cap(r"device\(s\); .* its cap of (\d+)")),
        ("slot-cap", cap(r"stack slots; cap (\d+)")),
    ]
});

/// A wait's reason as the event keeps it: `wait`, its kind, and the numbers in it: the busy
/// phones and the free ones not ready, by what is wrong with each (#56), or the cap it is at.
pub fn wait_reason(why: &str) -> Fields {
    let mut out = Fields::new();
    let kind = if why.is_empty() {
        "none"
    } else if let Some((kind, _)) = CAPS.iter().find(|(_, re)| re.is_match(why)) {
        kind
    } else if why.contains("too little memory for an emulator") {
        "memory"
    } else if why.starts_with("no free phone") {
        "phones"
    } else if why.starts_with("queued behind") {
        "queue"
    } else if why.contains("stack slots of") {
        "slots"
    } else if why == "adb is restarting" {
        "adb"
    } else if why == "a phone is being probed again" {
        "reprobe"
    } else {
        "other"
    };
    put(&mut out, "wait", kind);
    if why.contains("phone(s) busy") {
        for (key, re) in COUNTS.iter() {
            if let Some(n) = re.captures(why).and_then(|c| c[1].parse::<u64>().ok()) {
                put(&mut out, key, n);
            }
        }
    }
    if let Some((_, re)) = CAPS.iter().find(|(k, _)| *k == kind)
        && let Some(n) = re.captures(why).and_then(|c| c[1].parse::<u64>().ok())
    {
        put(&mut out, "cap", n);
    }
    out
}

/// The `lab-reclaim` event of a `kind` device (`model`) taken from lease `lease`, which ran
/// nothing for `idle_minutes`, for waiter `waiter`.
pub fn reclaim(
    lease: i64,
    kind: &str,
    model: &str,
    idle_minutes: i64,
    waiter: Option<i64>,
) -> Fields {
    let mut out = Fields::new();
    put(&mut out, "lease", lease);
    put(&mut out, "kind", kind);
    if !model.is_empty() {
        put(&mut out, "model", model);
    }
    put(&mut out, "idle_minutes", idle_minutes.max(0));
    if let Some(waiter) = waiter {
        put(&mut out, "waiter", waiter);
    }
    out
}

/// A readiness as the event says it: one word, no spaces.
fn readiness_word(r: Readiness) -> &'static str {
    match r {
        Readiness::StayAwakeOff => "stay-awake-off",
        other => other.word(),
    }
}

/// The `lab-readiness` event of phone `serial` (`model`) going `from` one readiness (none:
/// first seen) `to` another; `notified`, whether a desktop notification said so.
pub fn readiness(
    model: &str,
    serial: &str,
    from: Option<Readiness>,
    to: Readiness,
    notified: bool,
) -> Fields {
    let mut out = Fields::new();
    if !model.is_empty() {
        put(&mut out, "model", model);
    }
    put(&mut out, "phone", short_id(serial));
    put(&mut out, "from", from.map_or("none", readiness_word));
    put(&mut out, "to", readiness_word(to));
    put(
        &mut out,
        "notified",
        if notified { "true" } else { "false" },
    );
    out
}

/// A stable short name for a device that is not its serial: the first 8 hex digits of the
/// serial's sha256.
pub fn short_id(serial: &str) -> String {
    Sha256::digest(serial.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}
