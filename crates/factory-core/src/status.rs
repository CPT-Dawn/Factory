//! `factory status`: what the daemon holds, as one record and as one screen.
//!
//! The record ([`Status`]) is what `--json` prints, every field of it. The screen ([`screen`])
//! is the same record cut to fit [`ROWS`] rows of [`COLUMNS`] columns for a lab of three slots,
//! three review backends and five leads: a section that has more rows than its cap says how many
//! it left out.

use serde::{Deserialize, Serialize};

use crate::lab::{ago, base_name};
use crate::steward::kilo;
use crate::timestamp::Timestamp;
use crate::usage::UsageReading;

/// Everything `factory status` shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The daemon's clock, milliseconds since the epoch.
    pub now: i64,
    pub lab: Lab,
    /// The configured review backends, in the order they are tried.
    pub review: Vec<Backend>,
    /// The Claude plan's 5-hour window, and whether a lead may dispatch.
    pub claude: Claude,
    pub waits: Vec<Wait>,
    pub leads: Leads,
}

/// The lab's leases, slots and queue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lab {
    /// By id.
    pub leases: Vec<Lease>,
    /// By project, then slot: every slot the lab has used.
    pub slots: Vec<Slot>,
    /// Slots set aside for a lead's implementers, by id.
    pub reservations: Vec<Reservation>,
    /// Oldest first.
    pub queue: Vec<Queued>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: i64,
    pub project: Option<String>,
    pub slot: Option<u32>,
    pub worktree: String,
    /// `<comm>:<pid>` of the owner.
    pub owner: String,
    /// Seconds since the epoch.
    pub created: i64,
    pub touched: i64,
    /// The serials of the devices it holds.
    pub devices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub project: String,
    pub slot: u32,
    /// The lease that holds it; none when it is free.
    pub lease: Option<i64>,
    /// The reservation that holds it; none when it is free or leased.
    pub reservation: Option<i64>,
    pub last_worktree: Option<String>,
    /// devlab started its stack, so the idle timer may stop it.
    pub stack_ours: bool,
    /// A `release --stop-stack` is stopping its stack.
    pub stopping: bool,
    pub idle_since: Option<i64>,
}

/// A slot set aside for one of a lead's implementers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: i64,
    pub project: String,
    pub slot: u32,
    /// The lead's worktree.
    pub worktree: String,
    /// `<comm>:<pid>` of the lead.
    pub owner: String,
    /// Seconds since the epoch.
    pub created: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    pub id: i64,
    /// `<owner label> @ <worktree name>`.
    pub label: String,
    pub project: Option<String>,
    /// Seconds since the epoch.
    pub created: i64,
    /// When the request gives up, in milliseconds since the epoch.
    pub deadline_ms: Option<i64>,
}

/// One review backend's quota and load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backend {
    pub name: String,
    pub quota: QuotaState,
    /// When a cooldown ends, in milliseconds since the epoch.
    pub resets: Option<i64>,
    /// Why its quota is unknown, when a review could not read it (agy's usage answer).
    #[serde(default)]
    pub reason: Option<String>,
    pub running: u32,
    pub concurrency: u32,
    /// `queue` or `next`.
    pub when_busy: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaState {
    /// Nothing learned yet, or a cooldown is over.
    Unknown,
    Available,
    Cooling,
}

/// The Claude plan's quota as `factory quota claude` judges it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claude {
    pub verdict: ClaudeState,
    /// The 5-hour window's spent share, rounded; none while no reading of it stands.
    pub used_percent: Option<u32>,
    /// When the 5-hour window resets, in milliseconds since the epoch.
    pub resets: Option<i64>,
    /// When a hold ends, in milliseconds since the epoch.
    pub until: Option<i64>,
    /// What holds dispatches: `weekly 91%`, `5h 93%` or `limit hit`.
    #[serde(default)]
    pub hold: Option<String>,
    /// The last reading of each window, the 5-hour one first.
    #[serde(default)]
    pub windows: Vec<UsageReading>,
}

/// Whether a lead may dispatch on the Claude plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeState {
    /// No window seen, or the last one has reset.
    #[default]
    Unknown,
    Go,
    Hold,
}

/// A registered wait.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    /// The herdr pane it was registered from; none outside herdr.
    pub pane: Option<String>,
    /// The lead in that pane, when there is one.
    pub lead: Option<String>,
    /// `lead`, `ci` or `review`.
    pub kind: String,
    /// The lead's name, the pull request as `<owner>/<repo>#<number>`, or the review's label.
    pub on: String,
}

/// The leads in herdr and what the context steward knows of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leads {
    /// Why the leads can't be told, when herdr is not reachable.
    pub error: Option<String>,
    /// Whether the context steward runs.
    pub monitor: bool,
    pub leads: Vec<Lead>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lead {
    pub name: String,
    /// Working, waiting, blocked, asking, finished or dead.
    pub state: String,
    /// The state as `factory lead list` says it, such as `waiting: lead-b`.
    pub status: String,
    pub pane: String,
    pub report: Option<String>,
    /// The Claude Code session the steward found for it.
    pub session: Option<String>,
    /// The latest context; none before the steward has read one.
    pub tokens: Option<u64>,
    pub asked: bool,
    pub compacting: bool,
    pub subagents: Vec<Subagent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    pub agent: String,
    pub description: Option<String>,
    pub tokens: Option<u64>,
    pub told: bool,
}

/// The rows a screen may take.
pub const ROWS: usize = 24;
/// The columns a row may take.
pub const COLUMNS: usize = 80;

/// The most rows each section lists before it says how many it left out. The lab the ticket
/// names (three slots, three backends, five leads) takes at most 23 rows with every section at
/// its cap, the Claude plan's row included.
const LAB_ROWS: usize = 5;
const QUEUE_ROWS: usize = 2;
const WAIT_ROWS: usize = 3;
const LEAD_ROWS: usize = 5;

/// The record as rows of at most [`COLUMNS`] characters.
pub fn screen(status: &Status) -> Vec<String> {
    let mut rows = vec![format!("factory status {}", minute(status.now))];
    lab(status, &mut rows);
    review(status, &mut rows);
    claude(status, &mut rows);
    waits(status, &mut rows);
    leads(status, &mut rows);
    rows.iter().map(|row| fit(row)).collect()
}

fn lab(status: &Status, rows: &mut Vec<String>) {
    let lab = &status.lab;
    if lab.leases.is_empty()
        && lab.slots.is_empty()
        && lab.reservations.is_empty()
        && lab.queue.is_empty()
    {
        rows.push("lab: idle".to_owned());
        return;
    }
    let free = lab
        .slots
        .iter()
        .filter(|slot| slot.lease.is_none() && slot.reservation.is_none() && !slot.stopping)
        .count();
    let reserved = match lab.reservations.len() {
        0 => String::new(),
        n => format!(", {n} reserved"),
    };
    rows.push(format!(
        "lab: {} leased{reserved}, {free} free, {} queued",
        lab.leases.len(),
        lab.queue.len()
    ));
    let now = status.now / 1000;
    let mut held: Vec<String> = lab
        .leases
        .iter()
        .map(|lease| {
            let place = match (&lease.project, lease.slot) {
                (Some(project), Some(slot)) => format!("{project}/{slot} "),
                _ => String::new(),
            };
            let devices = match lease.devices.len() {
                0 => String::new(),
                1 => ", 1 device".to_owned(),
                n => format!(", {n} devices"),
            };
            format!(
                "  lease {} {place}{} {} {}{devices}",
                lease.id,
                base_name(&lease.worktree),
                lease.owner,
                ago(now, lease.created)
            )
        })
        .collect();
    held.extend(lab.reservations.iter().map(|reservation| {
        format!(
            "  reserved {}/{} {} {} {}",
            reservation.project,
            reservation.slot,
            base_name(&reservation.worktree),
            reservation.owner,
            ago(now, reservation.created)
        )
    }));
    held.extend(
        lab.slots
            .iter()
            .filter(|slot| slot.lease.is_none() && slot.reservation.is_none())
            .map(|slot| {
                let state = if slot.stopping { "stopping" } else { "free" };
                let last = slot
                    .last_worktree
                    .as_deref()
                    .map_or_else(String::new, |wt| format!(", last {}", base_name(wt)));
                format!("  {}/{} {state}{last}", slot.project, slot.slot)
            }),
    );
    listed(rows, held, LAB_ROWS, "in the lab");
    let queued = lab
        .queue
        .iter()
        .map(|waiter| format!("  queued {} {}", waiter.label, ago(now, waiter.created)))
        .collect();
    listed(rows, queued, QUEUE_ROWS, "queued");
}

fn review(status: &Status, rows: &mut Vec<String>) {
    if status.review.is_empty() {
        rows.push("review: no backends configured".to_owned());
        return;
    }
    rows.push("review quota".to_owned());
    let width = status
        .review
        .iter()
        .map(|backend| backend.name.chars().count())
        .max()
        .unwrap_or(0);
    for backend in &status.review {
        let quota = match (backend.quota, backend.resets) {
            (QuotaState::Cooling, Some(resets)) => format!(
                "cooling, resets {} (in {})",
                moment(resets, status.now),
                ago(resets / 1000, status.now / 1000)
            ),
            (QuotaState::Cooling, None) => "cooling".to_owned(),
            (QuotaState::Available, _) => "available".to_owned(),
            (QuotaState::Unknown, _) => "unknown".to_owned(),
        };
        rows.push(format!(
            "  {:<width$}  {quota}, {}/{} running",
            backend.name, backend.running, backend.concurrency
        ));
        // Why it is unknown, under it: a row of its own keeps the reason within the columns.
        if let (QuotaState::Unknown, Some(why)) = (backend.quota, &backend.reason) {
            rows.push(format!("    {why}"));
        }
    }
}

fn claude(status: &Status, rows: &mut Vec<String>) {
    let claude = &status.claude;
    let now = status.now;
    let row = match (claude.verdict, claude.until, &claude.hold) {
        (ClaudeState::Hold, Some(until), hold) => {
            let mut row = format!(
                "claude: hold: {} until {}",
                hold.as_deref().unwrap_or("limit hit"),
                moment(until, now)
            );
            if !claude.windows.is_empty() {
                let windows: Vec<String> = claude
                    .windows
                    .iter()
                    .map(|window| window.describe(now))
                    .collect();
                row.push_str("; ");
                row.push_str(&windows.join(", "));
            }
            row
        }
        _ if claude.windows.is_empty() => "claude: no plan window seen".to_owned(),
        _ => {
            let windows: Vec<String> = claude
                .windows
                .iter()
                .map(|window| window.describe_with_reset(now, |resets| moment(resets, now)))
                .collect();
            format!("claude: {}", windows.join(", "))
        }
    };
    rows.push(row);
}

fn waits(status: &Status, rows: &mut Vec<String>) {
    if status.waits.is_empty() {
        rows.push("waits: none".to_owned());
        return;
    }
    rows.push(format!("waits: {}", status.waits.len()));
    let each = status
        .waits
        .iter()
        .map(|wait| {
            let waiter = match (&wait.lead, &wait.pane) {
                (Some(lead), _) => lead.clone(),
                (None, Some(pane)) => format!("pane {pane}"),
                (None, None) => "outside herdr".to_owned(),
            };
            let on = match wait.kind.as_str() {
                kind @ ("ci" | "review" | "lease" | "claude") => format!("{kind} {}", wait.on),
                _ => wait.on.clone(),
            };
            format!("  {waiter} -> {on}")
        })
        .collect();
    listed(rows, each, WAIT_ROWS, "waits");
}

fn leads(status: &Status, rows: &mut Vec<String>) {
    let leads = &status.leads;
    if leads.error.is_some() {
        rows.push("leads: herdr unavailable, see factory lead list".to_owned());
        return;
    }
    let monitor = if leads.monitor { "on" } else { "off" };
    if leads.leads.is_empty() {
        rows.push(format!("leads: none, monitor {monitor}"));
        return;
    }
    rows.push(format!("leads: {}, monitor {monitor}", leads.leads.len()));
    let shown = &leads.leads[..leads.leads.len().min(LEAD_ROWS)];
    let name_width = shown
        .iter()
        .map(|l| l.name.chars().count())
        .max()
        .unwrap_or(0);
    let status_width = shown
        .iter()
        .map(|l| l.status.chars().count())
        .max()
        .unwrap_or(0);
    let each = leads
        .leads
        .iter()
        .map(|lead| {
            let mut context: Vec<String> = lead.tokens.map(kilo).into_iter().collect();
            if lead.asked {
                context.push("asked".to_owned());
            }
            if lead.compacting {
                context.push("compacting".to_owned());
            }
            let mut context = if context.is_empty() {
                "-".to_owned()
            } else {
                context.join(" ")
            };
            if !lead.subagents.is_empty() {
                let most = lead.subagents.iter().filter_map(|sub| sub.tokens).max();
                let most =
                    most.map_or_else(String::new, |tokens| format!(", most {}", kilo(tokens)));
                let n = lead.subagents.len();
                let noun = if n == 1 { "sub" } else { "subs" };
                context.push_str(&format!("  {n} {noun}{most}"));
            }
            format!(
                "  {:<name_width$}  {:<status_width$}  {context}",
                lead.name, lead.status
            )
            .trim_end()
            .to_owned()
        })
        .collect();
    listed(rows, each, LEAD_ROWS, "leads");
}

/// Adds the first `cap` of `each`, then a row that says how many it left out.
fn listed(rows: &mut Vec<String>, each: Vec<String>, cap: usize, what: &str) {
    let left = each.len().saturating_sub(cap);
    rows.extend(each.into_iter().take(cap));
    if left > 0 {
        rows.push(format!("  +{left} more {what}"));
    }
}

/// `2026-11-23 04:13Z`.
fn minute(millis: i64) -> String {
    let stamp = Timestamp::from_unix_millis(millis).to_string();
    format!("{} {}Z", &stamp[..10], &stamp[11..16])
}

/// `04:13Z` on the day of `now`, else `11-24 01:00Z`.
pub(crate) fn moment(millis: i64, now: i64) -> String {
    let (day, now_day) = (
        Timestamp::from_unix_millis(millis).to_string(),
        Timestamp::from_unix_millis(now).to_string(),
    );
    if day[..10] == now_day[..10] {
        format!("{}Z", &day[11..16])
    } else {
        format!("{} {}Z", &day[5..10], &day[11..16])
    }
}

/// The row, cut to [`COLUMNS`] characters with an ellipsis when it is longer.
fn fit(row: &str) -> String {
    if row.chars().count() <= COLUMNS {
        return row.to_owned();
    }
    let mut cut: String = row.chars().take(COLUMNS - 1).collect();
    cut.push('…');
    cut
}
