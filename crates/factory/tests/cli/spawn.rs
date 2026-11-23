//! `factory lead spawn`, against a fake herdr and a real git repo.

use std::fs;

use serde_json::{Value, json};

use crate::support::Sandbox;
use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;

/// Writes the factory config, `$XDG_CONFIG_HOME/factory/config.toml`.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn configure(sandbox: &Sandbox, toml: &str) {
    let dir = sandbox.path("config/factory");
    fs::create_dir_all(&dir).expect("create the factory config dir");
    fs::write(dir.join("config.toml"), toml).expect("write the factory config");
}

/// The ledger's events of one type, parsed.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn events(sandbox: &Sandbox, kind: &str) -> Vec<Value> {
    sandbox
        .ledger_lines()
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("a ledger line is JSON"))
        .filter(|event| event["type"] == kind)
        .collect()
}

/// The prompt lead.sh sent for `#12` on the lead `cart`.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn cart_prompt(sandbox: &Sandbox) -> String {
    let report = sandbox.path("state/factory/leads/repo/cart.md");
    factory_core::lead::prompt(
        "implement-spec",
        "#12",
        "factory/cart",
        &report.to_string_lossy(),
    )
}

#[test]
fn spawn_prints_one_line_naming_the_lead_workspace_branch_worktree_and_report() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    let report = sandbox.path("state/factory/leads/repo/cart.md");
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        out.stdout,
        format!(
            "lead-cart: workspace w1, branch factory/cart, worktree /worktrees/repo/cart, report {}\n",
            report.display()
        )
    );
    assert_eq!(out.stderr, "");
    let created = herdr.requests("worktree.create");
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0]["cwd"], repo.display().to_string());
    assert_eq!(created[0]["branch"], "factory/cart");
    assert_eq!(created[0]["base"], "main");
    assert_eq!(created[0]["label"], "cart");
    let started = herdr.requests("agent.start");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["name"], "lead-cart");
    assert_eq!(started[0]["kind"], "claude");
    assert_eq!(started[0]["pane_id"], "w1:p1");
}

#[test]
fn json_gives_the_same_fields() {
    let sandbox = Sandbox::new();
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    let said: Value = serde_json::from_str(out.stdout.trim()).expect("one JSON line");
    let report = sandbox.path("state/factory/leads/repo/cart.md");
    assert_eq!(
        said,
        json!({
            "lead": "lead-cart",
            "workspace": "w1",
            "branch": "factory/cart",
            "worktree": "/worktrees/repo/cart",
            "report": report.display().to_string(),
        })
    );
}

#[test]
fn the_lead_starts_with_the_configured_plugins_disabled_and_the_lead_prompt() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let started = herdr.requests("agent.start");
    let args: Vec<&str> = started[0]["args"]
        .as_array()
        .expect("agent.start args")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(args[..4], ["--model", "opus", "--effort", "medium"]);
    assert_eq!(args[4], "--settings");
    let settings: Value = serde_json::from_str(args[5]).expect("--settings is JSON");
    assert_eq!(
        settings,
        json!({"enabledPlugins": {
            "expo@claude-plugins-official": false,
            "cloudflare@cloudflare": false,
        }})
    );
    let prompted = herdr.requests("agent.prompt");
    assert_eq!(prompted.len(), 1, "{prompted:?}");
    assert_eq!(prompted[0]["target"], "lead-cart");
    assert_eq!(prompted[0]["text"], cart_prompt(&sandbox));
}

#[test]
fn the_config_file_picks_the_model_effort_and_plugins() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[lead]\nmodel = \"sonnet\"\neffort = \"high\"\ndisable_plugins = [\"a@b\"]\n",
    );

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let args = herdr.requests("agent.start")[0]["args"].clone();
    assert_eq!(
        json!([&args[0], &args[1], &args[2], &args[3]]),
        json!(["--model", "sonnet", "--effort", "high"])
    );
    let settings: Value =
        serde_json::from_str(args[5].as_str().expect("settings text")).expect("--settings is JSON");
    assert_eq!(settings, json!({"enabledPlugins": {"a@b": false}}));
}

#[test]
fn a_lead_start_event_lands_in_the_ledger() {
    let sandbox = Sandbox::new();
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "main", "ship-it"]);

    assert_eq!(out.code, 0, "{out:?}");
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts.len(), 1, "{starts:?}");
    let start = &starts[0];
    for (field, value) in [
        ("repo", "repo"),
        ("slug", "cart"),
        ("spec", "#12"),
        ("skill", "ship-it"),
        ("base", "main"),
        ("branch", "factory/cart"),
        ("outcome", "ok"),
        ("model", "opus"),
        ("effort", "medium"),
    ] {
        assert_eq!(start[field], value, "{field} in {start}");
    }
    assert_eq!(start["stalls"], 0, "{start}");
    assert!(start.get("reason").is_none(), "{start}");
}

#[test]
fn a_lead_end_event_lands_once_the_lead_finishes() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    herdr.await_watch("w1:p1");
    assert_eq!(events(&sandbox, "lead-end"), Vec::<Value>::new());

    // The lead writes its report, then ends its turn.
    let report = sandbox.path("state/factory/leads/repo/cart.md");
    fs::write(&report, "done\n").expect("write the report");
    herdr.status("w1:p1", "idle");

    assert!(
        poll_until(|| !events(&sandbox, "lead-end").is_empty()),
        "no lead-end in {:?}",
        sandbox.ledger_lines()
    );
    let ends = events(&sandbox, "lead-end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["repo"], "repo");
    assert_eq!(ends[0]["slug"], "cart");
    assert_eq!(ends[0]["outcome"], "finished");
    let order: Vec<String> = sandbox
        .ledger_lines()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event["type"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(order, ["lead-start", "lead-end"]);
}

#[test]
fn a_missing_repo_fails_before_anything_starts() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let nowhere = sandbox.path("nowhere");

    let out = sandbox.factory(&[
        "lead",
        "spawn",
        "cart",
        "#12",
        "--repo",
        &nowhere.display().to_string(),
    ]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr_lines(),
        vec![format!(
            "factory: {} is not a git repository (no such directory); run from the repo or pass --repo",
            nowhere.display()
        )]
    );
    assert_eq!(
        herdr.count("worktree.create") + herdr.count("agent.start"),
        0
    );
    assert!(events(&sandbox, "lead-start").is_empty());
}

#[test]
fn a_directory_that_is_no_repo_fails_before_anything_starts() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let plain = sandbox.path("plain");
    fs::create_dir_all(&plain).expect("create a plain dir");

    let out = sandbox.factory_in(&plain, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stderr.starts_with(&format!(
            "factory: {} is not a git repository",
            plain.display()
        )),
        "{out:?}"
    );
    assert_eq!(
        herdr.count("worktree.create") + herdr.count("agent.start"),
        0
    );
    assert!(events(&sandbox, "lead-start").is_empty());
}

#[test]
fn an_existing_branch_fails_before_anything_starts() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    sandbox.git(&repo, &["branch", "factory/cart"]);

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr_lines(),
        vec![
            "factory: factory/cart already exists in repo; pick another slug, or delete the branch"
        ]
    );
    assert_eq!(
        herdr.count("worktree.create") + herdr.count("agent.start"),
        0
    );
    assert!(events(&sandbox, "lead-start").is_empty());
}

#[test]
fn an_unknown_base_is_a_usage_error_before_anything_starts() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "nope"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: unknown base: nope"),
        "{out:?}"
    );
    assert_eq!(
        herdr.count("worktree.create") + herdr.count("agent.start"),
        0
    );
}

#[test]
fn a_report_left_by_an_earlier_lead_is_cleared() {
    let sandbox = Sandbox::new();
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let report = sandbox.path("state/factory/leads/repo/cart.md");
    fs::create_dir_all(report.parent().expect("a report dir")).expect("create the report dir");
    fs::write(&report, "stale\n").expect("write a stale report");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(!report.exists(), "the stale report is still there");
}

#[test]
fn a_trust_dialog_is_answered_with_down_and_enter() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    herdr.refuse("agent.start", "timeout", "agent did not become ready");
    herdr.agent(Some("lead-cart"), "w1:p1", "idle");
    herdr.screen(
        "w1:p1",
        "Do you trust /worktrees/repo/cart?\n  1. Yes, I trust this folder\n  2. No\n",
    );

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let keys: Vec<Value> = herdr
        .requests("agent.send_keys")
        .iter()
        .map(|params| params["keys"].clone())
        .collect();
    assert_eq!(keys, vec![json!(["down"]), json!(["enter"])]);
    assert_eq!(herdr.count("agent.prompt"), 1);
}

#[test]
fn a_start_that_fails_off_the_trust_dialog_is_reported() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    herdr.refuse("agent.start", "timeout", "agent did not become ready");
    herdr.agent(Some("lead-cart"), "w1:p1", "idle");
    herdr.screen("w1:p1", "something else entirely\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(
            "factory: lead-cart is stuck at startup on something other than folder trust"
        ),
        "{out:?}"
    );
    assert_eq!(herdr.count("agent.prompt"), 0);
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0]["outcome"], "error");
}

#[test]
fn a_stalled_prompt_still_on_screen_gets_an_enter() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(&sandbox, "[lead]\nretry_pause = 0\n");
    herdr.refuse("agent.prompt", "agent_prompt_stalled", "stalled");
    // Claude wraps its input box anywhere: the screen holds the prompt with breaks in it.
    herdr.screen("w1:p1", &cart_prompt(&sandbox).replace(' ', "\n  "));

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let keys: Vec<Value> = herdr
        .requests("agent.send_keys")
        .iter()
        .map(|params| params["keys"].clone())
        .collect();
    assert_eq!(keys, vec![json!(["enter"])]);
    assert_eq!(herdr.count("agent.prompt"), 1);
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts[0]["stalls"], 1, "{starts:?}");
}

#[test]
fn a_stalled_prompt_that_is_gone_from_the_screen_is_typed_again() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(&sandbox, "[lead]\nretry_pause = 0\n");
    herdr.refuse("agent.prompt", "agent_prompt_stalled", "stalled");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(herdr.count("agent.prompt"), 2);
    assert_eq!(herdr.count("agent.send_keys"), 0);
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts[0]["stalls"], 1, "{starts:?}");
}

#[test]
fn a_retry_pause_too_long_for_the_cli_to_wait_is_refused_before_anything_starts() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(&sandbox, "[lead]\nretry_pause = 700\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("retry_pause is 700s"), "{out:?}");
    assert_eq!(herdr.count("worktree.create"), 0);
}

#[test]
fn a_repo_named_in_digits_still_gets_one_lead_end_after_a_restart() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.path("123");
    fs::create_dir_all(&repo).expect("create the repo dir");
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        sandbox.git(&repo, &args);
    }
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    let logged = sandbox.factory(&[
        "log",
        "lead-end",
        "repo=123",
        "slug=cart",
        "outcome=finished",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).expect("the daemon's pid");
    sandbox
        .command("kill", &["-9", pid.trim()])
        .output()
        .expect("run kill");
    let gone = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    assert!(poll_until(|| !gone.exists()), "the daemon never died");
    fs::write(sandbox.path("state/factory/leads/123/cart.md"), "done\n").expect("write the report");
    herdr.status("w1:p1", "idle");

    // A second lead, ended the same way, shows the restart has settled both.
    let listed = sandbox.factory(&["lead", "list"]);
    assert_eq!(listed.code, 0, "{listed:?}");
    let spawned = sandbox.factory_in(&repo, &["lead", "spawn", "pay", "#13"]);
    assert_eq!(spawned.code, 0, "{spawned:?}");
    herdr.await_watch("w2:p1");
    fs::write(sandbox.path("state/factory/leads/123/pay.md"), "done\n").expect("write the report");
    herdr.status("w2:p1", "idle");

    assert!(
        poll_until(|| events(&sandbox, "lead-end")
            .iter()
            .any(|end| end["slug"] == "pay")),
        "no lead-end for pay in {:?}",
        sandbox.ledger_lines()
    );
    assert_eq!(events(&sandbox, "lead-end").len(), 2);
}

#[test]
fn an_existing_branch_beats_an_unknown_base() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    sandbox.git(&repo, &["branch", "factory/cart"]);

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "nope"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(herdr.count("worktree.create"), 0);
}

#[test]
fn a_long_state_path_keeps_the_report_in_the_one_line_and_json_keeps_the_worktree() {
    let state = "s".repeat(80);
    let sandbox = Sandbox::new().env("FACTORY_STATE", Sandbox::new().path(&state));
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = out.stdout_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(lines[0].chars().count() <= 200, "{}", lines[0]);
    assert!(
        lines[0].contains(&format!("/{state}/")) && lines[0].contains(".md"),
        "the report path is missing: {}",
        lines[0]
    );
    let json = sandbox.factory_in(&repo, &["lead", "spawn", "pay", "#13", "--json"]);
    let said: Value = serde_json::from_str(json.stdout.trim()).expect("one JSON line");
    assert_eq!(said["worktree"], "/worktrees/repo/pay");
}

#[test]
fn a_restarted_daemon_still_writes_the_end_of_a_lead_it_spawned_and_only_once() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    for (slug, spec) in [("cart", "#12"), ("pay", "#13")] {
        let out = sandbox.factory_in(&repo, &["lead", "spawn", slug, spec]);
        assert_eq!(out.code, 0, "{out:?}");
    }
    // The old daemon wrote cart's end and stopped before it kept that.
    let logged = sandbox.factory(&[
        "log",
        "lead-end",
        "repo=repo",
        "slug=cart",
        "outcome=finished",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).expect("the daemon's pid");
    let killed = sandbox
        .command("kill", &["-9", pid.trim()])
        .output()
        .expect("run kill");
    assert!(killed.status.success(), "{killed:?}");
    // A command sent while it is dying would be reset, not start a new one.
    let gone = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    assert!(poll_until(|| !gone.exists()), "the daemon never died");
    for (slug, pane) in [("cart", "w1:p1"), ("pay", "w2:p1")] {
        fs::write(
            sandbox.path(format!("state/factory/leads/repo/{slug}.md")),
            "done\n",
        )
        .expect("write the report");
        herdr.status(pane, "idle");
    }

    // The next command starts a new daemon, which settles what the old one left.
    let listed = sandbox.factory(&["lead", "list"]);
    assert_eq!(listed.code, 0, "{listed:?}");

    assert!(
        poll_until(|| {
            events(&sandbox, "lead-end")
                .iter()
                .any(|end| end["slug"] == "pay")
        }),
        "no lead-end for pay in {:?}",
        sandbox.ledger_lines()
    );
    let ends: Vec<String> = events(&sandbox, "lead-end")
        .iter()
        .map(|end| end["slug"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(ends, ["cart", "pay"]);
}
