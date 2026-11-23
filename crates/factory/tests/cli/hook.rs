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
        "session_id": "session-1",
        "agent_id": format!("{agent}-1"),
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
fn subagent_stop_lets_the_report_go_when_it_sent_it_back_once() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook.report]\nmax_lines = 5\n");

    assert_eq!(
        answer(&subagent_stop(&sandbox, "implementer", SIX_LINES, false))["decision"],
        "block"
    );
    // Still too long after the revision this hook asked for: asking again would never end.
    let out = subagent_stop(&sandbox, "implementer", SIX_LINES, true);
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", ""),
        "{out:?}"
    );
    // The next time it stops of its own accord, the template holds again.
    assert_eq!(
        answer(&subagent_stop(&sandbox, "implementer", SIX_LINES, false))["decision"],
        "block"
    );
}

#[test]
fn subagent_stop_holds_the_template_when_another_hook_kept_the_agent_going() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook.report]\nmax_lines = 5\n");
    // This hook sent another agent back, which is no reason to let this one go.
    assert_eq!(
        answer(&subagent_stop(&sandbox, "merger", SIX_LINES, false))["decision"],
        "block"
    );

    let out = subagent_stop(&sandbox, "implementer", SIX_LINES, true);

    assert_eq!(answer(&out)["decision"], "block");
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

fn reason_of(out: &Run) -> String {
    let said = answer(out);
    assert_eq!(
        said["hookSpecificOutput"]["permissionDecision"], "deny",
        "{out:?}"
    );
    said["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn allowed(out: &Run) {
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "", ""),
        "{out:?}"
    );
}

#[test]
fn commit_a_takes_tracked_changes_along_but_not_untracked_files() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.repo();
    std::fs::write(repo.join("untracked.txt"), format!("{OWNER}\n")).unwrap();

    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -am 'fix: a'"));

    allowed(&out);
}

#[test]
fn a_targeted_add_checks_its_paths_and_what_is_already_staged() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.repo();
    std::fs::write(repo.join("unrelated.txt"), format!("{OWNER}\n")).unwrap();
    std::fs::write(repo.join("clean.txt"), "fine\n").unwrap();
    // An unrelated untracked file with the address is not part of `git add clean.txt`.
    allowed(&pre_tool(
        &sandbox,
        &bash_in(&repo, "git add clean.txt && git commit -m x"),
    ));
    // The path it names is.
    let reason = reason_of(&pre_tool(
        &sandbox,
        &bash_in(&repo, "git add unrelated.txt && git commit -m x"),
    ));
    assert!(reason.contains("unrelated.txt"), "{reason}");

    // The index keeps the address even when the working tree no longer has it.
    sandbox.git(&repo, &["add", "unrelated.txt"]);
    std::fs::write(repo.join("unrelated.txt"), "scrubbed after staging\n").unwrap();
    let reason = reason_of(&pre_tool(
        &sandbox,
        &bash_in(&repo, "git add clean.txt && git commit -m x"),
    ));
    assert!(reason.contains("unrelated.txt"), "{reason}");
}

#[test]
fn pre_tool_follows_cd_to_the_repo_the_command_commits_in() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);
    let elsewhere = sandbox.path("home");

    let out = pre_tool(
        &sandbox,
        &bash_in(
            &elsewhere,
            &format!("cd {} && git commit -m x", repo.display()),
        ),
    );
    denied_for_the_address(&out);
    // `~` is the home directory the hook runs with.
    let out = pre_tool(
        &sandbox,
        &bash_in(&elsewhere, "cd ~/../repo && git commit -m x"),
    );
    denied_for_the_address(&out);
}

#[test]
fn pre_tool_checks_every_repo_a_command_commits_in() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let leaking = repo_staging_the_address(&sandbox);
    let clean = sandbox.path("clean");
    std::fs::create_dir(&clean).unwrap();
    sandbox.git(&clean, &["init", "-q", "-b", "main"]);
    let (leaking, clean) = (leaking.display(), clean.display());

    for command in [
        format!("cd {clean} && git commit -m a && cd {leaking} && git commit -m b"),
        format!("cd {leaking} && git commit -m a && cd {clean} && git commit -m b"),
        format!("git -C {clean} commit -m a && git -C {leaking} commit -m b"),
    ] {
        let reason = reason_of(&pre_tool(
            &sandbox,
            &bash_in(&sandbox.path("home"), &command),
        ));
        assert!(reason.contains("notes.txt"), "{command}: {reason}");
    }
    let both_clean = format!("cd {clean} && git commit -m a; git -C {clean} push");
    allowed(&pre_tool(
        &sandbox,
        &bash_in(&sandbox.path("home"), &both_clean),
    ));
}

#[test]
fn a_commit_whose_diff_cannot_be_read_is_denied_with_what_to_do() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    // The working directory is no repository.
    let out = pre_tool(&sandbox, &bash_in(&sandbox.path("home"), "git commit -m x"));

    let reason = reason_of(&out);
    assert!(
        reason.contains("could not read") && reason.contains("git -C"),
        "{reason}"
    );
    assert!(!reason.contains('\n'), "{reason}");
}

#[test]
fn a_broken_config_never_echoes_what_was_in_it() {
    let sandbox = Sandbox::new();
    // A string where a list belongs: toml's own message would quote it.
    config(
        &sandbox,
        &format!("[hook]\nprivate_addresses = \"{OWNER}\"\n"),
    );
    let repo = sandbox.repo();

    let reason = reason_of(&pre_tool(&sandbox, &bash_in(&repo, "git commit -m x")));

    assert!(!reason.contains(OWNER), "{reason}");
    assert!(reason.contains("config.toml line 2"), "{reason}");
}

#[test]
fn review_does_not_read_the_hooks_settings() {
    let sandbox = Sandbox::new();
    config(&sandbox, "[hook]\nprivate_addresses = 3\n");
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["review", "main", "t-1"]);

    // It may fail for other reasons (no reviewer is scripted); not for the hook table.
    assert!(!out.stderr.contains("config.toml"), "{out:?}");
}

#[test]
fn a_targeted_update_checks_what_stays_staged_beside_its_paths() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);
    sandbox.git(&repo, &["commit", "-q", "--no-verify", "-m", "x"]);
    // notes.txt is tracked; its next version stages the address and then scrubs the working tree.
    std::fs::write(repo.join("notes.txt"), format!("still {OWNER}\n")).unwrap();
    sandbox.git(&repo, &["add", "notes.txt"]);
    std::fs::write(repo.join("notes.txt"), "scrubbed after staging\n").unwrap();
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();

    // `add -u a.txt` stages a.txt alone: the staged notes.txt goes into the commit as it is.
    let out = pre_tool(
        &sandbox,
        &bash_in(&repo, "git add -u a.txt && git commit -m x"),
    );

    denied_for_the_address(&out);
}

#[test]
fn commit_a_before_the_first_commit_checks_the_working_tree() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.path("fresh");
    std::fs::create_dir(&repo).unwrap();
    sandbox.git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("notes.txt"), "clean when staged\n").unwrap();
    sandbox.git(&repo, &["add", "notes.txt"]);
    std::fs::write(repo.join("notes.txt"), format!("mail {OWNER}\n")).unwrap();

    let out = pre_tool(&sandbox, &bash_in(&repo, "git commit -am 'feat: start'"));

    denied_for_the_address(&out);
}

#[test]
fn a_cd_inside_a_subshell_does_not_move_the_commit_after_it() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);
    let elsewhere = sandbox.path("home");

    let out = pre_tool(
        &sandbox,
        &bash_in(
            &repo,
            &format!("(cd {} && ls) && git commit -m x", elsewhere.display()),
        ),
    );

    denied_for_the_address(&out);
}

#[test]
fn a_commit_naming_a_path_checks_that_paths_working_tree() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.repo();
    // a.txt is tracked; the address is in its working tree only, which `commit a.txt` takes.
    std::fs::write(repo.join("a.txt"), format!("mail {OWNER}\n")).unwrap();

    let reason = reason_of(&pre_tool(
        &sandbox,
        &bash_in(&repo, "git commit -m x a.txt"),
    ));

    assert!(reason.contains("a.txt"), "{reason}");
}

#[test]
fn pre_tool_follows_env_c_and_git_dir_to_the_repo_they_name() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = repo_staging_the_address(&sandbox);
    let elsewhere = sandbox.path("home");
    let repo = repo.display();

    for command in [
        format!("env -C {repo} git commit -m x"),
        format!("git --git-dir {repo}/.git --work-tree {repo} commit -m x"),
        format!("GIT_DIR={repo}/.git GIT_WORK_TREE={repo} git commit -m x"),
    ] {
        denied_for_the_address(&pre_tool(&sandbox, &bash_in(&elsewhere, &command)));
    }
}

#[test]
fn a_check_that_cannot_finish_says_why_when_no_address_is_in_the_why() {
    let sandbox = Sandbox::new();
    leak_config(&sandbox);
    let repo = sandbox.repo();
    for n in 0..201 {
        std::fs::write(repo.join(format!("new-{n}.txt")), "x\n").unwrap();
    }

    let reason = reason_of(&pre_tool(
        &sandbox,
        &bash_in(&repo, "git add . && git commit -m x"),
    ));

    assert!(reason.contains("201 untracked files"), "{reason}");
    assert!(!reason.contains('\n'), "{reason}");
}

/// What a hook call may take: spec #1's 5 ms, where the number means something. A debug build
/// on a shared CI runner, with the whole suite running beside it, cannot promise 5 ms (the runs
/// that failed there took 5.1 and 7.2 ms), so under CI the bound only catches a call that
/// starts doing real work, such as reaching for the daemon or a network. A release build holds
/// the spec's number anywhere.
fn hook_budget() -> std::time::Duration {
    let on_ci = std::env::var_os("CI").is_some();
    if cfg!(debug_assertions) && on_ci {
        std::time::Duration::from_millis(100)
    } else {
        std::time::Duration::from_millis(5)
    }
}

/// Spec #1: "a hook call completes in under 5 ms on this machine". The fastest of a few runs is
/// the call's own cost, free of the scheduler's noise.
#[test]
fn a_hook_call_completes_in_under_5_ms() {
    let sandbox = Sandbox::new();
    config(
        &sandbox,
        &format!("[hook]\nprivate_addresses = [\"{OWNER}\"]\n[hook.report]\nmax_lines = 5\n"),
    );
    let repo = sandbox.repo();
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();
    std::fs::write(repo.join("new.txt"), "new\n").unwrap();
    let calls: [(&str, Value); 4] = [
        ("pre-tool", bash("bun run build && rg -n todo src")),
        // The costliest check: what is staged, and what the command stages before it commits.
        (
            "pre-tool",
            bash_in(&repo, "git add a.txt new.txt && git commit -m x"),
        ),
        (
            "post-tool",
            json!({"tool_name": "Bash", "tool_response": {"stdout": "x".repeat(13_000)}}),
        ),
        (
            "subagent-stop",
            json!({
                "session_id": "session-1",
                "agent_id": "implementer-1",
                "agent_type": "implementer",
                "last_assistant_message": SIX_LINES,
            }),
        ),
    ];
    let budget = hook_budget();
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
        assert!(fastest < budget, "{hook} took {fastest:?}, over {budget:?}");
    }
}
