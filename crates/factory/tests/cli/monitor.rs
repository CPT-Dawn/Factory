//! `factory monitor start|stop|status`, the context steward, against a fake herdr and session
//! logs under the sandbox's `~/.claude/projects`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;
use crate::support::{self, Sandbox, finish, spawn};

#[test]
fn starting_twice_is_a_no_op_and_stopping_stops_it() {
    let sandbox = Sandbox::new();
    let _herdr = FakeHerdr::start(&sandbox);
    let said = |args: &[&str]| {
        let out = sandbox.factory(args);
        assert_eq!(
            (out.code, out.stderr.as_str()),
            (0, ""),
            "{args:?}: {out:?}"
        );
        out.stdout
    };

    assert_eq!(said(&["monitor", "status"]), "monitor off\n");
    assert_eq!(said(&["monitor", "start"]), "monitor started\n");
    assert_eq!(said(&["monitor", "start"]), "monitor already running\n");
    assert_eq!(
        said(&["monitor", "status"]),
        "monitor on: 0 leads, 0 subagents\n"
    );
    assert_eq!(said(&["monitor", "stop"]), "monitor stopped\n");
    assert_eq!(said(&["monitor", "stop"]), "monitor was not running\n");
    assert_eq!(said(&["monitor", "status"]), "monitor off\n");
}

#[test]
fn without_herdr_the_monitor_does_not_start() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["monitor", "start"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    let socket = sandbox.path("config/herdr/herdr.sock");
    let prefix = format!("factory: herdr unreachable at {}: ", socket.display());
    assert!(out.stderr.starts_with(&prefix), "{out:?}");
    assert_eq!(
        sandbox.factory(&["monitor", "status"]).stdout,
        "monitor off\n"
    );
}

/// Where Claude Code keeps the log of session `id` run in `cwd`: every character of the cwd that
/// isn't a letter or digit becomes `-`.
pub(crate) fn session_log(sandbox: &Sandbox, cwd: &str, id: &str) -> PathBuf {
    let project: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    sandbox
        .path("home/.claude/projects")
        .join(project)
        .join(format!("{id}.jsonl"))
}

/// The log of subagent `agent-<id>` of the session whose log is `lead_log`.
pub(crate) fn subagent_log(lead_log: &Path, id: &str) -> PathBuf {
    lead_log
        .with_extension("")
        .join("subagents")
        .join(format!("agent-{id}.jsonl"))
}

/// An assistant line whose usage puts the context at `tokens`, as Claude Code logs it.
pub(crate) fn usage(tokens: u64, sidechain: bool) -> String {
    json!({
        "type": "assistant",
        "isSidechain": sidechain,
        "message": {"role": "assistant", "usage": {
            "input_tokens": 3,
            "cache_creation_input_tokens": 1000,
            "cache_read_input_tokens": tokens - 1003,
            "output_tokens": 50,
        }},
    })
    .to_string()
}

/// Appends `lines` to the log at `path`, making its directories.
pub(crate) fn append(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn configure(sandbox: &Sandbox, toml: &str) {
    let dir = sandbox.path("config/factory");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), toml).unwrap();
}

fn status(sandbox: &Sandbox) -> Value {
    let out = sandbox.factory(&["monitor", "status", "--json"]);
    assert_eq!(out.code, 0, "{out:?}");
    serde_json::from_str(&out.stdout).unwrap()
}

/// Asks `monitor status --json` until `done` accepts it, failing the test after [`DEADLINE`].
pub(crate) fn await_status(sandbox: &Sandbox, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    let seen = poll_until(|| {
        last = status(sandbox);
        done(&last)
    });
    assert!(seen, "never saw {what}; last {last}");
    last
}

pub(crate) fn start(sandbox: &Sandbox) {
    let out = sandbox.factory(&["monitor", "start"]);
    assert_eq!(out.code, 0, "{out:?}");
}

/// A herdr with the lead `lead-<slug>` in pane `w<n>:p1` of the repo `app`, its session `s<n>`
/// run in `/work/app.<slug>`; returns the session's log.
pub(crate) fn lead(
    sandbox: &Sandbox,
    herdr: &FakeHerdr,
    n: u32,
    slug: &str,
    status: &str,
) -> PathBuf {
    let pane = format!("w{n}:p1");
    let cwd = format!("/work/app.{slug}");
    let session = format!("s{n}");
    herdr.workspace(&format!("w{n}"), "app");
    herdr.agent(Some(&format!("lead-{slug}")), &pane, status);
    herdr.session(&pane, &cwd, &session);
    session_log(sandbox, &cwd, &session)
}

#[test]
fn context_comes_from_new_lines_of_leads_and_subagents_logs() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "working");
    append(
        &log,
        &[usage(120_000, false), r#"{"type":"user"}"#.to_owned()],
    );
    start(&sandbox);

    await_status(&sandbox, "the lead's context at 120k", |s| {
        s["leads"][0]["tokens"] == 120_000
    });
    append(&log, &[usage(150_000, false), usage(9_000, true)]);
    let shown = await_status(&sandbox, "the lead's context at 150k", |s| {
        s["leads"][0]["tokens"] == 150_000
    });
    assert_eq!(shown["leads"][0]["lead"], "lead-cart", "{shown}");
    assert_eq!(shown["leads"][0]["log"], log.to_str().unwrap(), "{shown}");

    let sub = subagent_log(&log, "a1");
    append(&sub, &[usage(40_000, true)]);
    fs::write(
        sub.with_extension("meta.json"),
        r#"{"agentType":"implementer","description":"Implement t3"}"#,
    )
    .unwrap();
    await_status(&sandbox, "the subagent at 40k", |s| {
        s["leads"][0]["subagents"][0]["tokens"] == 40_000
    });
    append(&sub, &[usage(170_000, true)]);
    let shown = await_status(&sandbox, "the subagent at 170k", |s| {
        s["leads"][0]["subagents"][0]["tokens"] == 170_000
    });
    assert_eq!(
        shown["leads"][0]["subagents"][0]["agent"], "agent-a1",
        "{shown}"
    );
    assert_eq!(
        shown["leads"][0]["subagents"][0]["description"], "Implement t3",
        "{shown}"
    );
    assert_eq!(
        sandbox.factory(&["monitor", "status"]).stdout,
        "monitor on: 1 lead, 1 subagent; most context: lead-cart/agent-a1 170k\n"
    );
    assert!(herdr.prompts().is_empty(), "{:?}", herdr.prompts());
}

#[test]
fn the_latest_context_is_found_behind_long_lines_and_a_line_being_written() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "working");
    let blob = json!({"type": "api-request-blob", "body": "x".repeat(3 << 20)}).to_string();
    append(&log, &[usage(80_000, false), usage(110_000, false), blob]);
    let line = usage(140_000, false);
    let (begun, rest) = line.split_at(40);
    let mut file = OpenOptions::new().append(true).open(&log).unwrap();
    write!(file, "{begun}").unwrap();
    start(&sandbox);

    await_status(&sandbox, "the context behind the blob", |s| {
        s["leads"][0]["tokens"] == 110_000
    });
    writeln!(file, "{rest}").unwrap();
    await_status(&sandbox, "the line once written", |s| {
        s["leads"][0]["tokens"] == 140_000
    });
}

#[test]
fn an_idle_lead_over_the_threshold_is_asked_once_then_compacted() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\nask = 300000\ngrace = \"1s\"\n");
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "idle");
    append(&log, &[usage(312_000, false)]);
    start(&sandbox);

    let prompts = herdr.await_prompts(3);

    let targets: Vec<&str> = prompts.iter().map(|(target, _)| target.as_str()).collect();
    assert_eq!(targets, ["lead-cart"; 3], "{prompts:?}");
    assert!(
        prompts[0]
            .1
            .starts_with("factory monitor: your context is at 312k. At your next natural break"),
        "{prompts:?}"
    );
    assert!(prompts[1].1.starts_with("/compact Keep: "), "{prompts:?}");
    assert_eq!(prompts[2].1, "Compacted. Continue where you left off.");
    let shown = await_status(&sandbox, "the compacted lead", |s| {
        s["leads"][0]["compacting"] == false
    });
    assert_eq!(shown["leads"][0]["asked"], false, "{shown}");
    assert_eq!(shown["leads"][0]["tokens"], Value::Null, "{shown}");
    append(&log, &[usage(40_000, false)]);
    await_status(&sandbox, "the context after compaction", |s| {
        s["leads"][0]["tokens"] == 40_000
    });
    assert_eq!(herdr.prompts().len(), 3, "{:?}", herdr.prompts());
}

#[test]
fn a_working_lead_over_the_threshold_is_not_asked_until_it_idles() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\ngrace = \"1s\"\n");
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "working");
    append(&log, &[usage(305_000, false)]);
    start(&sandbox);
    await_status(&sandbox, "the lead at 305k", |s| {
        s["leads"][0]["tokens"] == 305_000
    });
    assert!(herdr.prompts().is_empty(), "{:?}", herdr.prompts());

    herdr.status("w1:p1", "idle");

    let prompts = herdr.await_prompts(1);
    assert!(prompts[0].1.contains("305k"), "{prompts:?}");
}

#[test]
fn a_lead_with_a_registered_wait_is_never_asked_or_compacted() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\ngrace = \"1s\"\n");
    let herdr = FakeHerdr::start(&sandbox);
    let waiting = lead(&sandbox, &herdr, 1, "cart", "idle");
    let free = lead(&sandbox, &herdr, 2, "pay", "idle");
    lead(&sandbox, &herdr, 3, "ship", "working");
    append(&waiting, &[usage(330_000, false)]);
    append(&free, &[usage(320_000, false)]);
    let mut cmd = sandbox.factory_command(&["wait", "lead", "ship"]);
    cmd.env("HERDR_PANE_ID", "w1:p1");
    let waiter = spawn(cmd);
    assert!(
        poll_until(|| sandbox.factory(&["lead", "status", "cart"]).stdout
            == "lead-cart waiting: lead-ship\n"),
        "the wait never registered"
    );
    start(&sandbox);

    let prompts = herdr.await_prompts(3);

    assert!(
        prompts.iter().all(|(target, _)| target == "lead-pay"),
        "{prompts:?}"
    );
    await_status(&sandbox, "the free lead compacted", |s| {
        s["leads"][1]["lead"] == "lead-pay"
            && s["leads"][1]["compacting"] == false
            && s["leads"][1]["asked"] == false
    });
    assert_eq!(herdr.prompts().len(), 3, "{:?}", herdr.prompts());

    herdr.status("w3:p1", "done");
    finish(waiter);
    let prompts = herdr.await_prompts(4);
    assert_eq!(prompts[3].0, "lead-cart", "{prompts:?}");
    assert!(prompts[3].1.contains("330k"), "{prompts:?}");
}

#[test]
fn a_subagent_passing_its_threshold_is_told_to_its_lead_once() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\nsubagent = 200000\n");
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "working");
    append(&log, &[usage(90_000, false)]);
    let finished = subagent_log(&log, "a0");
    append(&finished, &[usage(260_000, true)]);
    fs::write(
        finished.with_extension("meta.json"),
        r#"{"agentType":"implementer","description":"Implement t2","finished":true}"#,
    )
    .unwrap();
    start(&sandbox);
    await_status(&sandbox, "the lead", |s| s["leads"][0]["tokens"] == 90_000);

    let first = subagent_log(&log, "a1");
    fs::create_dir_all(first.parent().unwrap()).unwrap();
    fs::write(
        first.with_extension("meta.json"),
        r#"{"agentType":"implementer","description":"Implement t3"}"#,
    )
    .unwrap();
    append(&first, &[usage(150_000, true)]);
    append(&first, &[usage(210_000, true)]);
    append(&first, &[usage(230_000, true)]);
    let second = subagent_log(&log, "a3");
    append(&second, &[usage(220_000, true)]);

    let prompts = herdr.await_prompts(2);

    assert_eq!(prompts[0].0, "lead-cart", "{prompts:?}");
    assert!(
        prompts[0]
            .1
            .starts_with("factory monitor: your subagent agent-a1 (Implement t3) is at 2"),
        "{prompts:?}"
    );
    assert!(
        prompts[0].1.contains("k context, past 200k; "),
        "{prompts:?}"
    );
    assert_eq!(prompts[1].0, "lead-cart", "{prompts:?}");
    assert!(prompts[1].1.contains("agent-a3"), "{prompts:?}");
    // A finished subagent has nothing left to hand on, and leaves the table.
    let shown = await_status(&sandbox, "both live subagents told", |s| {
        s["leads"][0]["subagents"]
            .as_array()
            .is_some_and(|subs| subs.len() == 2)
            && s["leads"][0]["subagents"][1]["told"] == true
    });
    assert_eq!(
        shown["leads"][0]["subagents"][0]["agent"], "agent-a1",
        "{shown}"
    );
    assert_eq!(shown["leads"][0]["subagents"][0]["told"], true, "{shown}");
    assert_eq!(herdr.prompts().len(), 2, "{:?}", herdr.prompts());
}

#[test]
fn the_monitor_and_what_it_told_outlive_a_daemon_restart() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\nsubagent = 200000\n");
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "working");
    append(&log, &[usage(90_000, false)]);
    append(&subagent_log(&log, "a1"), &[usage(240_000, true)]);
    start(&sandbox);
    herdr.await_prompts(1);
    await_status(&sandbox, "the subagent told", |s| {
        s["leads"][0]["subagents"][0]["told"] == true
    });

    kill_daemon(&sandbox);
    let shown = await_status(&sandbox, "the monitor back with the subagent", |s| {
        s["running"] == true && s["leads"][0]["subagents"][0]["tokens"] == 240_000
    });

    assert_eq!(shown["leads"][0]["subagents"][0]["told"], true, "{shown}");
    assert_eq!(herdr.prompts().len(), 1, "{:?}", herdr.prompts());
}

#[test]
fn an_asked_lead_is_not_asked_again_after_a_daemon_restart() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\ngrace = \"1h\"\n");
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "idle");
    append(&log, &[usage(312_000, false)]);
    start(&sandbox);
    herdr.await_prompts(1);
    await_status(&sandbox, "the lead asked", |s| {
        s["leads"][0]["asked"] == true
    });

    kill_daemon(&sandbox);
    let shown = await_status(&sandbox, "the monitor back with the lead", |s| {
        s["running"] == true && s["leads"][0]["tokens"] == 312_000
    });

    assert_eq!(shown["leads"][0]["asked"], true, "{shown}");
    assert_eq!(herdr.prompts().len(), 1, "{:?}", herdr.prompts());
}

#[test]
fn a_wait_registered_after_the_ask_holds_the_compaction_off() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\ngrace = \"3s\"\n");
    let herdr = FakeHerdr::start(&sandbox);
    let held = lead(&sandbox, &herdr, 1, "cart", "idle");
    let free = lead(&sandbox, &herdr, 2, "pay", "idle");
    lead(&sandbox, &herdr, 3, "ship", "working");
    append(&held, &[usage(330_000, false)]);
    append(&free, &[usage(320_000, false)]);
    start(&sandbox);
    let asked = herdr.await_prompts(2);
    assert_eq!(asked[0].0, "lead-cart", "{asked:?}");
    let mut cmd = sandbox.factory_command(&["wait", "lead", "ship"]);
    cmd.env("HERDR_PANE_ID", "w1:p1");
    let waiter = spawn(cmd);
    assert!(
        poll_until(|| sandbox.factory(&["lead", "status", "cart"]).stdout
            == "lead-cart waiting: lead-ship\n"),
        "the wait never registered"
    );

    // The free lead, asked after the held one, is compacted once both graces are over.
    let prompts = herdr.await_prompts(4);
    assert!(
        prompts[2..].iter().all(|(target, _)| target == "lead-pay"),
        "{prompts:?}"
    );

    herdr.status("w3:p1", "done");
    finish(waiter);
    let prompts = herdr.await_prompts(6);
    assert_eq!(prompts[4].0, "lead-cart", "{prompts:?}");
    assert!(prompts[4].1.starts_with("/compact Keep: "), "{prompts:?}");
}

#[test]
fn a_lead_that_works_after_the_ask_gets_its_whole_grace_once_idle_again() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[monitor]\ngrace = \"2s\"\n");
    let herdr = FakeHerdr::start(&sandbox);
    let log = lead(&sandbox, &herdr, 1, "cart", "idle");
    append(&log, &[usage(312_000, false)]);
    start(&sandbox);
    herdr.await_watch("w1:p1");
    herdr.await_prompts(1);

    herdr.status("w1:p1", "working");
    let idle = Instant::now();
    herdr.status("w1:p1", "idle");

    let prompts = herdr.await_prompts(2);
    let waited = idle.elapsed();
    assert!(prompts[1].1.starts_with("/compact Keep: "), "{prompts:?}");
    assert!(
        waited >= Duration::from_secs(2),
        "compacted {waited:?} after it went idle again"
    );
}

fn kill_daemon(sandbox: &Sandbox) {
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).unwrap_or_default();
    let pid: u32 = pid.trim().parse().unwrap_or(0);
    assert!(pid > 0, "no daemon pid");
    support::kill_group(pid, rustix::process::Signal::KILL);
    let socket = sandbox.path("run/factory/factory.sock");
    assert!(
        poll_until(|| std::os::unix::net::UnixStream::connect(&socket).is_err()),
        "the daemon outlived SIGKILL"
    );
}
