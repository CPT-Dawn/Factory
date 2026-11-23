//! `factory hook`: Claude Code's hook JSON on stdin, answered in its hook protocol.
//!
//! The rules are table-tested in factory-core; these tests pin the protocol around them.

use serde_json::{Value, json};

use crate::support::{Run, Sandbox};

fn pre_tool(sandbox: &Sandbox, call: &Value) -> Run {
    sandbox.factory_stdin(&["hook", "pre-tool"], &call.to_string())
}

fn bash(command: &str) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
    })
}

/// The one JSON object a hook that refuses prints on stdout.
fn answer(out: &Run) -> Value {
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    assert_eq!(out.stderr, "", "{out:?}");
    serde_json::from_str(&out.stdout).unwrap()
}

#[test]
fn pre_tool_denies_npm_in_claude_codes_protocol() {
    let sandbox = Sandbox::new();

    let out = pre_tool(&sandbox, &bash("npm install left-pad"));

    let said = answer(&out);
    let verdict = &said["hookSpecificOutput"];
    assert_eq!(verdict["hookEventName"], "PreToolUse");
    assert_eq!(verdict["permissionDecision"], "deny");
    let reason = verdict["permissionDecisionReason"].as_str().unwrap();
    assert!(reason.contains("bun"), "{reason}");
}

#[test]
fn pre_tool_says_nothing_to_a_call_it_allows() {
    let sandbox = Sandbox::new();

    let out = pre_tool(&sandbox, &bash("bun install"));

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", "")
    );
}

#[test]
fn pre_tool_lets_npm_through_when_the_escape_variable_is_set() {
    let sandbox = Sandbox::new().env("FACTORY_ALLOW_NPM", "1");

    let out = pre_tool(&sandbox, &bash("npm install"));

    assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{out:?}");
}

#[test]
fn a_hook_that_cannot_read_its_input_fails_open_with_one_line() {
    let sandbox = Sandbox::new();

    for input in ["", "not json", "[1]"] {
        let out = sandbox.factory_stdin(&["hook", "pre-tool"], input);

        // Exit 2 would block the tool call; 3 only shows the line.
        assert_eq!(out.code, 3, "{input:?}: {out:?}");
        assert_eq!(out.stdout, "", "{input:?}");
        assert_eq!(out.stderr_lines().len(), 1, "{input:?}: {out:?}");
        assert!(out.stderr.starts_with("factory: hook input"), "{out:?}");
    }
}

#[test]
fn pre_tool_denies_a_repl_script_that_uses_process() {
    let sandbox = Sandbox::new();
    let call = json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "REPL",
        "tool_input": {"code": "const home = process.env.HOME;", "description": "x"},
    });

    let out = pre_tool(&sandbox, &call);

    let said = answer(&out);
    assert_eq!(said["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = said["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("process") && reason.contains("printenv"),
        "{reason}"
    );
}

fn post_tool(sandbox: &Sandbox, stdout: &str) -> Run {
    let call = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "cat big.txt"},
        "tool_response": {"stdout": stdout, "stderr": "", "interrupted": false},
    });
    sandbox.factory_stdin(&["hook", "post-tool"], &call.to_string())
}

#[test]
fn post_tool_answers_a_long_result_in_claude_codes_protocol() {
    let sandbox = Sandbox::new();

    let out = post_tool(&sandbox, &"x".repeat(12_500));

    let said = answer(&out);
    assert_eq!(said["decision"], "block");
    let reason = said["reason"].as_str().unwrap();
    assert!(
        reason.contains("rg -n") && reason.contains("12500"),
        "{reason}"
    );
}

#[test]
fn post_tool_says_nothing_to_a_short_result() {
    let sandbox = Sandbox::new();

    let out = post_tool(&sandbox, "ok");

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", "")
    );
}

fn config(sandbox: &Sandbox, text: &str) {
    let dir = sandbox.path("config/factory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

fn subagent_stop(sandbox: &Sandbox, agent: &str, report: &str, active: bool) -> Run {
    let call = json!({
        "hook_event_name": "SubagentStop",
        "agent_type": agent,
        "stop_hook_active": active,
        "last_assistant_message": report,
    });
    sandbox.factory_stdin(&["hook", "subagent-stop"], &call.to_string())
}

const SIX_LINES: &str = "one\ntwo\nthree\nfour\nfive\nsix";

#[test]
fn subagent_stop_sends_a_long_report_back_in_claude_codes_protocol() {
    let sandbox = Sandbox::new();
    config(
        &sandbox,
        "[hook.report]\nmax_lines = 40\n[hook.report.agent.implementer]\nmax_lines = 5\n",
    );

    let out = subagent_stop(&sandbox, "implementer", SIX_LINES, false);

    let said = answer(&out);
    assert_eq!(said["decision"], "block");
    let reason = said["reason"].as_str().unwrap();
    assert!(
        reason.contains("6 lines") && reason.contains('5'),
        "{reason}"
    );
    // Another agent type has the default template, which this report fits.
    let out = subagent_stop(&sandbox, "merger", SIX_LINES, false);
    assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{out:?}");
}

#[test]
fn subagent_stop_lets_the_report_go_when_it_was_sent_back_once() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook.report]\nmax_lines = 5\n");

    let out = subagent_stop(&sandbox, "implementer", SIX_LINES, true);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", "")
    );
}

#[test]
fn subagent_stop_without_a_configured_template_sets_no_limit() {
    let sandbox = Sandbox::new();

    let out = subagent_stop(&sandbox, "implementer", &"line\n".repeat(500), false);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", "")
    );
}

#[test]
fn a_broken_config_fails_open_with_the_file_and_the_line() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook.report]\nmax_lines = \"five\"\n");

    let out = subagent_stop(&sandbox, "implementer", SIX_LINES, false);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.contains("config.toml line 2") && out.stderr.contains("fix the config"),
        "{out:?}"
    );
}

const OWNER: &str = "owner@example.invalid";

fn leak_config(sandbox: &Sandbox) {
    config(
        sandbox,
        &format!("[hook]\nprivate_addresses = [\"{OWNER}\"]\n"),
    );
}

fn bash_in(dir: &std::path::Path, command: &str) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "cwd": dir,
        "tool_input": {"command": command},
    })
}

/// A repo whose index adds a file with the private address in it.
fn repo_staging_the_address(sandbox: &Sandbox) -> std::path::PathBuf {
    let repo = sandbox.repo();
    std::fs::write(repo.join("notes.txt"), format!("mail {OWNER} for access\n")).unwrap();
    sandbox.git(&repo, &["add", "notes.txt"]);
    repo
}

fn denied_for_the_address(out: &Run) {
    let said = answer(out);
    assert_eq!(said["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = said["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(reason.contains("notes.txt"), "{reason}");
    assert!(
        !reason.contains(OWNER),
        "the address stays out of the line: {reason}"
    );
}

#[test]
fn pre_tool_denies_a_commit_whose_staged_diff_has_the_private_address() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);

    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -m 'feat: mail'"));

    denied_for_the_address(&out);
}

#[test]
fn pre_tool_denies_a_push_whose_staged_diff_has_the_private_address() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);

    let out = pre_tool(&sandbox, &bash_in(&repo, "git push origin work"));

    denied_for_the_address(&out);
}

#[test]
fn pre_tool_lets_a_commit_through_when_the_staged_diff_is_clean() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.repo();
    std::fs::write(repo.join("clean.txt"), "nothing private\n").unwrap();
    sandbox.git(&repo, &["add", "clean.txt"]);
    // The address is in the working tree, not staged: a plain commit does not take it along.
    std::fs::write(repo.join("a.txt"), format!("{OWNER}\n")).unwrap();

    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -m 'feat: clean'"));

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", "")
    );
}

#[test]
fn pre_tool_checks_the_working_tree_when_the_command_stages_it_first() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.repo();
    // A tracked change and an untracked file: `commit -a` takes the first, `add -A` both.
    std::fs::write(repo.join("a.txt"), format!("{OWNER}\n")).unwrap();
    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -am 'fix: a'"));
    let said = answer(&out);
    assert!(
        said["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("a.txt")
    );

    sandbox.git(&repo, &["checkout", "--", "a.txt"]);
    std::fs::write(repo.join("new.txt"), format!("{OWNER}\n")).unwrap();
    let out = pre_tool(
        &sandbox,
        &bash_in(&repo, "git add -A && git commit -m 'feat: new'"),
    );
    let said = answer(&out);
    assert!(
        said["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("new.txt")
    );

    // Without the add, the untracked file is not part of the commit.
    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -m 'feat: nothing'"));
    assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{out:?}");
}

#[test]
fn pre_tool_follows_git_dash_c_to_the_repo_it_commits_in() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);
    let elsewhere = sandbox.path("home");

    let out = pre_tool(
        &sandbox,
        &bash_in(
            &elsewhere,
            &format!("git -C {} commit -m x", repo.display()),
        ),
    );

    denied_for_the_address(&out);
}

#[test]
fn pre_tool_checks_nothing_without_a_configured_address() {
    let sandbox = Sandbox::new();
    let repo = repo_staging_the_address(&sandbox);

    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -m 'feat: mail'"));

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", "")
    );
}

#[test]
fn pre_tool_refuses_a_commit_it_cannot_check_because_the_config_is_broken() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook]\nprivate_addresses = 3\n");
    let repo = sandbox.repo();

    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -m x"));

    // A leak must not slip by on a typo: the call is denied, saying what to fix.
    let said = answer(&out);
    assert_eq!(said["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = said["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("config.toml line 2") && reason.contains("fix the config"),
        "{reason}"
    );
    // A call that is no commit or push never reads the config.
    let out = pre_tool(&sandbox, &bash_in(&repo, "git status"));
    assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{out:?}");
}

/// Spec #1: "a hook call completes in under 5 ms on this machine". The fastest of a few runs is
/// the call's own cost, free of the scheduler's noise.
#[test]
fn a_hook_call_completes_in_under_5_ms() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook.report]\nmax_lines = 5\n");
    let calls: [(&str, Value); 3] = [
        ("pre-tool", bash("bun run build && rg -n todo src")),
        (
            "post-tool",
            json!({"tool_name": "Bash", "tool_response": {"stdout": "x".repeat(11_000)}}),
        ),
        (
            "subagent-stop",
            json!({"agent_type": "implementer", "last_assistant_message": SIX_LINES}),
        ),
    ];
    for (hook, call) in calls {
        let input = call.to_string();
        let fastest = (0..25)
            .map(|_| {
                let started = std::time::Instant::now();
                let out = sandbox.factory_stdin(&["hook", hook], &input);
                let took = started.elapsed();
                assert!(out.code == 0 || out.code == 3, "{hook}: {out:?}");
                took
            })
            .min()
            .unwrap();
        assert!(
            fastest < std::time::Duration::from_millis(5),
            "{hook} took {fastest:?}"
        );
    }
}
