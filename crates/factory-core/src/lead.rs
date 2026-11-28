//! A lead: a Claude session herdr runs under the name `lead-<slug>`, working through one spec.
//!
//! What herdr says about the lead's agent, whether its report file exists and what it has
//! registered a wait on fold into one [`LeadState`], and that state decides what a wait on the
//! lead answers.

use std::fmt;
use std::time::Duration;

use crate::exit::Exit;

/// The prefix of every lead's herdr agent name.
pub const PREFIX: &str = "lead-";

/// How long herdr waits for Claude to come up in a lead's pane.
pub const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long herdr gets to call a fresh pane's shell available, once it has refused
/// `agent.start` with `agent_pane_busy` while the shell runs its rc.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// The first wait before `agent.start` is asked again on a busy pane; each wait doubles up to
/// [`BUSY_PAUSE_MAX`].
pub const BUSY_PAUSE: Duration = Duration::from_millis(100);
/// The longest single wait between asks on a busy pane.
pub const BUSY_PAUSE_MAX: Duration = Duration::from_secs(1);
/// How long Claude has to settle once the folder-trust dialog is answered.
pub const TRUST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Claude has to start working once the prompt is typed.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Claude has to start working once an enter submits a prompt that was left typed.
pub const SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the trust dialog's selection takes to move before enter confirms it.
pub const KEY_PAUSE: Duration = Duration::from_millis(300);
/// How many times a stalled prompt is pressed or typed again.
pub const TRIES: u32 = 5;
/// What herdr's adapter adds to each call's own wait: 5 s to connect and 5 s to answer.
const CALL_SLACK: Duration = Duration::from_secs(10);

/// The longest a spawn can take with `pause` between its tries at a stalled prompt: every
/// herdr call at its limit, down the slowest path (the trust dialog, then every try typed
/// again), so a caller that waits this long always hears how the spawn ended.
pub fn spawn_longest(pause: Duration) -> Duration {
    let worktree = CALL_SLACK;
    // A busy pane is asked again until BUSY_TIMEOUT has gone by on the clock, so the longest is
    // that, plus one last attempt that can start just past it.
    let start = START_TIMEOUT + CALL_SLACK + BUSY_TIMEOUT + CALL_SLACK;
    // The screen read, down, enter and the wait for Claude to settle.
    let trust = CALL_SLACK * 4 + KEY_PAUSE + TRUST_TIMEOUT;
    let prompt = PROMPT_TIMEOUT + CALL_SLACK;
    // The screen read, then the prompt typed again: slower than an enter and its short wait.
    let try_again = pause + CALL_SLACK * 2 + PROMPT_TIMEOUT;
    worktree + start + trust + prompt + try_again * TRIES
}

/// What herdr reports about an agent: herdr's `AgentStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    /// herdr's spelling of the status.
    pub fn name(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }

    /// Reads herdr's spelling; a status this factory doesn't know reads as unknown.
    pub fn parse(status: &str) -> AgentStatus {
        match status {
            "idle" => AgentStatus::Idle,
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "done" => AgentStatus::Done,
            _ => AgentStatus::Unknown,
        }
    }
}

/// Everything a lead's state is decided from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadFacts<'a> {
    /// The status of the lead's agent; none once its agent or pane is gone.
    pub agent: Option<AgentStatus>,
    /// Whether the lead has written its final report.
    pub reported: bool,
    /// What the lead has registered a wait on, if anything.
    pub waiting_on: Option<&'a str>,
}

/// What a lead is doing, as the chief of staff needs to know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeadState {
    Working,
    /// Waiting on what it names, such as another lead or a PR's CI.
    Waiting(String),
    /// Stopped at a dialog or a permission prompt.
    Blocked,
    /// Ended its turn without its report: it has a question.
    Asking,
    /// Wrote its report.
    Finished,
    /// Its agent or pane is gone without a report.
    Dead,
}

impl LeadState {
    /// Decides a lead's state. A blocked lead is blocked even with a report, since a prompt
    /// still waits on an answer. Otherwise the report is the completion signal, unless the lead
    /// is still working; a gone agent without one is dead; a registered wait comes before working
    /// or asking. An unknown status counts as working: herdr has not settled on one yet.
    pub fn of(facts: LeadFacts<'_>) -> LeadState {
        let Some(status) = facts.agent else {
            return if facts.reported {
                LeadState::Finished
            } else {
                LeadState::Dead
            };
        };
        if status == AgentStatus::Blocked {
            return LeadState::Blocked;
        }
        if facts.reported && status != AgentStatus::Working {
            return LeadState::Finished;
        }
        if let Some(on) = facts.waiting_on {
            return LeadState::Waiting(on.to_owned());
        }
        match status {
            AgentStatus::Idle | AgentStatus::Done => LeadState::Asking,
            AgentStatus::Working | AgentStatus::Unknown | AgentStatus::Blocked => {
                LeadState::Working
            }
        }
    }

    /// The one word for the state, as `--json` gives it.
    pub fn kind(&self) -> &'static str {
        match self {
            LeadState::Working => "working",
            LeadState::Waiting(_) => "waiting",
            LeadState::Blocked => "blocked",
            LeadState::Asking => "asking",
            LeadState::Finished => "finished",
            LeadState::Dead => "dead",
        }
    }

    /// What a wait on the lead answers in this state: none while it should go on waiting.
    pub fn wait_exit(&self) -> Option<Exit> {
        match self {
            LeadState::Working | LeadState::Waiting(_) => None,
            LeadState::Finished => Some(Exit::Ok),
            LeadState::Blocked | LeadState::Asking | LeadState::Dead => Some(Exit::NeedsAttention),
        }
    }

    /// Whether the lead's last pane lines are worth showing: it is waiting on an answer.
    pub fn shows_screen(&self) -> bool {
        matches!(self, LeadState::Blocked | LeadState::Asking)
    }
}

impl fmt::Display for LeadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeadState::Waiting(on) => write!(f, "waiting: {on}"),
            other => f.write_str(other.kind()),
        }
    }
}

/// The herdr agent name of the lead an argument names: its slug, or the name itself.
pub fn lead_name(arg: &str) -> Result<String, BadLeadName> {
    let slug = arg.strip_prefix(PREFIX).unwrap_or(arg);
    let mut chars = slug.chars();
    let starts_well = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_well = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if starts_well && rest_well && slug.len() <= 27 {
        Ok(format!("{PREFIX}{slug}"))
    } else {
        Err(BadLeadName(arg.to_owned()))
    }
}

/// The integration branch a lead's worktree is cut on.
pub fn branch(slug: &str) -> String {
    format!("factory/{slug}")
}

/// What a freshly started lead is told: run `skill` on `spec`, unattended unless `attended`
/// says a person is at its pane, and leave the final report at `report`.
pub fn prompt(skill: &str, spec: &str, branch: &str, report: &str, attended: bool) -> String {
    let unattended = if attended {
        ""
    } else {
        "You run unattended: wherever a skill says to ask or confirm with the user, decide from \
         the spec and tickets yourself and list each such decision in your report. "
    };
    format!(
        "/{skill} {spec}. Integration branch: {branch}, already checked out here. \
         {unattended}When everything is finished, run devlab release (a finished lead must not \
         hold a stack slot or a phone), then write your final report to {report}."
    )
}

/// The efforts Claude starts with, as `factory lead spawn --effort` takes them.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The arguments Claude starts with: its model and effort, and a settings override that turns
/// each of `disabled_plugins` (`name@marketplace`) off, so their skills stay out of every request.
pub fn claude_args(model: &str, effort: &str, disabled_plugins: &[String]) -> Vec<String> {
    let mut args = vec![
        "--model".to_owned(),
        model.to_owned(),
        "--effort".to_owned(),
        effort.to_owned(),
    ];
    if !disabled_plugins.is_empty() {
        let enabled: serde_json::Map<String, serde_json::Value> = disabled_plugins
            .iter()
            .map(|plugin| (plugin.clone(), serde_json::Value::Bool(false)))
            .collect();
        args.push("--settings".to_owned());
        args.push(serde_json::json!({ "enabledPlugins": enabled }).to_string());
    }
    args
}

/// The slug of a lead's agent name.
pub fn slug(name: &str) -> &str {
    name.strip_prefix(PREFIX).unwrap_or(name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadLeadName(pub String);

impl fmt::Display for BadLeadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no lead can be named {:?}: give its slug (lowercase letters, digits and -, at most 27) or lead-<slug>",
            self.0
        )
    }
}
