//! The hook rules, as tables: each row is one tool call and what the hook says to it.

use factory_core::hook::{
    GitAction, PreTool, RESULT_LIMIT, Template, Templates, mentions_private, post_tool, pre_tool,
    private_address, subagent_stop,
};
use serde_json::{Value, json};

fn bash(command: &str) -> Value {
    json!({"tool_name": "Bash", "tool_input": {"command": command}})
}

fn blocked(call: &Value, escape: bool) -> Option<String> {
    match pre_tool(call, escape) {
        PreTool::Block(reason) => Some(reason),
        _ => None,
    }
}

#[test]
fn pre_tool_blocks_a_js_package_manager_as_the_command_word() {
    for command in [
        "npm install",
        "npx tsc --noEmit",
        "yarn add left-pad",
        "pnpm i",
        "cd app && npm test",
        "echo hi; yarn",
        "ls | pnpm run x",
        "FOO=1 npm run build",
        "sudo npm i -g x",
        "env npm i",
        "time npm test",
        "/usr/bin/npm ci",
        "(npm test)",
        "echo $(npm bin)",
        "echo `npm bin`",
        "bash -c 'npm test'",
        "sh -c \"cd x && yarn\"",
        "npm\ninstall",
        "true && \\\n  npm test",
    ] {
        let reason = blocked(&bash(command), false);
        assert!(reason.is_some(), "{command:?} should be blocked");
    }
}

#[test]
fn pre_tool_lets_everything_else_through() {
    for command in [
        "bun install",
        "bunx tsc",
        "bun run build",
        "git commit -m 'drop npm for bun'",
        "rg npm src",
        "echo npm",
        "man npm",
        "cat package.json",
        "ls npm-stuff",
        "cd npm && ls",
        "git log --grep=yarn",
        "npm_config_x=1 bun x",
        "echo 'a; npm b'",
        "",
    ] {
        assert!(
            matches!(
                pre_tool(&bash(command), false),
                PreTool::Allow | PreTool::CheckStaged(_)
            ),
            "{command:?} should pass"
        );
    }
}

#[test]
fn the_escape_lets_a_package_manager_through_once_set() {
    for command in ["npm install", "cd a && npx tsc"] {
        assert!(matches!(pre_tool(&bash(command), true), PreTool::Allow));
    }
    // Set in the command itself, for the one call.
    for command in [
        "FACTORY_ALLOW_NPM=1 npm install",
        "cd a && FACTORY_ALLOW_NPM=1 pnpm i",
    ] {
        assert!(matches!(pre_tool(&bash(command), false), PreTool::Allow));
    }
    // Only for the command it prefixes.
    assert!(blocked(&bash("FACTORY_ALLOW_NPM=1 bun i; npm i"), false).is_some());
}

#[test]
fn the_package_manager_block_is_one_line_with_the_fix() {
    let reason = blocked(&bash("npm install"), false).unwrap();
    assert!(!reason.contains('\n'), "{reason}");
    assert!(reason.chars().count() <= 200, "{reason}");
    assert!(reason.contains("bun install"), "{reason}");
    assert!(reason.contains("FACTORY_ALLOW_NPM=1"), "{reason}");
    let reason = blocked(&bash("npx tsc"), false).unwrap();
    assert!(reason.contains("bunx"), "{reason}");
}

#[test]
fn other_tools_and_shapes_pass_untouched() {
    for call in [
        json!({}),
        json!({"tool_name": "Read", "tool_input": {"file_path": "npm"}}),
        json!({"tool_name": "Bash"}),
        json!({"tool_name": "Bash", "tool_input": {"command": 7}}),
    ] {
        assert!(matches!(pre_tool(&call, false), PreTool::Allow), "{call}");
    }
}

fn repl(code: &str) -> Value {
    json!({"tool_name": "REPL", "tool_input": {"code": code}})
}

#[test]
fn pre_tool_stops_a_repl_script_that_uses_what_the_repl_lacks() {
    // (script, the word the line must name)
    for (code, word) in [
        ("const home = process.env.HOME;", "process"),
        ("process.exit(1)", "process"),
        ("await $`ls`;\nconst fs = require('fs');", "require"),
        ("const m = BASH_REMATCH[1];", "BASH_REMATCH"),
        ("$`echo ${process.env.HOME}`", "process"),
        ("`a ${`b ${process.pid}`}`", "process"),
        ("const x = a / process.pid / 2;", "process"),
        ("if (ok) { require('x') }", "require"),
        ("const p = (typeof x) + process;", "process"),
        ("const all = [...process.argv];", "process"),
        ("f({...BASH_REMATCH})", "BASH_REMATCH"),
        ("const o = {...require};", "require"),
        ("const y = (a) / process.pid / 2;", "process"),
        ("if (f(x) / process.pid) {}", "process"),
    ] {
        let reason =
            blocked(&repl(code), false).unwrap_or_else(|| panic!("{code:?} should be stopped"));
        assert!(reason.contains(word), "{code:?}: {reason}");
        assert!(!reason.contains('\n'), "{reason}");
        assert!(reason.chars().count() <= 200, "{reason}");
    }
}

#[test]
fn pre_tool_lets_a_repl_script_through_that_only_mentions_those_words() {
    for code in [
        "const x = 'process';",
        "const x = \"it's a require\";",
        "// require it\nconst a = 1;",
        "/* process\n require */ const a = 1;",
        "obj.process = 1; x.require(); y?.process;",
        "const processed = 1, myprocess = 2, requireAll = 3, BASH_REMATCHES = 4;",
        "$`echo process; require`",
        "$`[[ a =~ b ]] && echo \\${BASH_REMATCH[1]}`",
        "const hit = text.match(/require|process/);",
        "const hit = text.split(/[/]process/);",
        "const m = text.match(/(\\d+)/); m[1]",
        "const o = {process: 1}.process;",
        "if (ready) /process/.test(text);",
        "while (more()) /require/.exec(s);",
        "for (;;) /BASH_REMATCH/.test(a);",
        "if (a && (b || c)) /process/.test(t);",
        "",
    ] {
        assert!(
            matches!(pre_tool(&repl(code), false), PreTool::Allow),
            "{code:?} should pass"
        );
    }
}

#[test]
fn the_repl_rule_reads_only_the_repl_and_its_code() {
    for call in [
        json!({"tool_name": "Bash", "tool_input": {"command": "echo", "code": "process"}}),
        json!({"tool_name": "REPL", "tool_input": {"command": "process"}}),
        json!({"tool_name": "REPL", "tool_input": {"code": 4}}),
        json!({"tool_name": "REPL"}),
    ] {
        assert!(matches!(pre_tool(&call, false), PreTool::Allow), "{call}");
    }
}

#[test]
fn the_repl_block_names_the_line_it_found_the_word_on() {
    let reason = blocked(&repl("const a = 1;\n\nconst b = require('x');"), false).unwrap();
    assert!(reason.contains("line 3"), "{reason}");
}

fn result(response: Value) -> Value {
    json!({"tool_name": "Bash", "tool_input": {}, "tool_response": response})
}

#[test]
fn post_tool_answers_a_result_over_the_limit_with_one_line() {
    assert_eq!(RESULT_LIMIT, 12_000);
    let over = "x".repeat(12_001);
    // (response, the characters in all its strings)
    for (response, chars) in [
        (
            json!({"stdout": over, "stderr": "", "interrupted": false}),
            12_001,
        ),
        // The parts add up.
        (
            json!({"stdout": "x".repeat(6_000), "stderr": "y".repeat(6_001)}),
            12_001,
        ),
        (
            json!({"type": "text", "file": {"filePath": "/a", "content": over}}),
            12_007,
        ),
        (json!(over), 12_001),
        (
            json!([{"type": "text", "text": "x".repeat(7_000)}, {"type": "text", "text": "x".repeat(5_001)}]),
            12_009,
        ),
    ] {
        let line = post_tool(&result(response.clone()))
            .unwrap_or_else(|| panic!("{} should be answered", preview(&response)));
        assert!(!line.contains('\n'), "{line}");
        assert!(line.chars().count() <= 200, "{line}");
        assert!(line.contains("rg -n"), "{line}");
        assert!(line.contains(&chars.to_string()), "{line}");
    }
}

fn preview(value: &Value) -> String {
    value.to_string().chars().take(60).collect()
}

#[test]
fn post_tool_says_nothing_to_a_result_within_the_limit() {
    for response in [
        json!({"stdout": "x".repeat(12_000), "stderr": ""}),
        // Characters count, not bytes.
        json!({"stdout": "é".repeat(7_000)}),
        json!({"stdout": "", "interrupted": true, "isImage": false}),
        // Keys and numbers are not output.
        json!({"a".repeat(13_000): 1}),
        json!(null),
        json!("short"),
    ] {
        assert_eq!(
            post_tool(&result(response.clone())),
            None,
            "{}",
            preview(&response)
        );
    }
    assert_eq!(post_tool(&json!({"tool_name": "Bash"})), None);
    assert_eq!(post_tool(&json!({})), None);
}

fn stop(agent: &str, report: &str, active: bool) -> Value {
    json!({
        "hook_event_name": "SubagentStop",
        "agent_type": agent,
        "stop_hook_active": active,
        "last_assistant_message": report,
    })
}

fn lines(n: usize) -> String {
    vec!["a line"; n].join("\n")
}

fn five_lines() -> Templates {
    Templates {
        default: Template {
            max_lines: Some(5),
            max_chars: None,
        },
        agents: Default::default(),
    }
}

#[test]
fn subagent_stop_sends_back_a_report_longer_than_the_template() {
    let reason = subagent_stop(&stop("implementer", &lines(6), false), &five_lines(), false)
        .expect("6 lines is over 5");
    assert!(!reason.contains('\n'), "{reason}");
    assert!(reason.contains('6') && reason.contains('5'), "{reason}");
}

#[test]
fn subagent_stop_lets_a_report_that_fits_stop() {
    let templates = five_lines();
    for report in [
        lines(5),
        lines(1),
        String::new(),
        format!("{}\n\n", lines(5)),
    ] {
        assert_eq!(
            subagent_stop(&stop("x", &report, false), &templates, false),
            None,
            "{report:?}"
        );
    }
}

#[test]
fn subagent_stop_counts_characters_when_the_template_limits_them() {
    let templates = Templates {
        default: Template {
            max_lines: None,
            max_chars: Some(100),
        },
        agents: Default::default(),
    };
    assert_eq!(
        subagent_stop(&stop("x", &"é".repeat(100), false), &templates, false),
        None
    );
    let reason = subagent_stop(&stop("x", &"é".repeat(101), false), &templates, false).unwrap();
    assert!(reason.contains("101") && reason.contains("100"), "{reason}");
}

#[test]
fn subagent_stop_sends_a_report_back_once_per_stop_it_asked_for() {
    // (stop_hook_active, this hook sent the agent back, the report is sent back again)
    for (active, sent_back, again) in [
        // This hook asked for the revision: asking again would never end.
        (true, true, false),
        // Another hook kept the agent going: the template still holds.
        (true, false, true),
        // A fresh stop: what this hook did before is history.
        (false, true, true),
        (false, false, true),
    ] {
        let said = subagent_stop(&stop("x", &lines(50), active), &five_lines(), sent_back);
        assert_eq!(
            said.is_some(),
            again,
            "active {active}, sent back {sent_back}"
        );
    }
}

#[test]
fn subagent_stop_without_a_template_or_a_report_says_nothing() {
    assert_eq!(
        subagent_stop(&stop("x", &lines(500), false), &Templates::default(), false),
        None
    );
    assert_eq!(
        subagent_stop(&json!({"agent_type": "x"}), &five_lines(), false),
        None
    );
    assert_eq!(subagent_stop(&json!({}), &five_lines(), false), None);
}

#[test]
fn a_template_for_an_agent_type_overrides_the_default_field_by_field() {
    let mut templates = Templates {
        default: Template {
            max_lines: Some(40),
            max_chars: Some(5000),
        },
        agents: Default::default(),
    };
    templates.agents.insert(
        "implementer".to_owned(),
        Template {
            max_lines: Some(5),
            max_chars: None,
        },
    );

    // The implementer gets its own line limit and the default character limit.
    assert!(subagent_stop(&stop("implementer", &lines(6), false), &templates, false).is_some());
    assert!(
        subagent_stop(
            &stop("implementer", &"x".repeat(5001), false),
            &templates,
            false
        )
        .is_some()
    );
    assert_eq!(
        subagent_stop(&stop("implementer", &lines(5), false), &templates, false),
        None
    );
    // Any other agent gets the default.
    assert_eq!(
        subagent_stop(&stop("merger", &lines(6), false), &templates, false),
        None
    );
    assert!(subagent_stop(&stop("merger", &lines(41), false), &templates, false).is_some());
}

fn git(command: &str) -> Vec<GitAction> {
    match pre_tool(&bash(command), false) {
        PreTool::CheckStaged(actions) => actions,
        _ => Vec::new(),
    }
}

/// A commit or push in `dir`: `tracked` when it takes every tracked change along, `added` the
/// pathspecs an earlier `git add` named (none named: all of them).
fn action(dir: Option<&str>, tracked: bool, added: Option<&[&str]>) -> GitAction {
    GitAction {
        dir: dir.map(str::to_owned),
        git_dir: None,
        work_tree: None,
        tracked,
        added: added.map(|paths| paths.iter().map(|p| (*p).to_owned()).collect()),
    }
}

#[test]
fn pre_tool_asks_for_the_staged_diff_of_a_commit_or_push() {
    let plain = action(None, false, None);
    let nothing: Vec<GitAction> = Vec::new();
    for (command, expected) in [
        ("git commit -m x", vec![plain.clone()]),
        ("git commit --amend --no-edit", vec![plain.clone()]),
        ("git commit -am x", vec![action(None, true, None)]),
        ("git commit -a", vec![action(None, true, None)]),
        ("git commit --all -m x", vec![action(None, true, None)]),
        ("git commit -m 'add -a flag'", vec![plain.clone()]),
        ("git push", vec![plain.clone()]),
        ("git push origin main", vec![plain.clone()]),
        ("git --no-pager commit -m x", vec![plain.clone()]),
        ("FOO=1 git commit -m x", vec![plain.clone()]),
        ("sudo git push", vec![plain.clone()]),
        ("git commit -m x && git push", vec![plain.clone()]),
        // The directory the command works in.
        (
            "git -C /r commit -m x",
            vec![action(Some("/r"), false, None)],
        ),
        (
            "git -c user.name=x -C r push",
            vec![action(Some("r"), false, None)],
        ),
        (
            "cd r && git commit -m x",
            vec![action(Some("r"), false, None)],
        ),
        (
            "cd /a && git -C b commit -m x",
            vec![action(Some("/a/b"), false, None)],
        ),
        (
            "cd a && cd /b && git push",
            vec![action(Some("/b"), false, None)],
        ),
        (
            "cd ~/proj && git commit -m x",
            vec![action(Some("~/proj"), false, None)],
        ),
        (
            "cd a && git commit -m x && cd ../b && git commit -m y",
            vec![
                action(Some("a"), false, None),
                action(Some("a/../b"), false, None),
            ],
        ),
        // What an earlier `git add` stages is not staged yet when the hook runs.
        (
            "git add -A && git commit -m x",
            vec![action(None, false, Some(&[]))],
        ),
        (
            "git add --all; git commit -m x",
            vec![action(None, false, Some(&[]))],
        ),
        (
            "git add a.txt b/c && git commit -m x",
            vec![action(None, false, Some(&["a.txt", "b/c"]))],
        ),
        (
            "git add -f a && git add b && git commit",
            vec![action(None, false, Some(&["a", "b"]))],
        ),
        (
            "git add . && git commit -m x",
            vec![action(None, false, Some(&["."]))],
        ),
        (
            "git add -u && git commit -m x",
            vec![action(None, true, None)],
        ),
        (
            "git add -- -odd && git commit -m x",
            vec![action(None, false, Some(&["-odd"]))],
        ),
        (
            "git add . && git commit -am x",
            vec![action(None, true, Some(&["."]))],
        ),
        (
            "git add a && git push",
            vec![action(None, false, Some(&["a"]))],
        ),
        (
            "git add . && git commit -m x && git push",
            vec![
                action(None, false, Some(&["."])),
                action(None, false, Some(&["."])),
            ],
        ),
        ("git commit -m x && git add y", vec![plain.clone()]),
        // A repository's staging is its own.
        (
            "git -C a add . && git commit -m x",
            vec![action(None, false, None)],
        ),
        ("git status", nothing.clone()),
        ("git log --grep=commit", nothing.clone()),
        ("git diff --cached", nothing.clone()),
        ("git stash push", nothing.clone()),
        ("git commit-tree abc", nothing.clone()),
        ("git pushall", nothing.clone()),
        ("echo git push", nothing.clone()),
        ("gh pr create", nothing.clone()),
        ("git", nothing),
    ] {
        let mut actions = git(command);
        // A commit and the push after it check the same thing once.
        actions.dedup();
        let mut expected = expected;
        expected.dedup();
        assert_eq!(actions, expected, "{command:?}");
    }
}

#[test]
fn a_cd_in_a_subshell_or_a_child_process_stays_there() {
    for (command, expected) in [
        (
            "(cd sub && git commit -m x) && git push",
            vec![action(Some("sub"), false, None), action(None, false, None)],
        ),
        ("(cd sub); git commit -m x", vec![action(None, false, None)]),
        (
            "(cd a); (cd b; git push)",
            vec![action(Some("b"), false, None)],
        ),
        (
            "cd a && (cd b && git commit) && git push",
            vec![
                action(Some("a/b"), false, None),
                action(Some("a"), false, None),
            ],
        ),
        (
            "echo $(cd sub && pwd); git push",
            vec![action(None, false, None)],
        ),
        ("echo `cd sub`; git push", vec![action(None, false, None)]),
        (
            "bash -c 'cd sub' && git push",
            vec![action(None, false, None)],
        ),
        (
            "bash -c 'cd sub && git push'",
            vec![action(Some("sub"), false, None)],
        ),
        // Each command of a pipeline, and a command sent to the background, is a child.
        ("cd sub | cat; git push", vec![action(None, false, None)]),
        ("echo | cd sub; git push", vec![action(None, false, None)]),
        ("cd sub & git push", vec![action(None, false, None)]),
        (
            "ls | (cd sub; git push)",
            vec![action(Some("sub"), false, None)],
        ),
        // These are no children.
        (
            "cd sub || exit; git push",
            vec![action(Some("sub"), false, None)],
        ),
        (
            "cd sub 2>&1 && git push",
            vec![action(Some("sub"), false, None)],
        ),
        (
            "{ cd sub; git commit; }; git push",
            vec![action(Some("sub"), false, None)],
        ),
    ] {
        let mut actions = git(command);
        actions.dedup();
        assert_eq!(actions, expected, "{command:?}");
    }
}

#[test]
fn a_directory_set_by_pushd_a_wrapper_or_git_itself_is_followed() {
    let at = |dir: &str| action(Some(dir), false, None);
    let repo = |dir: Option<&str>, git_dir: Option<&str>, work_tree: Option<&str>| GitAction {
        git_dir: git_dir.map(str::to_owned),
        work_tree: work_tree.map(str::to_owned),
        ..action(dir, false, None)
    };
    for (command, expected) in [
        ("pushd /b && git commit -m x", vec![at("/b")]),
        (
            "pushd /b; popd; git commit -m x",
            vec![action(None, false, None)],
        ),
        ("pushd /a && pushd b && popd && git push", vec![at("/a")]),
        ("env -C /b git commit -m x", vec![at("/b")]),
        ("env --chdir=/b git commit -m x", vec![at("/b")]),
        ("cd /a && env -C b git push", vec![at("/a/b")]),
        ("sudo -D /b git commit -m x", vec![at("/b")]),
        ("git -C a -C b commit -m x", vec![at("a/b")]),
        (
            "git --git-dir /b/.git --work-tree /b commit -m x",
            vec![repo(None, Some("/b/.git"), Some("/b"))],
        ),
        (
            "git --git-dir=/b/.git commit -m x",
            vec![repo(None, Some("/b/.git"), None)],
        ),
        (
            "GIT_DIR=/b/.git GIT_WORK_TREE=/b git push",
            vec![repo(None, Some("/b/.git"), Some("/b"))],
        ),
        (
            "cd /r && git --work-tree=w commit -m x",
            vec![repo(Some("/r"), None, Some("/r/w"))],
        ),
        // A list sent to the background runs in a child as a whole.
        (
            "cd /b && sleep 1 & git commit -m x",
            vec![action(None, false, None)],
        ),
        ("cd /b &&\ngit push", vec![at("/b")]),
        ("cd /b\ngit push", vec![at("/b")]),
    ] {
        let mut actions = git(command);
        actions.dedup();
        assert_eq!(actions, expected, "{command:?}");
    }
}

#[test]
fn a_commit_that_names_paths_takes_their_working_tree_along() {
    for (command, expected) in [
        (
            "git commit -m x notes.txt",
            action(None, false, Some(&["notes.txt"])),
        ),
        (
            "git commit -i -m x a b",
            action(None, false, Some(&["a", "b"])),
        ),
        ("git commit -o a", action(None, false, Some(&["a"]))),
        (
            "git commit -am 'msg' -- a",
            action(None, true, Some(&["a"])),
        ),
        ("git commit -mfix a", action(None, false, Some(&["a"]))),
        (
            "git commit --message x --author 'A <a>' a",
            action(None, false, Some(&["a"])),
        ),
        (
            "git commit --message=x a",
            action(None, false, Some(&["a"])),
        ),
        ("git commit -F msg.txt", action(None, false, None)),
        ("git commit -C HEAD --amend", action(None, false, None)),
        ("git commit -S -s -m x", action(None, false, None)),
        (
            "git commit --pathspec-from-file=list -m x",
            action(None, false, Some(&[])),
        ),
        (
            "git add a && git commit -m x b",
            action(None, false, Some(&["a", "b"])),
        ),
        (
            "git add -A && git commit -m x b",
            action(None, false, Some(&[])),
        ),
    ] {
        assert_eq!(git(command), vec![expected], "{command:?}");
    }
}

#[test]
fn an_add_that_names_paths_stages_only_those_paths() {
    for (command, expected) in [
        (
            "git add -u a.txt && git commit -m x",
            action(None, false, Some(&["a.txt"])),
        ),
        (
            "git add --update -- a b && git commit -m x",
            action(None, false, Some(&["a", "b"])),
        ),
        (
            "git add -A sub && git commit -m x",
            action(None, false, Some(&["sub"])),
        ),
        ("git add -u && git commit -m x", action(None, true, None)),
        (
            "git add -A && git commit -m x",
            action(None, false, Some(&[])),
        ),
    ] {
        assert_eq!(git(command), vec![expected], "{command:?}");
    }
}

#[test]
fn a_wrappers_option_values_are_not_the_command() {
    for command in [
        "sudo -u root npm i",
        "sudo -u root -g wheel -- npm i",
        "env -u HOME npm i",
        "nice -n 5 npx tsc",
        "doas -u root pnpm i",
        "sudo -E FOO=1 yarn",
    ] {
        assert!(blocked(&bash(command), false).is_some(), "{command:?}");
    }
    assert!(git("sudo -u me git push").len() == 1);
}

#[test]
fn a_wrapper_that_takes_values_before_its_command_still_runs_it() {
    for command in [
        "timeout 60 npm test",
        "timeout -s KILL 5m npx tsc",
        "timeout --kill-after 5 60 yarn",
        "ls | xargs npm i",
        "xargs -n 1 -P 4 pnpm add",
        "stdbuf -oL yarn build",
        "stdbuf -o L npm test",
        "ionice -c 3 npm i",
        "setsid npm start",
        "flock /tmp/lock npm i",
        "flock -w 5 /tmp/lock npm i",
        "chrt 10 npm i",
        "taskset 0x1 npm i",
    ] {
        assert!(blocked(&bash(command), false).is_some(), "{command:?}");
    }
    for command in [
        "timeout 60 bun test",
        "xargs echo npm",
        "flock /tmp/npm bun i",
    ] {
        assert!(blocked(&bash(command), false).is_none(), "{command:?}");
    }
}

#[test]
fn a_package_manager_is_refused_before_the_diff_is_asked_for() {
    assert!(blocked(&bash("npm i && git commit -m x"), false).is_some());
}

#[test]
fn a_heredoc_body_is_text_not_commands() {
    assert!(matches!(
        pre_tool(&bash("cat <<EOF > notes\nnpm install\nEOF\nls"), false),
        PreTool::Allow
    ));
    assert!(matches!(
        pre_tool(&bash("cat <<-'EOF'\n\tnpm install\n\tEOF\nls"), false),
        PreTool::Allow
    ));
    assert!(blocked(&bash("cat <<'EOF'\nnpm i\nEOF\nnpm i"), false).is_some());
    assert!(git("cat <<EOF\ngit push\nEOF").is_empty());
}

const DIFF: &str = "diff --git a/src/a.txt b/src/a.txt
index 111..222 100644
--- a/src/a.txt
+++ b/src/a.txt
@@ -1,3 +1,4 @@
 context
-old line
+new line by Me@Example.Invalid today
+++ not a header
diff --git a/b.txt b/b.txt
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-me@example.invalid removed
+clean
";

fn addresses(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| (*a).to_owned()).collect()
}

#[test]
fn a_staged_diff_adding_a_private_address_is_refused_naming_the_file() {
    let reason = private_address(DIFF, &addresses(&["me@example.invalid"])).unwrap();
    assert!(reason.contains("src/a.txt"), "{reason}");
    assert!(
        !reason.contains("example.invalid"),
        "the address stays out of the line: {reason}"
    );
    assert!(
        !reason.contains('\n') && reason.chars().count() <= 200,
        "{reason}"
    );
}

#[test]
fn a_private_address_that_is_only_removed_or_in_context_passes() {
    let removed = "diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-me@example.invalid\n+clean\n";
    let context = "diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1,2 +1,2 @@\n me@example.invalid\n-a\n+b\n";
    for diff in [removed, context, ""] {
        assert_eq!(
            private_address(diff, &addresses(&["me@example.invalid"])),
            None,
            "{diff}"
        );
    }
}

#[test]
fn with_no_address_configured_nothing_is_private() {
    assert_eq!(private_address(DIFF, &[]), None);
    // An empty entry would match every line.
    assert_eq!(private_address(DIFF, &addresses(&[""])), None);
    assert!(private_address(DIFF, &addresses(&["", "ME@EXAMPLE.INVALID"])).is_some());
}

#[test]
fn any_of_several_addresses_is_private() {
    let diff =
        "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -0,0 +1 @@\n+reach me at other@example.invalid\n";
    let reason = private_address(
        diff,
        &addresses(&["me@example.invalid", "other@example.invalid"]),
    );
    assert!(reason.unwrap().contains(" x"));
}

#[test]
fn a_file_named_with_the_private_address_is_not_named_in_the_line() {
    let diff = "diff --git a/me@example.invalid.txt b/me@example.invalid.txt\n--- a/me@example.invalid.txt\n+++ b/me@example.invalid.txt\n@@ -0,0 +1 @@\n+me@example.invalid\n";
    let reason = private_address(diff, &addresses(&["me@example.invalid"])).unwrap();
    assert!(
        !reason.to_lowercase().contains("example.invalid"),
        "{reason}"
    );
}

#[test]
fn a_new_or_renamed_file_named_with_the_address_is_refused_even_with_clean_content() {
    let created = "diff --git a/me@example.invalid.txt b/me@example.invalid.txt\nnew file mode 100644\nindex 0000000..e69de29\n";
    let renamed = "diff --git a/old.txt b/me@example.invalid.txt\nsimilarity index 100%\nrename from old.txt\nrename to me@example.invalid.txt\n";
    let deleted = "diff --git a/me@example.invalid.txt b/me@example.invalid.txt\ndeleted file mode 100644\n--- a/me@example.invalid.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n";
    let addresses = addresses(&["me@example.invalid"]);
    for diff in [created, renamed] {
        let reason = private_address(diff, &addresses).expect(diff);
        assert!(
            !reason.to_lowercase().contains("example.invalid"),
            "{reason}"
        );
    }
    assert_eq!(private_address(deleted, &addresses), None);
}

#[test]
fn a_wrappers_long_options_take_their_values_too() {
    for command in [
        "sudo --user root npm i",
        "sudo --user=root npm i",
        "env --unset HOME npm i",
    ] {
        assert!(blocked(&bash(command), false).is_some(), "{command:?}");
    }
}

#[test]
fn a_line_mentions_a_private_address_as_a_diff_would_add_it() {
    let list = addresses(&["Owner@Example.invalid", " "]);
    for (text, mentions) in [
        ("cannot read notes-owner@example.invalid.txt: denied", true),
        ("git diff failed: fatal: not a git repository", false),
        ("", false),
    ] {
        assert_eq!(mentions_private(text, &list), mentions, "{text:?}");
    }
    assert!(!mentions_private("anything", &addresses(&[" "])));
}
