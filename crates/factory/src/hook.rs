//! `factory hook pre-tool|post-tool|subagent-stop`: Claude Code's hook JSON on stdin,
//! answered in its hook protocol on stdout.
//!
//! A hook runs on every tool call, so it never talks to the daemon and reads no more than it
//! must. It prints nothing and exits 0 to let a call through, and one JSON line to refuse it.
//! A hook that cannot read its input exits 3: Claude Code shows that line and carries on, where
//! exit 2 would block the call.

use std::env;
use std::fs::{self, DirBuilder};
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;

use factory_adapters::git::{GitRepo, Repo};
use factory_core::hook::{self, GitAction, PreTool};
use serde_json::{Value, json};

use crate::cli::HookCommand;
use crate::config;
use crate::output::{self, Failure};
use crate::paths;

pub fn run(command: &HookCommand, json: bool) -> ExitCode {
    match answer(command) {
        Ok(Some(line)) => {
            let _ = writeln!(io::stdout().lock(), "{line}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(failure) => output::finish(Err(failure), json),
    }
}

/// The JSON line the hook prints, or none to let the call through.
fn answer(command: &HookCommand) -> Result<Option<Value>, Failure> {
    let input = read_input()?;
    Ok(match command {
        HookCommand::PreTool => match hook::pre_tool(&input, escape_set()) {
            PreTool::Allow => None,
            PreTool::Block(reason) => Some(deny(&reason)),
            PreTool::CheckStaged(actions) => {
                // A config that can't be read refuses the call: a leak must not slip by on a typo.
                match leak(&input, &actions) {
                    Ok(None) => None,
                    Ok(Some(reason)) => Some(deny(&reason)),
                    Err(failure) => Some(deny(&failure.message)),
                }
            }
        },
        HookCommand::PostTool => {
            hook::post_tool(&input).map(|line| json!({"decision": "block", "reason": line}))
        }
        HookCommand::SubagentStop => {
            let settings = config::hook_settings(&paths::config_dir())?;
            subagent_stop(&input, &settings.templates)
                .map(|line| json!({"decision": "block", "reason": line}))
        }
    })
}

/// The subagent-stop rule, with a mark in the runtime dir for each agent this hook sent back, so
/// it lets go only of an agent it kept going itself. Without a place for the mark, an agent kept
/// going by any hook is let go, as nothing could tell this hook's send-back from another's.
fn subagent_stop(input: &Value, templates: &hook::Templates) -> Option<String> {
    let Some(mark) = sent_back_mark(input) else {
        return hook::subagent_stop(input, templates, true);
    };
    let said = hook::subagent_stop(input, templates, mark.exists());
    if said.is_none() {
        let _ = fs::remove_file(&mark);
        return None;
    }
    // A send-back this hook cannot remember could send the agent back forever.
    let remembered = mark.parent().is_some_and(|dir| {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .is_ok()
    }) && fs::write(&mark, "").is_ok();
    said.filter(|_| remembered)
}

/// Where the mark for the agent the input names goes: `$XDG_RUNTIME_DIR/factory/sent-back/`, under
/// the session and agent ids in hex. None without an agent id or a runtime dir.
fn sent_back_mark(input: &Value) -> Option<PathBuf> {
    let id = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    let agent = id("agent_id");
    let key = format!("{}\0{agent}", id("session_id"));
    // A file name holds 255 bytes, two hex digits a byte.
    if agent.is_empty() || key.len() > 120 {
        return None;
    }
    let name: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    Some(paths::runtime_dir()?.join("sent-back").join(name))
}

fn deny(reason: &str) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }})
}

/// The refusal for a commit or push, in any of its repositories, whose diff adds a private
/// address from the config. A diff that cannot be read is refused too: the hook cannot say it is
/// clean, and the line says how to look.
fn leak(input: &Value, actions: &[GitAction]) -> Result<Option<String>, Failure> {
    let addresses = config::hook_settings(&paths::config_dir())?.private_addresses;
    if addresses.is_empty() {
        return Ok(None);
    }
    let cwd = match input.get("cwd").and_then(Value::as_str) {
        Some(cwd) => PathBuf::from(cwd),
        None => env::current_dir().unwrap_or_default(),
    };
    for action in actions {
        let dir = match &action.dir {
            Some(dir) => cwd.join(expand_home(dir)),
            None => cwd.clone(),
        };
        let diff = GitRepo::at(&dir)
            .staged_diff(action.tracked, action.added.as_deref())
            .map_err(|e| {
                // Neither the path nor git's words: they may carry the address.
                let _ = e;
                Failure::precondition(
                    "The hook could not read what would be committed: run git -C <the repository> status to see why, then commit again.",
                )
            })?;
        if let Some(reason) = hook::private_address(&diff, &addresses) {
            return Ok(Some(reason));
        }
    }
    Ok(None)
}

/// `~` and `~/x` the way the shell the command runs in would.
fn expand_home(dir: &str) -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from);
    match (dir, dir.strip_prefix("~/"), home) {
        ("~", _, Some(home)) => home,
        (_, Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(dir),
    }
}

fn read_input() -> Result<Value, Failure> {
    let bad = |why: &str| {
        Failure::precondition(format!(
            "hook input is not a JSON object ({why}); Claude Code sends the call on stdin"
        ))
    };
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| bad(&e.to_string()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(input @ Value::Object(_)) => Ok(input),
        Ok(_) => Err(bad("another JSON value")),
        Err(e) => Err(bad(&e.to_string())),
    }
}

fn escape_set() -> bool {
    env::var_os(hook::ESCAPE).is_some_and(|value| !value.is_empty() && value != "0")
}
