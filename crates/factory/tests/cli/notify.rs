//! `factory wait ci|lead --notify`: the wait outlives its CLI, and the daemon posts one line into
//! the registering Claude Code session when it ends, against a fake session inbox.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use rustix::process::Signal;
use serde_json::{Value, json};

use crate::support::github::FakeGitHub;
use crate::support::herdr::FakeHerdr;
use crate::support::inbox::{FakeInbox, TOKEN};
use crate::support::world::poll_until;
use crate::support::{Run, Sandbox, kill_group, run};

const REGISTERED: &str =
    "wait #1 on ci acme/shop#12 registered; its line comes to this session when it ends\n";

/// A sandbox that polls GitHub every second, with event delivery switched `on` or not.
fn delivery(on: bool) -> Sandbox {
    let sandbox = Sandbox::new();
    let config = sandbox.path("config/factory");
    fs::create_dir_all(&config).unwrap();
    let mut text = String::from("[ci]\npoll = \"1s\"\n");
    if on {
        text.push_str("\n[notify]\nenabled = true\n");
    }
    fs::write(config.join("config.toml"), text).unwrap();
    sandbox
}

#[test]
fn with_delivery_off_notify_is_refused_with_exit_3_and_one_line() {
    let sandbox = delivery(false);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let inbox = FakeInbox::start(&sandbox, "lead");
    let mut cmd = sandbox.factory_command(&["wait", "ci", "12", "--notify"]);
    inbox.inside(&mut cmd);

    let out = crate::support::run(cmd);

    let config = sandbox.path("config/factory/config.toml");
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            3,
            "",
            format!(
                "factory: --notify is off: event delivery into sessions stays off until the \
                 owner sets [notify] enabled = true in {}; wait without --notify\n",
                config.display()
            )
            .as_str()
        ),
        "{out:?}"
    );
    assert!(github.polls().is_empty(), "a refused wait polls nothing");
}

#[test]
fn outside_a_claude_code_session_notify_is_refused_with_exit_3() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);

    let out = sandbox.factory(&["wait", "ci", "12", "--notify"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            3,
            "",
            "factory: --notify posts into the Claude Code session it runs in, and this shell has \
             no CLAUDE_CODE_MESSAGING_SOCKET and TOKEN; run it from the session's own shell, or \
             wait without --notify\n"
        ),
        "{out:?}"
    );
}

#[test]
fn with_delivery_on_notify_returns_at_once_and_posts_one_line_when_ci_ends() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let _herdr = shop(&sandbox);
    let inbox = FakeInbox::start(&sandbox, "lead");

    let out = notify(&sandbox, &inbox, &["ci", "12"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, REGISTERED, ""),
        "{out:?}"
    );
    assert_eq!(status(&sandbox), "lead-a waiting: ci acme/shop#12\n");
    assert!(poll_until(|| !github.polls().is_empty()), "never polled");
    assert!(inbox.posts().is_empty(), "nothing is posted before CI ends");

    github.checks(&[(1, "test", "completed", Some("success"))]);

    assert_eq!(
        inbox.await_posts(1),
        vec![posted(
            "factory wait #1 ended (exit 0): acme/shop#12: 1 passed, 0 failed"
        )]
    );
    assert_eq!(status(&sandbox), "lead-a working\n", "the wait is cleared");
    let db = fs::metadata(sandbox.path("state/factory/factory.db")).unwrap();
    assert_eq!(
        db.permissions().mode() & 0o777,
        0o600,
        "the database keeps session tokens"
    );
}

#[test]
fn json_names_the_wait_and_what_it_waits_on() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let inbox = FakeInbox::start(&sandbox, "lead");

    let out = notify(&sandbox, &inbox, &["ci", "12", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    let answer: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        (answer["id"].as_i64(), answer["on"].as_str()),
        (Some(1), Some("ci acme/shop#12")),
        "{answer}"
    );
    assert!(answer["deadline"].as_i64().is_some(), "{answer}");
}

#[test]
fn a_wait_that_runs_out_of_time_posts_its_timeout() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let inbox = FakeInbox::start(&sandbox, "lead");

    let out = notify(&sandbox, &inbox, &["ci", "12", "--timeout", "2s"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        inbox.await_posts(1),
        vec![posted(
            "factory wait #1 ended (exit 124): acme/shop#12 still has 1 check running after 2s; \
             wait again"
        )]
    );
}

#[test]
fn a_session_that_exits_is_told_nothing_and_its_wait_is_cleared_at_once() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let _herdr = shop(&sandbox);
    let inbox = FakeInbox::start(&sandbox, "lead");
    // The session's own process, as CLAUDE_PID names it.
    let mut session = Command::new("sleep")
        .arg("600")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let mut cmd = sandbox.factory_command(&["wait", "ci", "12", "--notify"]);
    inbox.inside(&mut cmd);
    cmd.env("HERDR_PANE_ID", "w1:p1")
        .env("CLAUDE_PID", session.id().to_string());
    let out = run(cmd);
    assert_eq!((out.code, out.stdout.as_str()), (0, REGISTERED), "{out:?}");
    assert_eq!(status(&sandbox), "lead-a waiting: ci acme/shop#12\n");

    session.kill().unwrap();
    session.wait().unwrap();

    let mut last = String::new();
    let cleared = poll_until(|| {
        last = status(&sandbox);
        last == "lead-a working\n"
    });
    assert!(cleared, "the wait outlived its session: {last:?}");
    github.checks(&[(1, "test", "completed", Some("success"))]);
    restart(&sandbox);
    assert_eq!(
        status(&sandbox),
        "lead-a working\n",
        "a restart resumes no wait"
    );
    assert!(inbox.posts().is_empty(), "{:?}", inbox.posts());
}

#[test]
fn a_session_whose_inbox_is_gone_is_told_nothing_and_its_wait_is_cleared() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let _herdr = shop(&sandbox);
    let inbox = FakeInbox::start(&sandbox, "lead");
    let out = notify(&sandbox, &inbox, &["ci", "12"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, REGISTERED), "{out:?}");

    inbox.close();
    github.checks(&[(1, "test", "completed", Some("success"))]);

    let mut last = String::new();
    let cleared = poll_until(|| {
        last = status(&sandbox);
        last == "lead-a working\n"
    });
    assert!(cleared, "the wait was never cleared: {last:?}");
    assert!(inbox.posts().is_empty(), "{:?}", inbox.posts());
}

#[test]
fn a_daemon_restart_resumes_the_wait_and_still_posts_its_line() {
    let sandbox = delivery(true);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let _herdr = shop(&sandbox);
    let inbox = FakeInbox::start(&sandbox, "lead");
    let out = notify(&sandbox, &inbox, &["ci", "12"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, REGISTERED), "{out:?}");

    restart(&sandbox);

    assert_eq!(status(&sandbox), "lead-a waiting: ci acme/shop#12\n");
    github.checks(&[(1, "test", "completed", Some("success"))]);
    assert_eq!(
        inbox.await_posts(1),
        vec![posted(
            "factory wait #1 ended (exit 0): acme/shop#12: 1 passed, 0 failed"
        )]
    );
}

#[test]
fn wait_lead_notify_posts_the_line_once_the_lead_settles() {
    let sandbox = delivery(true);
    let herdr = shop(&sandbox);
    herdr.agent(Some("lead-b"), "w1:p2", "working");
    let inbox = FakeInbox::start(&sandbox, "lead");

    let out = notify(&sandbox, &inbox, &["lead", "b"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            0,
            "wait #1 on lead-b registered; its line comes to this session when it ends\n",
            ""
        ),
        "{out:?}"
    );
    assert_eq!(status(&sandbox), "lead-a waiting: lead-b\n");
    herdr.await_watch("w1:p2");

    herdr.status("w1:p2", "blocked");

    assert_eq!(
        inbox.await_posts(1),
        vec![posted("factory wait #1 ended (exit 10): lead-b blocked")]
    );
}

/// Runs `factory wait <args> --notify` from lead-a's pane, in `inbox`'s session.
fn notify(sandbox: &Sandbox, inbox: &FakeInbox, args: &[&str]) -> Run {
    let mut all = vec!["wait"];
    all.extend(args);
    all.push("--notify");
    let mut cmd = sandbox.factory_command(&all);
    inbox.inside(&mut cmd);
    cmd.env("HERDR_PANE_ID", "w1:p1");
    run(cmd)
}

/// A herdr with lead-a working in pane w1:p1, where the waits come from.
fn shop(sandbox: &Sandbox) -> FakeHerdr {
    let herdr = FakeHerdr::start(sandbox);
    herdr.workspace("w1", "shop");
    herdr.agent(Some("lead-a"), "w1:p1", "working");
    herdr
}

fn status(sandbox: &Sandbox) -> String {
    sandbox.factory(&["lead", "status", "a"]).stdout
}

/// What one post says: the token, then `line` as a user message from factory.
fn posted(line: &str) -> Vec<Value> {
    vec![
        json!({"type": "auth", "token": TOKEN}),
        json!({
            "type": "user",
            "message": {"role": "user", "content": line},
            "from": "factory",
        }),
    ]
}

/// Stops the daemon as systemd does for a restart, and waits until it is gone; the next call
/// starts another.
fn restart(sandbox: &Sandbox) {
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).unwrap();
    let pid: u32 = pid.trim().parse().unwrap();
    kill_group(pid, Signal::TERM);
    let proc = format!("/proc/{pid}");
    assert!(
        poll_until(|| !Path::new(&proc).exists()),
        "the daemon outlived its TERM"
    );
}
