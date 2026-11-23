//! `factory status`: leases, the queue, reviewer quotas, waits, leads and session contexts on one
//! screen, read from the daemon's own state.

use serde_json::Value;

use crate::lab::{Agent, for_owner, lab, waiting};
use crate::monitor::{self, append, subagent_log, usage};
use crate::router::{
    CLEAN_REPORT, claude_writes, codex_out_of_quota, configure, reset_in_two_hours,
};
use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;
use crate::support::{Run, Sandbox, finish, run, spawn};

/// `factory status`, which must succeed.
fn status(sandbox: &Sandbox) -> Run {
    let out = sandbox.factory(&["status"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    out
}

/// `factory status --json`, which must succeed.
fn record(sandbox: &Sandbox) -> Value {
    let out = sandbox.factory(&["status", "--json"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    serde_json::from_str(&out.stdout).unwrap()
}

/// Asks for the screen until `done` accepts it, failing the test when it never does.
fn screen_until(sandbox: &Sandbox, what: &str, done: impl Fn(&[&str]) -> bool) -> Run {
    let mut last = status(sandbox);
    let seen = poll_until(|| {
        last = status(sandbox);
        done(&last.stdout_lines())
    });
    assert!(seen, "never saw {what}; last {last:?}");
    last
}

#[test]
fn an_idle_factory_says_so_in_every_section() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["status"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    let lines = out.stdout_lines();
    assert!(lines[0].starts_with("factory status "), "{out:?}");
    let sections: Vec<&str> = lines
        .iter()
        .filter(|line| !line.starts_with(' '))
        .map(|line| line.split([':', ' ']).next().unwrap_or_default())
        .collect();
    assert_eq!(
        sections,
        ["factory", "lab", "review", "claude", "waits", "leads"],
        "{out:?}"
    );
    assert!(lines.contains(&"lab: idle"), "{out:?}");
    assert!(lines.contains(&"waits: none"), "{out:?}");
}

#[test]
fn json_gives_every_field_of_every_section() {
    let sandbox = Sandbox::new();

    let shown = record(&sandbox);

    let keys = |value: &Value| -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(&shown),
        ["claude", "lab", "leads", "now", "review", "waits"]
    );
    assert_eq!(
        keys(&shown["claude"]),
        ["resets", "until", "used_percent", "verdict"]
    );
    assert_eq!(shown["claude"]["verdict"], "unknown", "{shown}");
    assert_eq!(
        keys(&shown["lab"]),
        ["leases", "queue", "reservations", "slots"]
    );
    assert_eq!(keys(&shown["leads"]), ["error", "leads", "monitor"]);
    assert!(
        shown["now"].as_i64().unwrap() > 1_700_000_000_000,
        "{shown}"
    );
    let names: Vec<&str> = shown["review"]
        .as_array()
        .unwrap()
        .iter()
        .map(|backend| backend["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["agy", "codex", "claude"],
        "the default backends: {shown}"
    );
    assert_eq!(
        keys(&shown["review"][0]),
        [
            "concurrency",
            "name",
            "quota",
            "resets",
            "running",
            "when_busy"
        ]
    );
}

#[test]
fn a_broken_config_is_refused_in_one_line_naming_the_file() {
    let sandbox = Sandbox::new();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"claude\"\nconcurrency = 0\n",
    );

    let out = sandbox.factory(&["status"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("config.toml"), "{out:?}");
}

#[test]
fn a_cooled_backend_shows_when_it_resets_and_the_other_is_available() {
    let sandbox = Sandbox::new().env("TZ", "UTC");
    let repo = sandbox.repo();
    let (reset, until) = reset_in_two_hours();
    sandbox.fake("codex", &codex_out_of_quota(&reset));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));
    let reviewed = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);
    assert_eq!(reviewed.code, 0, "{reviewed:?}");

    let out = status(&sandbox);

    let lines = out.stdout_lines();
    let at = until.to_string();
    let codex = lines.iter().find(|l| l.starts_with("  codex")).unwrap();
    assert!(
        codex.contains(&format!("cooling, resets {}Z (in 2h)", &at[11..16])),
        "{out:?}"
    );
    let claude = lines.iter().find(|l| l.starts_with("  claude")).unwrap();
    assert!(claude.contains("available, 0/"), "{out:?}");
    assert!(lines.contains(&"review quota"), "{out:?}");
    let shown = record(&sandbox);
    assert_eq!(shown["review"][0]["quota"], "unknown", "{shown}");
    assert_eq!(shown["review"][1]["name"], "codex", "{shown}");
    assert_eq!(shown["review"][1]["quota"], "cooling", "{shown}");
    assert_eq!(shown["review"][1]["resets"], until.unix_millis(), "{shown}");
    assert_eq!(shown["review"][2]["quota"], "available", "{shown}");
    assert_eq!(shown["review"][2]["resets"], Value::Null, "{shown}");
}

#[test]
fn leads_show_their_context_and_their_subagents_when_the_steward_runs() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let log = monitor::lead(&sandbox, &herdr, 1, "cart", "working");
    append(&log, &[usage(212_000, false)]);
    let sub = subagent_log(&log, "a1");
    append(&sub, &[usage(170_000, true)]);
    monitor::start(&sandbox);
    monitor::await_status(&sandbox, "the subagent at 170k", |s| {
        s["leads"][0]["subagents"][0]["tokens"] == 170_000 && s["leads"][0]["tokens"] == 212_000
    });

    let out = status(&sandbox);

    let lines = out.stdout_lines();
    assert!(lines.contains(&"leads: 1, monitor on"), "{out:?}");
    let cart = lines.iter().find(|l| l.starts_with("  lead-cart")).unwrap();
    assert_eq!(
        cart.split_whitespace().collect::<Vec<_>>(),
        ["lead-cart", "working", "212k", "1", "sub,", "most", "170k"],
        "{out:?}"
    );
    let shown = record(&sandbox);
    let lead = &shown["leads"]["leads"][0];
    assert_eq!(shown["leads"]["monitor"], true, "{shown}");
    assert_eq!(shown["leads"]["error"], Value::Null, "{shown}");
    assert_eq!(lead["name"], "lead-cart", "{shown}");
    assert_eq!(lead["state"], "working", "{shown}");
    assert_eq!(lead["pane"], "w1:p1", "{shown}");
    assert_eq!(lead["session"], "s1", "{shown}");
    assert_eq!(lead["tokens"], 212_000, "{shown}");
    assert_eq!(lead["subagents"][0]["tokens"], 170_000, "{shown}");
    assert_eq!(lead["subagents"][0]["agent"], "agent-a1", "{shown}");
}

#[test]
fn leads_show_without_context_while_the_steward_is_off() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    monitor::lead(&sandbox, &herdr, 1, "cart", "working");

    let out = screen_until(&sandbox, "the lead", |lines| {
        lines.contains(&"leads: 1, monitor off")
    });

    let cart = out
        .stdout_lines()
        .into_iter()
        .find(|l| l.starts_with("  lead-cart"))
        .unwrap();
    assert_eq!(
        cart.split_whitespace().collect::<Vec<_>>(),
        ["lead-cart", "working", "-"],
        "{out:?}"
    );
    assert_eq!(record(&sandbox)["leads"]["leads"][0]["tokens"], Value::Null);
}

#[test]
fn leads_say_so_in_one_line_when_herdr_is_not_there() {
    let sandbox = Sandbox::new();

    let out = status(&sandbox);

    assert!(
        out.stdout_lines()
            .contains(&"leads: herdr unavailable, see factory lead list"),
        "{out:?}"
    );
    let shown = record(&sandbox);
    assert!(
        shown["leads"]["error"]
            .as_str()
            .is_some_and(|why| why.contains("herdr")),
        "{shown}"
    );
}

#[test]
fn a_registered_wait_shows_who_waits_on_what_until_it_ends() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    monitor::lead(&sandbox, &herdr, 1, "cart", "working");
    monitor::lead(&sandbox, &herdr, 2, "pay", "working");
    let mut cmd = sandbox.factory_command(&["wait", "lead", "pay"]);
    cmd.env("HERDR_PANE_ID", "w1:p1");
    let child = spawn(cmd);

    screen_until(&sandbox, "the wait", |lines| {
        lines.contains(&"  lead-cart -> lead-pay")
    });

    let shown = record(&sandbox);
    assert_eq!(
        shown["waits"],
        serde_json::json!([
            {"pane": "w1:p1", "lead": "lead-cart", "kind": "lead", "on": "lead-pay"}
        ]),
        "{shown}"
    );
    assert!(status(&sandbox).stdout_lines().contains(&"waits: 1"));
    herdr.status("w2:p1", "blocked");
    assert_eq!(finish(child).code, 10);
    let out = screen_until(&sandbox, "the wait to end", |lines| {
        lines.contains(&"waits: none")
    });
    assert!(!out.stdout.contains("lead-pay ->"), "{out:?}");
}

#[test]
fn leases_slots_and_the_queue_come_from_the_lab() {
    let (sandbox, world) = lab();
    let (a, b, c) = (Agent::start(), Agent::start(), Agent::start());
    let [wa, wb, wc] = ["demo-a", "demo-b", "demo-c"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let queued = spawn(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "30"],
    ));
    assert!(poll_until(|| waiting(&sandbox, &wa) == ["demo-c"]));

    let out = status(&sandbox);

    let lines = out.stdout_lines();
    assert!(
        lines.contains(&"lab: 2 leased, 0 free, 1 queued"),
        "{out:?}"
    );
    let leases: Vec<&&str> = lines.iter().filter(|l| l.starts_with("  lease ")).collect();
    assert_eq!(leases.len(), 2, "{out:?}");
    assert!(
        leases[0].contains("demo-a") && leases[0].contains(&format!("sleep:{}", a.pid())),
        "{out:?}"
    );
    assert!(leases[1].contains("demo-b"), "{out:?}");
    let queue: Vec<&&str> = lines
        .iter()
        .filter(|l| l.starts_with("  queued "))
        .collect();
    assert!(queue.len() == 1 && queue[0].contains("demo-c"), "{out:?}");
    let shown = record(&sandbox);
    assert_eq!(
        shown["lab"]["leases"].as_array().unwrap().len(),
        2,
        "{shown}"
    );
    assert_eq!(shown["lab"]["leases"][0]["id"], 1, "{shown}");
    assert_eq!(shown["lab"]["leases"][0]["slot"], 0, "{shown}");
    assert_eq!(
        shown["lab"]["leases"][0]["owner"],
        format!("sleep:{}", a.pid())
    );
    assert_eq!(shown["lab"]["slots"][0]["lease"], 1, "{shown}");
    assert_eq!(shown["lab"]["slots"][1]["lease"], 2, "{shown}");
    assert_eq!(
        shown["lab"]["queue"].as_array().unwrap().len(),
        1,
        "{shown}"
    );
    assert!(
        shown["lab"]["queue"][0]["label"]
            .as_str()
            .is_some_and(|label| label.contains("demo-c")),
        "{shown}"
    );

    let freed = run(for_owner(&sandbox, &wa, a.pid(), &["release"]));
    assert_eq!(freed.code, 0, "{freed:?}");
    assert_eq!(finish(queued).code, 0);
    let after = status(&sandbox);
    assert!(
        after
            .stdout_lines()
            .contains(&"lab: 2 leased, 0 free, 0 queued"),
        "{after:?}"
    );
}

#[test]
fn a_freed_slot_shows_as_free_with_the_worktree_that_had_it() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let wa = world.worktree("demo-a");
    let held = run(for_owner(
        &sandbox,
        &wa,
        a.pid(),
        &["acquire", "--no-device"],
    ));
    assert_eq!(held.code, 0, "{held:?}");
    let freed = run(for_owner(&sandbox, &wa, a.pid(), &["release"]));
    assert_eq!(freed.code, 0, "{freed:?}");

    let out = status(&sandbox);

    assert!(
        out.stdout_lines()
            .contains(&"lab: 0 leased, 1 free, 0 queued"),
        "{out:?}"
    );
    assert!(out.stdout.contains("/0 free, last demo-a\n"), "{out:?}");
}

#[test]
fn five_leads_three_backends_and_a_busy_lab_fit_one_terminal_screen() {
    let (sandbox, world) = lab();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"codex\"\n\n[[review.backend]]\ncli = \"claude\"\n\n\
         [[review.backend]]\ncli = \"agy\"\nname = \"gemini\"\n",
    );
    let herdr = FakeHerdr::start(&sandbox);
    let slugs = [
        "checkout",
        "payments",
        "search-index",
        "onboarding",
        "docs-site",
    ];
    for (n, slug) in (1..).zip(slugs) {
        let log = monitor::lead(&sandbox, &herdr, n, slug, "working");
        append(&log, &[usage(200_000 + u64::from(n) * 1_000, false)]);
    }
    monitor::start(&sandbox);
    let (a, b, c) = (Agent::start(), Agent::start(), Agent::start());
    let [wa, wb, wc] = ["demo-a", "demo-b", "demo-c"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let queued = spawn(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "30"],
    ));
    assert!(poll_until(|| waiting(&sandbox, &wa) == ["demo-c"]));

    let out = screen_until(&sandbox, "all five leads with a context", |lines| {
        lines.iter().filter(|l| l.starts_with("  lead-")).count() == 5
            && lines.iter().all(|l| !l.ends_with("  -"))
    });

    let lines = out.stdout_lines();
    assert!(lines.len() <= 24, "{} rows:\n{}", lines.len(), out.stdout);
    for line in &lines {
        assert!(line.chars().count() <= 80, "{line:?} in\n{}", out.stdout);
    }
    for name in [
        "codex",
        "claude",
        "gemini",
        "lead-checkout",
        "lead-docs-site",
    ] {
        assert!(out.stdout.contains(name), "{name} in\n{}", out.stdout);
    }
    let freed = run(for_owner(&sandbox, &wa, a.pid(), &["release"]));
    assert_eq!(freed.code, 0, "{freed:?}");
    assert_eq!(finish(queued).code, 0);
}

#[test]
fn a_status_runs_no_external_command_and_asks_herdr_nothing() {
    let (sandbox, world) = lab();
    let herdr = FakeHerdr::start(&sandbox);
    monitor::lead(&sandbox, &herdr, 1, "cart", "working");
    let a = Agent::start();
    let held = run(for_owner(
        &sandbox,
        &world.worktree("demo-a"),
        a.pid(),
        &["acquire", "--no-device"],
    ));
    assert_eq!(held.code, 0, "{held:?}");
    // The daemon's first look at herdr is its own, at its start.
    screen_until(&sandbox, "the daemon to know its lead", |lines| {
        lines.contains(&"leads: 1, monitor off")
    });
    let tools = ["adb", "docker", "systemctl", "gh", "codex", "claude", "agy"];
    let calls = |sandbox: &Sandbox| -> Vec<usize> {
        tools.iter().map(|tool| sandbox.calls(tool).len()).collect()
    };
    let (before, herdr_before) = (calls(&sandbox), herdr.methods());

    for _ in 0..3 {
        status(&sandbox);
        record(&sandbox);
    }

    assert_eq!(calls(&sandbox), before);
    assert_eq!(herdr.methods(), herdr_before);
}

#[test]
fn a_status_is_a_screen_of_plain_lines_under_the_output_contract() {
    let sandbox = Sandbox::new();

    let out = status(&sandbox);

    assert!(out.stdout.ends_with('\n'), "{out:?}");
    for line in out.stdout_lines() {
        assert!(line.chars().count() <= 80, "{line:?}");
        assert_eq!(line, line.trim_end(), "{line:?}");
    }
}

#[test]
fn a_reserved_slot_is_the_leads_and_not_free() {
    let (sandbox, world) = lab();
    let (lead, other) = (Agent::start(), Agent::start());
    let held = run(for_owner(
        &sandbox,
        &world.worktree("demo-other"),
        other.pid(),
        &["acquire", "--no-device", "--quiet"],
    ));
    assert_eq!(held.code, 0, "{held:?}");
    let reserved = run(for_owner(
        &sandbox,
        &world.worktree("demo-lead"),
        lead.pid(),
        &["reserve"],
    ));
    assert_eq!(reserved.code, 0, "{reserved:?}");

    let out = status(&sandbox);

    let lines = out.stdout_lines();
    assert!(
        lines.contains(&"lab: 1 leased, 1 reserved, 0 free, 0 queued"),
        "{out:?}"
    );
    let row = lines.iter().find(|l| l.starts_with("  reserved ")).unwrap();
    assert!(
        row.contains("demo-lead") && row.contains(&format!("sleep:{}", lead.pid())),
        "{out:?}"
    );
    let shown = record(&sandbox);
    assert_eq!(shown["lab"]["reservations"][0]["id"], 1, "{shown}");
    assert_eq!(shown["lab"]["reservations"][0]["slot"], 1, "{shown}");
    assert_eq!(
        shown["lab"]["slots"][0]["reservation"],
        Value::Null,
        "{shown}"
    );
}
