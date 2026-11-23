//! Starting a lead in herdr: a workspace on a new worktree, Claude in its pane, and the lead's
//! prompt typed and submitted.
//!
//! Claude in a freshly made worktree can stop at the folder-trust dialog, and one that has
//! just come up can take the prompt's keys before its input box submits them, which herdr
//! reports as `agent_prompt_stalled`. Both are handled here, as lead.sh handled them, so a
//! spawn that only needed an enter doesn't come back as a failure.

use std::path::Path;
use std::time::Duration;

use factory_adapters::herdr::{HerdrApi, HerdrError};
use tokio::time::sleep;

/// How long herdr waits for Claude to come up in its pane.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long Claude has to settle once the trust dialog is answered.
const TRUST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Claude has to start working once the prompt is typed.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Claude has to start working once an enter submits a prompt that was left typed.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the trust dialog's selection takes to move before enter confirms it.
const KEY_PAUSE: Duration = Duration::from_millis(300);
/// How many times a stalled prompt is pressed or typed again.
const TRIES: u32 = 5;
/// How many characters of the prompt identify it on the screen.
const HEAD: usize = 60;
const STALLED: &str = "agent_prompt_stalled";
const TRUST_TEXT: &str = "Yes, I trust this folder";

/// What to start.
pub struct Plan<'a> {
    /// The agent's herdr name, `lead-<slug>`.
    pub name: &'a str,
    /// The repository the worktree is cut from.
    pub repo: &'a Path,
    pub branch: &'a str,
    pub base: &'a str,
    /// The workspace's label.
    pub label: &'a str,
    /// What Claude starts with.
    pub claude_args: &'a [String],
    pub prompt: &'a str,
    /// How long to wait before looking at a prompt that stalled.
    pub pause: Duration,
}

/// A lead that is working.
pub struct Started {
    pub workspace: String,
    pub worktree: String,
    /// How many times the prompt had to be pressed or typed again.
    pub stalls: u32,
}

/// Why a lead did not start, in one line, and how far it got.
pub struct Failed {
    pub message: String,
    pub stalls: u32,
}

impl Failed {
    fn at(message: String, stalls: u32) -> Failed {
        Failed { message, stalls }
    }
}

/// Opens the workspace, starts Claude in it and gets the prompt submitted.
pub async fn start<H: HerdrApi>(herdr: &H, plan: &Plan<'_>) -> Result<Started, Failed> {
    let made = herdr
        .create_worktree(plan.repo, plan.branch, plan.base, plan.label)
        .await
        .map_err(|e| {
            Failed::at(
                format!(
                    "herdr could not open a worktree for {}: {e}; check herdr, then retry",
                    plan.branch
                ),
                0,
            )
        })?;
    let at = Spot {
        name: plan.name,
        workspace: &made.workspace_id,
    };
    if herdr
        .start_agent(plan.name, &made.pane_id, plan.claude_args, START_TIMEOUT)
        .await
        .is_err()
    {
        pass_trust_dialog(herdr, &at, &made.path).await?;
    }
    let stalls = submit(herdr, &at, plan).await?;
    Ok(Started {
        workspace: made.workspace_id,
        worktree: made.path,
        stalls,
    })
}

/// Where a lead is, for the lines that say what went wrong.
struct Spot<'a> {
    name: &'a str,
    workspace: &'a str,
}

impl Spot<'_> {
    fn failed(&self, what: &str, stalls: u32) -> Failed {
        Failed::at(
            format!("{} {what}; see workspace {}", self.name, self.workspace),
            stalls,
        )
    }
}

/// A repo Claude has never opened stops at the folder-trust dialog. Answers it, but only when
/// the screen names the worktree made for this lead: any other stop is not ours to answer.
async fn pass_trust_dialog<H: HerdrApi>(
    herdr: &H,
    at: &Spot<'_>,
    worktree: &str,
) -> Result<(), Failed> {
    let screen = herdr.read_agent(at.name).await.unwrap_or_default();
    if !(screen.contains(worktree) && screen.contains(TRUST_TEXT)) {
        return Err(at.failed(
            "is stuck at startup on something other than folder trust",
            0,
        ));
    }
    let answered = async {
        herdr.send_keys(at.name, &["down"]).await?;
        sleep(KEY_PAUSE).await;
        herdr.send_keys(at.name, &["enter"]).await?;
        herdr
            .wait_agent(at.name, &["idle", "done"], TRUST_TIMEOUT)
            .await
    };
    answered
        .await
        .map_err(|_| at.failed("did not come up after the trust dialog", 0))
}

/// Types the prompt. A lead that stalls with the prompt still typed gets an enter, and one
/// whose prompt is gone gets it typed again, up to [`TRIES`] times. Returns how many tries it took.
async fn submit<H: HerdrApi>(herdr: &H, at: &Spot<'_>, plan: &Plan<'_>) -> Result<u32, Failed> {
    if typed(herdr, at, plan.prompt, 0).await? {
        return Ok(0);
    }
    let head = flat(&plan.prompt.chars().take(HEAD).collect::<String>());
    for stalls in 1..=TRIES {
        sleep(plan.pause).await;
        let screen = herdr.read_agent(at.name).await.unwrap_or_default();
        if flat(&screen).contains(&head) {
            let pressed = herdr.send_keys(at.name, &["enter"]).await;
            if pressed.is_ok()
                && herdr
                    .wait_agent(at.name, &["working", "blocked"], SUBMIT_TIMEOUT)
                    .await
                    .is_ok()
            {
                return Ok(stalls);
            }
        } else if typed(herdr, at, plan.prompt, stalls).await? {
            return Ok(stalls);
        }
    }
    Err(at.failed(
        &format!("took the prompt but never started working ({STALLED}, {TRIES} retries)"),
        TRIES,
    ))
}

/// Types the prompt: true once the lead is working, false when herdr says it stalled.
async fn typed<H: HerdrApi>(
    herdr: &H,
    at: &Spot<'_>,
    prompt: &str,
    stalls: u32,
) -> Result<bool, Failed> {
    match herdr
        .prompt_agent(at.name, prompt, &["working", "blocked"], PROMPT_TIMEOUT)
        .await
    {
        Ok(()) => Ok(true),
        Err(HerdrError::Refused { code, .. }) if code == STALLED => Ok(false),
        Err(e) => {
            let why = match e {
                HerdrError::Refused { code, .. } => code,
                other => other.to_string(),
            };
            Err(at.failed(&format!("did not start working ({why})"), stalls))
        }
    }
}

/// The text with its whitespace removed: Claude wraps its input box anywhere, even inside a path.
fn flat(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}
