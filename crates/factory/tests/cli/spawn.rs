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
        false,
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

/// What Claude was started with, from herdr's `agent.start`.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn started_args(herdr: &FakeHerdr) -> Vec<String> {
    herdr.requests("agent.start")[0]["args"]
        .as_array()
        .expect("agent.start args")
        .iter()
        .filter_map(|arg| arg.as_str().map(str::to_owned))
        .collect()
}

#[test]
fn model_and_effort_flags_beat_the_config_and_land_on_the_start_event() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(&sandbox, "[lead]\nmodel = \"sonnet\"\neffort = \"high\"\n");

    let out = sandbox.factory_in(
        &repo,
        &[
            "lead", "spawn", "cart", "#12", "--model", "opus", "--effort", "xhigh",
        ],
    );

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        started_args(&herdr)[..4],
        ["--model", "opus", "--effort", "xhigh"]
    );
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts[0]["model"], "opus", "{starts:?}");
    assert_eq!(starts[0]["effort"], "xhigh", "{starts:?}");
    let prompted = herdr.requests("agent.prompt");
    assert_eq!(prompted[0]["text"], cart_prompt(&sandbox));
}

#[test]
fn one_flag_leaves_the_other_to_the_config() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(&sandbox, "[lead]\nmodel = \"sonnet\"\neffort = \"high\"\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "--effort", "max"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        started_args(&herdr)[..4],
        ["--model", "sonnet", "--effort", "max"]
    );
}

#[test]
fn an_unknown_effort_is_a_usage_error_before_anything_starts() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(
        &repo,
        &["lead", "spawn", "cart", "#12", "--effort", "turbo"],
    );

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("turbo"), "{out:?}");
    assert_eq!(out.stderr.lines().count(), 1, "{out:?}");
    assert_eq!(herdr.count("worktree.create"), 0);
    assert!(events(&sandbox, "lead-start").is_empty());
}

#[test]
fn every_effort_claude_takes_is_accepted() {
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let sandbox = Sandbox::new();
        let herdr = FakeHerdr::start(&sandbox);
        let repo = sandbox.repo();

        let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "--effort", effort]);

        assert_eq!(out.code, 0, "{effort}: {out:?}");
        assert_eq!(started_args(&herdr)[3], effort);
    }
}

#[test]
fn an_attended_lead_is_not_told_it_runs_unattended() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "--attended"]);

    assert_eq!(out.code, 0, "{out:?}");
    let report = sandbox.path("state/factory/leads/repo/cart.md");
    let text = herdr.requests("agent.prompt")[0]["text"]
        .as_str()
        .expect("the prompt")
        .to_owned();
    assert!(!text.contains("unattended"), "{text}");
    assert!(!text.contains("decide from the spec"), "{text}");
    assert!(
        text.starts_with("/implement-spec #12. Integration branch: factory/cart"),
        "{text}"
    );
    assert!(text.contains("run devlab release"), "{text}");
    assert!(
        text.ends_with(&format!("write your final report to {}.", report.display())),
        "{text}"
    );
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts[0]["attended"], "true", "{starts:?}");
}

#[test]
fn a_lead_is_unattended_unless_told_otherwise_and_the_event_says_so() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let text = herdr.requests("agent.prompt")[0]["text"]
        .as_str()
        .expect("the prompt")
        .to_owned();
    assert!(
        text.contains("You run unattended: wherever a skill says to ask"),
        "{text}"
    );
    assert_eq!(events(&sandbox, "lead-start")[0]["attended"], "false");
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
    // The build names itself the way `--version` does: a short sha, maybe dirty, or unknown.
    let build = start["build"].as_str().expect("build in the start event");
    let sha = build.strip_suffix("-dirty").unwrap_or(build);
    assert!(
        sha == "unknown" || (sha.len() == 7 && sha.bytes().all(|b| b.is_ascii_hexdigit())),
        "{start}"
    );
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
            "factory: lead.sh: {} is not a git repository (no such directory); run from the repo or pass --repo",
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
            "factory: lead.sh: {} is not a git repository",
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
            "factory: lead.sh: factory/cart already exists in repo; pick another slug, or delete the branch"
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
        out.stderr
            .starts_with("factory: lead.sh: unknown base: nope"),
        "{out:?}"
    );
    assert_eq!(
        herdr.count("worktree.create") + herdr.count("agent.start"),
        0
    );
}

/// factory-stats sorts a failure that says `lead.sh: ` as the lead script's, so the text stays.
#[test]
fn a_spawn_failure_keeps_the_lead_sh_prefix_factory_stats_matches() {
    let sandbox = Sandbox::new();
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    configure(&sandbox, "[lead]\nretry_pause = 700\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    let json = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12", "--json"]);

    assert!(out.stderr.starts_with("factory: lead.sh: "), "{out:?}");
    let said: Value = serde_json::from_str(json.stdout.trim()).expect("one JSON line");
    assert!(
        said["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("lead.sh: ")),
        "{said}"
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
fn a_freshly_started_claude_gets_its_prompt_only_once_it_is_ready() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let methods = herdr.methods();
    let ready = methods
        .iter()
        .rposition(|method| method == "agent.get")
        .expect("spawn asked herdr whether the agent was ready");
    let prompted = methods
        .iter()
        .position(|method| method == "agent.prompt")
        .expect("spawn typed the prompt");
    assert!(
        ready < prompted,
        "prompted before it was ready: {methods:?}"
    );
    assert_eq!(herdr.count("agent.prompt"), 1);
    assert_eq!(herdr.count("agent.send_keys"), 0);
}

#[test]
fn a_trust_dialog_met_while_coming_up_is_answered_with_down_and_enter() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    herdr.blocks_at_startup();
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

/// herdr refuses `agent.start` with `agent_pane_busy` while the pane's shell still runs its rc,
/// which is the case right after `worktree.create`.
#[test]
fn a_pane_still_busy_with_its_shell_is_retried_until_it_takes_the_agent() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    for _ in 0..3 {
        herdr.refuse(
            "agent.start",
            "agent_pane_busy",
            "agent target pane w1:p1 is not an available shell",
        );
    }
    // A bare shell mid-rc: were the refusal taken for the trust path, this is what it would read.
    herdr.screen("w1:p1", "user@host ~ $ \n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(herdr.count("agent.start"), 4);
    assert_eq!(herdr.count("agent.send_keys"), 0);
    assert_eq!(herdr.count("agent.prompt"), 1);
    // Three asks were refused, so three retries, and the event keeps the count.
    assert_eq!(events(&sandbox, "lead-start")[0]["busy_retries"], 3);
}

#[test]
fn a_pane_that_is_never_busy_records_no_retries() {
    let sandbox = Sandbox::new();
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(events(&sandbox, "lead-start")[0]["busy_retries"], 0);
}

#[test]
fn a_pane_that_stays_busy_fails_the_spawn_naming_herdrs_code() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    for _ in 0..200 {
        herdr.refuse(
            "agent.start",
            "agent_pane_busy",
            "agent target pane w1:p1 is not an available shell",
        );
    }

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("could not start Claude") && out.stderr.contains("agent_pane_busy"),
        "{out:?}"
    );
    assert!(herdr.count("agent.start") > 4);
    assert_eq!(herdr.count("agent.read"), 0);
    assert_eq!(herdr.count("agent.prompt"), 0);
    // The failed start still says how long it pushed: one retry per refusal but the last.
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts[0]["outcome"], "error", "{starts:?}");
    let retries = starts[0]["busy_retries"].as_u64().expect("busy_retries");
    assert_eq!(retries, herdr.count("agent.start") as u64 - 1, "{starts:?}");
}

#[test]
fn a_start_that_fails_off_the_trust_dialog_is_reported() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    herdr.refuse(
        "agent.start",
        "agent_not_ready",
        "agent lead-cart is blocked during startup",
    );
    herdr.agent(Some("lead-cart"), "w1:p1", "idle");
    herdr.screen("w1:p1", "something else entirely\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(
            "factory: lead.sh: lead-cart is stuck at startup on something other than folder trust"
        ),
        "{out:?}"
    );
    // herdr's own words ride along, in the same one line.
    assert!(
        out.stderr.contains("agent_not_ready") && out.stderr.contains("blocked during startup"),
        "{out:?}"
    );
    assert_eq!(out.stderr.lines().count(), 1, "{out:?}");
    assert_eq!(herdr.count("agent.prompt"), 0);
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0]["outcome"], "error");
}

/// A start that herdr refuses for any other reason is not a stop at startup: the line carries
/// herdr's own error, and nothing says the lead is stuck on a dialog.
#[test]
fn a_start_herdr_refuses_for_another_reason_carries_herdrs_error() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    herdr.refuse(
        "agent.start",
        "agent_name_taken",
        "an agent named lead-cart already exists",
    );
    herdr.screen("w1:p1", "something else entirely\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with("factory: lead.sh: lead-cart could not start Claude: herdr refused (agent_name_taken): an agent named lead-cart already exists"),
        "{out:?}"
    );
    assert!(!out.stderr.contains("stuck at startup"), "{out:?}");
    assert_eq!(out.stderr.lines().count(), 1, "{out:?}");
    assert!(out.stderr.len() < 400, "{out:?}");
    assert_eq!(herdr.count("agent.prompt"), 0);
    let starts = events(&sandbox, "lead-start");
    assert_eq!(starts[0]["outcome"], "error", "{starts:?}");
    assert!(
        starts[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("agent_name_taken")),
        "{starts:?}"
    );
}

#[test]
fn a_long_multi_line_herdr_error_still_fits_one_line() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let message = format!("first line\nsecond line {}", "x".repeat(600));
    herdr.refuse("agent.start", "agent_name_taken", &message);
    herdr.screen("w1:p1", "something else entirely\n");

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr.lines().count(), 1, "{out:?}");
    assert!(out.stderr.contains("first line second line xxx"), "{out:?}");
    assert!(out.stderr.chars().count() < 400, "{out:?}");
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
            "user.email=ping@cptdawn.dev",
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

/// Kills the daemon as a crash would, and waits until it is gone.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn crash_daemon(sandbox: &Sandbox) {
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).expect("the daemon's pid");
    let killed = sandbox
        .command("kill", &["-9", pid.trim()])
        .output()
        .expect("run kill");
    assert!(killed.status.success(), "{killed:?}");
    // A command sent while it is dying would be reset, not start a new one.
    let gone = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    assert!(poll_until(|| !gone.exists()), "the daemon never died");
}

/// Spawns `slug`, lets it finish, and waits for its `lead-end`: once it lands, the daemon has
/// settled every lead it knew of.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn spawn_and_finish(
    sandbox: &Sandbox,
    herdr: &FakeHerdr,
    repo: &std::path::Path,
    slug: &str,
    pane: &str,
) {
    let out = sandbox.factory_in(repo, &["lead", "spawn", slug, "#99"]);
    assert_eq!(out.code, 0, "{out:?}");
    herdr.await_watch(pane);
    fs::write(
        sandbox.path(format!("state/factory/leads/repo/{slug}.md")),
        "done\n",
    )
    .expect("write the report");
    herdr.status(pane, "idle");
    assert!(
        poll_until(|| events(sandbox, "lead-end")
            .iter()
            .any(|end| end["slug"] == slug)),
        "no lead-end for {slug} in {:?}",
        sandbox.ledger_lines()
    );
}

#[test]
fn a_lead_whose_agent_dies_without_a_report_ends_dead() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    herdr.await_watch("w1:p1");

    herdr.close("w1:p1");

    assert!(
        poll_until(|| !events(&sandbox, "lead-end").is_empty()),
        "no lead-end in {:?}",
        sandbox.ledger_lines()
    );
    let ends = events(&sandbox, "lead-end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["slug"], "cart");
    assert_eq!(ends[0]["outcome"], "dead");
}

#[test]
fn a_lead_that_finishes_between_the_listing_and_the_subscription_still_ends() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    crash_daemon(&sandbox);
    fs::write(sandbox.path("state/factory/leads/repo/cart.md"), "done\n")
        .expect("write the report");

    // The new daemon lists cart working; it goes idle before the subscription is up.
    herdr.change_before_subscribing("w1:p1", "idle");
    let listed = sandbox.factory(&["lead", "list"]);
    assert_eq!(listed.code, 0, "{listed:?}");

    assert!(
        poll_until(|| !events(&sandbox, "lead-end").is_empty()),
        "no lead-end in {:?}",
        sandbox.ledger_lines()
    );
    assert_eq!(events(&sandbox, "lead-end")[0]["outcome"], "finished");
}

#[test]
fn an_idle_event_newer_than_a_listing_is_not_undone_by_it() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let report = sandbox.path("state/factory/leads/repo/cart.md");
    // The event is on the subscription before the listing that predates it is answered.
    herdr.overtake_listing("w1:p1", "idle", move || {
        fs::write(&report, "done\n").expect("write the report");
    });

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");

    assert!(
        poll_until(|| !events(&sandbox, "lead-end").is_empty()),
        "no lead-end in {:?}",
        sandbox.ledger_lines()
    );
}

#[test]
fn a_lead_whose_start_never_reached_the_ledger_gets_no_end_either() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    // The daemon kept cart as running, then died before its lead-start was written.
    crash_daemon(&sandbox);
    let ledger = sandbox.path("state/factory/events.jsonl");
    fs::write(&ledger, "").expect("empty the ledger");
    fs::write(sandbox.path("state/factory/leads/repo/cart.md"), "done\n")
        .expect("write the report");
    herdr.status("w1:p1", "idle");

    spawn_and_finish(&sandbox, &herdr, &repo, "pay", "w2:p1");

    let ends: Vec<Value> = events(&sandbox, "lead-end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["slug"], "pay");
}

/// The details file a spawn line names when the paths are too long for one line, read back.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn details_named_in(line: &str) -> Value {
    let file = line
        .split(" in ")
        .last()
        .unwrap_or_else(|| panic!("no file named in {line}"));
    let text = fs::read_to_string(file).unwrap_or_else(|e| panic!("cannot read {file}: {e}"));
    serde_json::from_str(text.trim()).expect("the details file is JSON")
}

#[test]
fn a_long_state_path_leaves_the_worktree_and_report_in_a_file_the_line_names() {
    // A report path of 120 characters: too long for the full line, not for the short one.
    let outer = Sandbox::new();
    let base = outer.path("").display().to_string();
    let state =
        "s".repeat(120 - "/leads/repo/cart.md".len() - base.trim_end_matches('/').len() - 1);
    let sandbox = Sandbox::new().env("FACTORY_STATE", outer.path(&state));
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = out.stdout_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(lines[0].chars().count() <= 200, "{}", lines[0]);
    assert!(
        lines[0]
            .starts_with("lead-cart: workspace w1, branch factory/cart; worktree and report in "),
        "{}",
        lines[0]
    );
    let details = details_named_in(lines[0]);
    assert_eq!(details["worktree"], "/worktrees/repo/cart");
    assert!(
        details["report"]
            .as_str()
            .is_some_and(|report| report.ends_with(&format!("/{state}/leads/repo/cart.md"))),
        "{details}"
    );
}

#[test]
fn a_state_path_too_long_for_any_line_still_names_its_file_whole() {
    let state = ["s".repeat(100), "t".repeat(100), "u".repeat(100)].join("/");
    let sandbox = Sandbox::new().env("FACTORY_STATE", Sandbox::new().path(&state));
    let _herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);

    assert_eq!(out.code, 0, "{out:?}");
    let lines = out.stdout_lines();
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(!lines[0].contains('…'), "cut: {}", lines[0]);
    assert_eq!(
        details_named_in(lines[0])["worktree"],
        "/worktrees/repo/cart"
    );
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

#[test]
fn a_lead_end_logged_for_a_spawned_lead_keeps_the_daemon_from_writing_a_second() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    for (slug, spec) in [("cart", "#12"), ("pay", "#13")] {
        let out = sandbox.factory_in(&repo, &["lead", "spawn", slug, spec]);
        assert_eq!(out.code, 0, "{out:?}");
    }
    herdr.await_watch("w1:p1");
    herdr.await_watch("w2:p1");
    let logged = sandbox.factory(&[
        "log",
        "lead-end",
        "repo=repo",
        "slug=cart",
        "outcome=parked",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");
    for (slug, pane) in [("cart", "w1:p1"), ("pay", "w2:p1")] {
        fs::write(
            sandbox.path(format!("state/factory/leads/repo/{slug}.md")),
            "done\n",
        )
        .expect("write the report");
        herdr.status(pane, "idle");
    }

    // The daemon settles every lead it knew of in one pass, so pay's end shows cart's is skipped.
    assert!(
        poll_until(|| events(&sandbox, "lead-end")
            .iter()
            .any(|end| end["slug"] == "pay")),
        "no lead-end for pay in {:?}",
        sandbox.ledger_lines()
    );
    let ends: Vec<(String, String)> = events(&sandbox, "lead-end")
        .iter()
        .map(|end| {
            (
                end["slug"].as_str().unwrap_or_default().to_owned(),
                end["outcome"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        ends,
        [
            ("cart".to_owned(), "parked".to_owned()),
            ("pay".to_owned(), "finished".to_owned())
        ]
    );
}

#[test]
fn a_lead_respawned_after_its_logged_end_still_gets_its_own_end() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    herdr.await_watch("w1:p1");
    let logged = sandbox.factory(&[
        "log",
        "lead-end",
        "repo=repo",
        "slug=cart",
        "outcome=parked",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");
    herdr.close("w1:p1");
    // A barrier: pay's end lands after the daemon has settled cart, whose own end it skips.
    spawn_and_finish(&sandbox, &herdr, &repo, "pay", "w2:p1");

    let again = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(again.code, 0, "{again:?}");
    herdr.await_watch("w3:p1");
    fs::write(sandbox.path("state/factory/leads/repo/cart.md"), "done\n")
        .expect("write the report");
    herdr.status("w3:p1", "idle");

    assert!(
        poll_until(|| events(&sandbox, "lead-end")
            .iter()
            .any(|end| end["slug"] == "cart" && end["outcome"] == "finished")),
        "no finished lead-end for cart in {:?}",
        sandbox.ledger_lines()
    );
    let outcomes: Vec<String> = events(&sandbox, "lead-end")
        .iter()
        .filter(|end| end["slug"] == "cart")
        .map(|end| end["outcome"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(outcomes, ["parked", "finished"]);
}

#[test]
fn a_failed_start_of_a_running_lead_leaves_it_its_own_end() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    herdr.await_watch("w1:p1");
    herdr.refuse("worktree.create", "internal", "the disk is full");
    let failed = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(failed.code, 3, "{failed:?}");
    let outcomes: Vec<Value> = events(&sandbox, "lead-start")
        .iter()
        .map(|start| start["outcome"].clone())
        .collect();
    assert_eq!(outcomes, ["ok", "error"]);

    fs::write(sandbox.path("state/factory/leads/repo/cart.md"), "done\n")
        .expect("write the report");
    herdr.status("w1:p1", "idle");

    assert!(
        poll_until(|| !events(&sandbox, "lead-end").is_empty()),
        "no lead-end for cart in {:?}",
        sandbox.ledger_lines()
    );
    let ends = events(&sandbox, "lead-end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "finished");
}

#[test]
fn a_failed_start_after_a_logged_park_gets_no_second_end_on_a_restart() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let repo = sandbox.repo();
    let out = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(out.code, 0, "{out:?}");
    herdr.await_watch("w1:p1");
    let logged = sandbox.factory(&[
        "log",
        "lead-end",
        "repo=repo",
        "slug=cart",
        "outcome=parked",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");
    herdr.refuse("worktree.create", "internal", "the disk is full");
    let failed = sandbox.factory_in(&repo, &["lead", "spawn", "cart", "#12"]);
    assert_eq!(failed.code, 3, "{failed:?}");
    // The daemon dies before it settles cart, whose database row is still open.
    crash_daemon(&sandbox);
    herdr.close("w1:p1");

    // A barrier: pay's end lands after the restarted daemon has settled what it inherited.
    spawn_and_finish(&sandbox, &herdr, &repo, "pay", "w2:p1");

    let outcomes: Vec<String> = events(&sandbox, "lead-end")
        .iter()
        .filter(|end| end["slug"] == "cart")
        .map(|end| end["outcome"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(outcomes, ["parked"]);
}
