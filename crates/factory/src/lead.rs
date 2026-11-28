//! `factory lead list|status` and `factory wait lead`: what the leads in herdr are doing, from
//! the daemon, which follows herdr's events.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use factory_adapters::git::{GitRepo, Repo};
use factory_core::duration;
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
    /// Overrides `[lead] model`.
    pub model: Option<String>,
    /// Overrides `[lead] effort`.
    pub effort: Option<String>,
    /// Whether a person is at the lead's pane, so its prompt need not say it runs unattended.
    pub attended: bool,
}

/// Starts the lead `args.slug` and says where it is. Everything that can be refused is refused
/// here, against the repo, before herdr is asked for anything.
pub fn spawn(args: &SpawnArgs) -> Result<Done, Failure> {
    let name = lead_name(&args.slug).map_err(|e| Failure::usage(e.to_string()))?;
    let paths = Paths::from_env()?;
    let setup = config::lead_setup(&paths)?;
    let model = args.model.clone().unwrap_or(setup.model);
    let effort = args.effort.clone().unwrap_or(setup.effort);
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
        claude_args: lead::claude_args(&model, &effort, &setup.disable_plugins),
        model,
        effort,
        attended: args.attended,
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
    let (lead, waiter) = (
        lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?,
        client::waiter(),
    );
    let params = |resume, timeout_secs| WaitLeadParams {
        lead: lead.clone(),
        waiter: waiter.clone(),
        timeout_secs,
        resume,
    };
    let paths = Paths::from_env()?;
    let answer: WaitLeadResult = client::hold(&paths, WAIT_LEAD, params, timeout)?;
    let exit = Exit::from_code(answer.exit).unwrap_or(Exit::NeedsAttention);
    let mut done = shown(&answer.lead, exit);
    done.json = serde_json::to_value(&answer).unwrap_or(Value::Null);
    Ok(done)
}

/// `factory wait lead --any`: one wait per lead, each held by the daemon as `wait` holds it,
/// answered by the first that settles. A lead the daemon refuses fails the whole wait at once;
/// the time runs out once every wait's has.
pub fn wait_any(leads: &[String], timeout: Duration) -> Result<Done, Failure> {
    let mut names: Vec<String> = Vec::new();
    for lead in leads {
        let name = lead_name(lead).map_err(|e| Failure::usage(e.to_string()))?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let (tx, rx) = mpsc::channel();
    for name in &names {
        let (tx, name) = (tx.clone(), name.clone());
        // Detached: the CLI exits with the first answer, and the others' connections close
        // with it, which the daemon takes as their callers gone.
        let worker = thread::Builder::new().spawn({
            let name = name.clone();
            move || {
                let _ = tx.send(wait(&name, timeout));
            }
        });
        if let Err(e) = worker {
            return Err(Failure::new(
                Exit::Precondition,
                format!("could not start the wait on {name}: {e}; wait on fewer leads"),
            ));
        }
    }
    drop(tx);
    let mut timed_out = 0;
    while let Ok(answer) = rx.recv() {
        match answer {
            Err(failure) if failure.exit == Exit::Timeout => timed_out += 1,
            answer => return answer,
        }
    }
    let message = if timed_out == names.len() {
        format!(
            "none of {} settled within {}; wait again, or see factory lead list",
            names.join(", "),
            duration::show(timeout)
        )
    } else {
        "a lead's wait ended without an answer; wait again".to_owned()
    };
    Err(Failure::new(Exit::Timeout, message))
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
