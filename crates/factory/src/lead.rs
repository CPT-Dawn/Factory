//! `factory lead list|status` and `factory wait lead`: what the leads in herdr are doing, from
//! the daemon, which follows herdr's events.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_adapters::git::{GitRepo, Repo};
use factory_core::exit::Exit;
use factory_core::lead::{self, lead_name};
use serde_json::{Value, json};

use crate::client;
use crate::config;
use crate::notify;
use crate::output::{Done, Failure, LINE_LIMIT};
use crate::paths::Paths;
use crate::protocol::{
    LEAD_LIST, LEAD_SPAWN, LEAD_STATUS, LeadList, LeadParams, LeadReport, NotifyOn, SpawnParams,
    SpawnResult, WAIT_LEAD, WaitLeadParams, WaitLeadResult,
};

/// What the daemon asks of herdr around a spawn, beyond the spawn's own steps: its ping, and
/// the listings and subscription before and after.
const DAEMON_SLACK: Duration = Duration::from_secs(60);

pub struct SpawnArgs {
    pub slug: String,
    pub spec: String,
    pub base: String,
    pub skill: String,
    pub repo: Option<PathBuf>,
}

/// Starts the lead `args.slug` and says where it is. Everything that can be refused is refused
/// here, against the repo, before herdr is asked for anything.
pub fn spawn(args: &SpawnArgs) -> Result<Done, Failure> {
    let name = lead_name(&args.slug).map_err(|e| Failure::usage(e.to_string()))?;
    let paths = Paths::from_env()?;
    let setup = config::lead_setup(&paths)?;
    let dir = match &args.repo {
        Some(dir) => dir.clone(),
        None => env::current_dir().map_err(|e| {
            Failure::precondition(format!(
                "cannot tell the current directory: {e}; pass --repo"
            ))
        })?,
    };
    let git = GitRepo::at(&dir);
    let no_repo = |why: &dyn std::fmt::Display| {
        Failure::precondition(format!(
            "{} is not a git repository ({why}); run from the repo or pass --repo",
            dir.display()
        ))
    };
    if !dir.is_dir() {
        return Err(no_repo(&"no such directory"));
    }
    let top = git.toplevel().map_err(|e| no_repo(&e))?;
    let repo_name = git.name().map_err(|e| no_repo(&e))?;
    let git_failed = |e: factory_adapters::git::GitError| {
        Failure::precondition(format!(
            "{e}; check the repository at {} and retry",
            top.display()
        ))
    };
    let branch = lead::branch(lead::slug(&name));
    if git.branch_exists(&branch).map_err(git_failed)? {
        return Err(Failure::precondition(format!(
            "{branch} already exists in {repo_name}; pick another slug, or delete the branch"
        )));
    }
    if git.commit(&args.base).map_err(git_failed)?.is_none() {
        return Err(Failure::usage(format!(
            "unknown base: {}; name a branch, tag or commit of {}",
            args.base,
            top.display()
        )));
    }
    let params = SpawnParams {
        lead: name,
        repo_path: top.to_string_lossy().into_owned(),
        repo_name,
        base: args.base.clone(),
        skill: args.skill.clone(),
        spec: args.spec.clone(),
        claude_args: lead::claude_args(&setup.model, &setup.effort, &setup.disable_plugins),
        model: setup.model,
        effort: setup.effort,
        retry_pause_ms: u64::try_from(setup.retry_pause.as_millis()).unwrap_or(u64::MAX),
        session: env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty()),
    };
    // As long as the slowest spawn takes, so the line always comes back.
    let patience = lead::spawn_longest(setup.retry_pause) + DAEMON_SLACK;
    let started: SpawnResult = client::wait(&paths, LEAD_SPAWN, &params, patience)?;
    Ok(spawned(&started))
}

pub fn list() -> Result<Done, Failure> {
    let paths = Paths::from_env()?;
    let list: LeadList = client::call(&paths, LEAD_LIST, &json!({}))?;
    let json = serde_json::to_value(&list).unwrap_or(Value::Null);
    let mut lines = list.leads.iter().map(line);
    let Some(first) = lines.next() else {
        return Ok(Done::new("no leads in herdr", json));
    };
    let mut done = Done::new(first, json);
    done.more = lines.collect();
    Ok(done)
}

pub fn status(lead: &str) -> Result<Done, Failure> {
    let params = LeadParams {
        lead: lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?,
    };
    let paths = Paths::from_env()?;
    let report: LeadReport = client::call(&paths, LEAD_STATUS, &params)?;
    Ok(shown(&report, Exit::Ok))
}

pub fn wait(lead: &str, timeout: Duration) -> Result<Done, Failure> {
    let params = WaitLeadParams {
        lead: lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?,
        waiter: client::waiter(),
        timeout_secs: timeout.as_secs(),
        resume: None,
    };
    let paths = Paths::from_env()?;
    let answer: WaitLeadResult = client::hold(&paths, WAIT_LEAD, &params, timeout)?;
    let exit = Exit::from_code(answer.exit).unwrap_or(Exit::NeedsAttention);
    let mut done = shown(&answer.lead, exit);
    done.json = serde_json::to_value(&answer).unwrap_or(Value::Null);
    Ok(done)
}

/// `factory wait lead --notify`: registers the wait, and the daemon posts its line when it ends.
pub fn notify(lead: &str, timeout: Duration) -> Result<Done, Failure> {
    let lead = lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?;
    notify::register(NotifyOn::Lead { lead }, timeout)
}

/// A lead's line, then its pane's last lines when it is blocked or asking.
fn shown(report: &LeadReport, exit: Exit) -> Done {
    let mut done = Done::new(
        line(report),
        serde_json::to_value(report).unwrap_or(Value::Null),
    );
    done.more.clone_from(&report.lines);
    done.exit = exit;
    done
}

pub fn line(report: &LeadReport) -> String {
    match &report.report {
        Some(path) if report.state == "finished" => format!("{} finished: {path}", report.lead),
        _ => format!("{} {}", report.lead, report.status),
    }
}

/// The line that says where a lead is. When the paths would push it past the output limit, the
/// worktree and report go to a details file beside the report, and the line names that file
/// whole: a cut path is no path.
fn spawned(lead: &SpawnResult) -> Done {
    let json = serde_json::to_value(lead).unwrap_or(Value::Null);
    let full = format!(
        "{}: workspace {}, branch {}, worktree {}, report {}",
        lead.lead, lead.workspace, lead.branch, lead.worktree, lead.report
    );
    let mut done = Done::new(full, json);
    if done.line.chars().count() <= LINE_LIMIT {
        return done;
    }
    let details = Path::new(&lead.report).with_extension("spawn.json");
    // A details file that can't be written leaves the full line, shown whole.
    if fs::write(&details, format!("{}\n", done.json)).is_ok() {
        done.line = format!(
            "{}: workspace {}, branch {}; worktree and report in {}",
            lead.lead,
            lead.workspace,
            lead.branch,
            details.display()
        );
    }
    done.whole = true;
    done
}
