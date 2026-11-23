//! `factory lead list|status` and `factory wait lead`, against a fake herdr.

use std::fs;
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::support::herdr::FakeHerdr;
use crate::support::{DEADLINE, Sandbox, finish, spawn};

#[test]
fn a_herdr_on_another_protocol_is_reported_and_not_used() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    herdr.protocol(23);

    let out = sandbox.factory(&["lead", "list"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr_lines(),
        vec![format!(
            "factory: herdr at {} speaks protocol 23, this factory speaks 22; \
             install the factory release that matches herdr",
            herdr.socket().display()
        )]
    );
    let methods = herdr.methods();
    assert!(
        !methods.is_empty() && methods.iter().all(|m| m == "ping"),
        "{methods:?}"
    );
}

/// A fake herdr with one workspace per lead, all on worktrees of the repo `Shop`.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn shop(sandbox: &Sandbox, leads: &[(&str, &str, &str)]) -> FakeHerdr {
    let herdr = FakeHerdr::start(sandbox);
    for (name, pane, status) in leads {
        let workspace = pane
            .split(':')
            .next()
            .expect("a pane id names its workspace");
        herdr.workspace(workspace, "Shop");
        herdr.agent(Some(name), pane, status);
    }
    herdr
}

/// Writes the final report of the lead `slug` in the repo `Shop`, where lead.sh puts it.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn report(sandbox: &Sandbox, slug: &str) -> String {
    let path = sandbox.path(format!("state/factory/leads/Shop/{slug}.md"));
    let dir = path.parent().expect("a report lives in a dir");
    fs::create_dir_all(dir).expect("create the report dir");
    fs::write(&path, "# Report\n").expect("write the report");
    path.display().to_string()
}

#[test]
fn list_says_what_each_lead_is_doing() {
    let sandbox = Sandbox::new();
    let herdr = shop(
        &sandbox,
        &[
            ("lead-cart", "w1:p1", "working"),
            ("lead-pay", "w2:p1", "blocked"),
            ("lead-docs", "w3:p1", "idle"),
            ("lead-ship", "w4:p1", "done"),
        ],
    );
    herdr.agent(None, "w5:p1", "working");
    herdr.agent(Some("reviewer"), "w5:p2", "idle");
    herdr.agent(Some("lead-Not_A_Slug"), "w5:p3", "working");
    let shipped = report(&sandbox, "ship");

    let out = sandbox.factory(&["lead", "list"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        out.stdout_lines(),
        vec![
            "lead-cart working".to_owned(),
            "lead-docs asking".to_owned(),
            "lead-pay blocked".to_owned(),
            format!("lead-ship finished: {shipped}"),
        ]
    );
    assert_eq!(out.stderr, "");
}

#[test]
fn list_with_no_leads_says_so_in_one_line() {
    let sandbox = Sandbox::new();
    let _herdr = shop(&sandbox, &[]);

    let out = sandbox.factory(&["lead", "list"]);

    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "no leads in herdr\n"),
        "{out:?}"
    );
}

#[test]
fn status_of_a_working_lead_is_one_line() {
    let sandbox = Sandbox::new();
    let _herdr = shop(&sandbox, &[("lead-cart", "w1:p1", "working")]);

    for arg in ["cart", "lead-cart"] {
        let out = sandbox.factory(&["lead", "status", arg]);

        assert_eq!(
            (out.code, out.stdout.as_str()),
            (0, "lead-cart working\n"),
            "{out:?}"
        );
    }
}

#[test]
fn status_of_a_blocked_or_asking_lead_adds_the_panes_last_lines() {
    let sandbox = Sandbox::new();
    let herdr = shop(
        &sandbox,
        &[
            ("lead-pay", "w2:p1", "blocked"),
            ("lead-docs", "w3:p1", "idle"),
        ],
    );
    let screen: Vec<String> = (1..=12).map(|n| format!("line {n}")).collect();
    herdr.screen("w2:p1", &format!("{}\n\n", screen.join("\n")));
    herdr.screen("w3:p1", "Which base branch?\n> ");

    let blocked = sandbox.factory(&["lead", "status", "pay"]);
    let asking = sandbox.factory(&["lead", "status", "docs"]);

    assert_eq!(blocked.code, 0, "{blocked:?}");
    let mut expected = vec!["lead-pay blocked".to_owned()];
    expected.extend((3..=12).map(|n| format!("line {n}")));
    assert_eq!(blocked.stdout_lines(), expected);
    assert_eq!(
        asking.stdout_lines(),
        vec!["lead-docs asking", "Which base branch?", ">"]
    );
}

#[test]
fn status_json_gives_the_state_pane_and_lines() {
    let sandbox = Sandbox::new();
    let herdr = shop(&sandbox, &[("lead-pay", "w2:p1", "blocked")]);
    herdr.screen("w2:p1", "Allow edit? (y/n)");

    let out = sandbox.factory(&["lead", "status", "pay", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    let json: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(json["lead"], "lead-pay");
    assert_eq!(json["state"], "blocked");
    assert_eq!(json["pane"], "w2:p1");
    assert_eq!(json["lines"], serde_json::json!(["Allow edit? (y/n)"]));
}

#[test]
fn status_of_a_lead_herdr_lacks_is_refused() {
    let sandbox = Sandbox::new();
    let _herdr = shop(&sandbox, &[("lead-cart", "w1:p1", "working")]);

    let out = sandbox.factory(&["lead", "status", "pay"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: herdr has no lead named lead-pay; see factory lead list\n"
    );
}

#[test]
fn a_name_no_lead_can_have_is_a_usage_error() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["lead", "status", "Bad_Name"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
}

#[test]
fn without_herdr_lead_commands_say_where_they_looked() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["lead", "list"]);

    assert_eq!(out.code, 3, "{out:?}");
    let socket = sandbox.path("config/herdr/herdr.sock");
    let prefix = format!("factory: herdr unreachable at {}: ", socket.display());
    assert!(out.stderr.starts_with(&prefix), "{out:?}");
    assert!(
        out.stderr
            .ends_with("; start herdr, or set HERDR_SOCKET_PATH to its socket\n"),
        "{out:?}"
    );
}

/// Starts `factory wait lead <args>` and returns once the daemon watches `pane` in herdr, so an
/// event the test sends next reaches it.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn waiting(sandbox: &Sandbox, herdr: &FakeHerdr, args: &[&str], pane: &str) -> Child {
    let mut all = vec!["wait", "lead"];
    all.extend(args);
    let child = spawn(sandbox.factory_command(&all));
    herdr.await_watch(pane);
    child
}

/// Asks for `lead status <slug>` until it prints `line`, failing after [`DEADLINE`].
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn await_status(sandbox: &Sandbox, slug: &str, line: &str) {
    let start = Instant::now();
    loop {
        let out = sandbox.factory(&["lead", "status", slug]);
        if out.stdout_lines().first() == Some(&line) {
            return;
        }
        assert!(
            start.elapsed() < DEADLINE,
            "never saw {line:?}; last {out:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn wait_answers_0_when_an_event_says_the_lead_finished() {
    let sandbox = Sandbox::new();
    let herdr = shop(&sandbox, &[("lead-cart", "w1:p1", "working")]);
    let child = waiting(&sandbox, &herdr, &["cart"], "w1:p1");

    let shipped = report(&sandbox, "cart");
    herdr.status("w1:p1", "done");
    let out = finish(child);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout, format!("lead-cart finished: {shipped}\n"));
    assert_eq!(out.stderr, "");
    assert_eq!(herdr.count("agent.list"), 1, "{:?}", herdr.methods());
}

#[test]
fn wait_answers_10_with_the_screen_when_the_lead_blocks_or_asks() {
    for (status, state) in [("blocked", "blocked"), ("idle", "asking")] {
        let sandbox = Sandbox::new();
        let herdr = shop(&sandbox, &[("lead-pay", "w2:p1", "working")]);
        herdr.screen("w2:p1", "Run cargo publish?\n1. Yes  2. No\n");
        let child = waiting(&sandbox, &herdr, &["pay"], "w2:p1");

        herdr.status("w2:p1", status);
        let out = finish(child);

        assert_eq!(out.code, 10, "{out:?}");
        assert_eq!(
            out.stdout_lines(),
            vec![
                format!("lead-pay {state}").as_str(),
                "Run cargo publish?",
                "1. Yes  2. No"
            ]
        );
        assert_eq!(herdr.count("agent.list"), 1, "{:?}", herdr.methods());
    }
}

#[test]
fn wait_on_a_lead_already_settled_answers_at_once() {
    let sandbox = Sandbox::new();
    let _herdr = shop(&sandbox, &[("lead-pay", "w2:p1", "blocked")]);

    let out = sandbox.factory(&["wait", "lead", "pay", "--json"]);

    assert_eq!(out.code, 10, "{out:?}");
    let json: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        (json["state"].as_str(), json["exit"].as_u64()),
        (Some("blocked"), Some(10))
    );
}

#[test]
fn wait_answers_10_when_the_leads_pane_closes_without_a_report() {
    let sandbox = Sandbox::new();
    let herdr = shop(&sandbox, &[("lead-cart", "w1:p1", "working")]);
    let child = waiting(&sandbox, &herdr, &["cart"], "w1:p1");

    herdr.close("w1:p1");
    let out = finish(child);

    assert_eq!(
        (out.code, out.stdout.as_str()),
        (10, "lead-cart dead\n"),
        "{out:?}"
    );
}

#[test]
fn wait_times_out_with_124_never_0() {
    let sandbox = Sandbox::new();
    let herdr = shop(&sandbox, &[("lead-cart", "w1:p1", "working")]);

    let out = sandbox.factory(&["wait", "lead", "cart", "--timeout", "1s"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "factory: lead-cart is still working after 1s; \
         wait again, or see factory lead status cart\n"
    );
    assert_eq!(herdr.count("agent.list"), 1, "{:?}", herdr.methods());
}

#[test]
fn a_timeout_that_is_not_a_duration_is_a_usage_error() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["wait", "lead", "cart", "--timeout", "soon"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "");
}

#[test]
fn a_lead_waiting_on_another_shows_as_waiting_until_the_wait_ends() {
    let sandbox = Sandbox::new();
    let herdr = shop(
        &sandbox,
        &[
            ("lead-cart", "w1:p1", "working"),
            ("lead-pay", "w2:p1", "working"),
        ],
    );
    let mut cmd = sandbox.factory_command(&["wait", "lead", "pay"]);
    cmd.env("HERDR_PANE_ID", "w1:p1");
    let child = spawn(cmd);

    await_status(&sandbox, "cart", "lead-cart waiting: lead-pay");
    herdr.status("w2:p1", "blocked");
    let out = finish(child);

    assert_eq!(out.code, 10, "{out:?}");
    await_status(&sandbox, "cart", "lead-cart working");
}

#[test]
fn a_wait_whose_caller_is_gone_stops_waiting() {
    let sandbox = Sandbox::new();
    let _herdr = shop(
        &sandbox,
        &[
            ("lead-cart", "w1:p1", "working"),
            ("lead-pay", "w2:p1", "working"),
        ],
    );
    let mut cmd = sandbox.factory_command(&["wait", "lead", "pay"]);
    cmd.env("HERDR_PANE_ID", "w1:p1");
    let mut child = spawn(cmd);
    await_status(&sandbox, "cart", "lead-cart waiting: lead-pay");

    child.kill().unwrap();
    child.wait().unwrap();

    await_status(&sandbox, "cart", "lead-cart working");
}

#[test]
fn a_wait_that_loses_herdr_fails_and_the_next_command_reconnects() {
    let sandbox = Sandbox::new();
    let herdr = shop(
        &sandbox,
        &[
            ("lead-cart", "w1:p1", "working"),
            ("lead-pay", "w2:p1", "working"),
        ],
    );
    // The waiter names its own pane, so `lead status` shows once the daemon has the wait:
    // a hang-up before that would only make the wait reconnect.
    let mut cmd = sandbox.factory_command(&["wait", "lead", "cart"]);
    cmd.env("HERDR_PANE_ID", "w2:p1");
    let child = spawn(cmd);
    await_status(&sandbox, "pay", "lead-pay waiting: lead-cart");

    herdr.hang_up();
    let out = finish(child);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(
        out.stderr,
        format!(
            "factory: lost herdr's events at {}; retry once herdr runs\n",
            herdr.socket().display()
        )
    );
    let again = sandbox.factory(&["lead", "status", "cart"]);
    assert_eq!(
        (again.code, again.stdout.as_str()),
        (0, "lead-cart working\n"),
        "{again:?}"
    );
}

#[test]
fn a_lead_that_appears_later_is_found() {
    let sandbox = Sandbox::new();
    let herdr = shop(&sandbox, &[("lead-cart", "w1:p1", "working")]);
    assert_eq!(sandbox.factory(&["lead", "list"]).code, 0);

    herdr.workspace("w2", "Shop");
    herdr.agent(Some("lead-pay"), "w2:p1", "blocked");
    let out = sandbox.factory(&["lead", "status", "pay"]);

    assert_eq!(
        (out.code, out.stdout_lines()[0]),
        (0, "lead-pay blocked"),
        "{out:?}"
    );
}
