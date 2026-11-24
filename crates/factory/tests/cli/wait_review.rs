//! `factory wait review`, against reviews run by the binary and a fake codex.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::Value;

use crate::review::{CLEAN_REPORT, FINDINGS_REPORT, codex_writes};
use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;
use crate::support::{Sandbox, finish, freeze, kill_group, spawn, stop_daemon, thaw, waited_on};

/// A herdr with the lead `cart` in pane w1:p1, so a wait that names the pane shows up in `lead
/// status` once the daemon holds it.
fn shop(sandbox: &Sandbox) -> FakeHerdr {
    let herdr = FakeHerdr::start(sandbox);
    herdr.workspace("w1", "Shop");
    herdr.agent(Some("lead-cart"), "w1:p1", "working");
    herdr
}

/// Waits until the daemon holds a wait of the lead `cart` on the review `label`: from then on
/// an answer that has not come is the wait's own doing.
fn await_registered(sandbox: &Sandbox, label: &str) {
    let want = format!("lead-cart waiting: review {label}");
    assert!(
        poll_until(|| {
            sandbox
                .factory(&["lead", "status", "cart"])
                .stdout_lines()
                .first()
                == Some(&want.as_str())
        }),
        "the daemon never held the wait"
    );
}

/// A fake codex that holds the review until the test opens its gate, then writes `report`.
fn gated_codex(sandbox: &Sandbox, report: &str) -> std::path::PathBuf {
    let gate = sandbox.path("codex.gate");
    sandbox.fake(
        "codex",
        &format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done\n{}",
            gate.display(),
            codex_writes(report)
        ),
    );
    gate
}

#[test]
fn a_review_that_already_ended_answers_at_once_with_the_reviews_own_line() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    let review = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    let out = sandbox.factory_in(&repo, &["wait", "review", "t1-r1"]);

    assert_eq!(review.code, 0, "{review:?}");
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, review.stdout.as_str(), ""),
        "{out:?}"
    );
}

#[test]
fn a_review_with_findings_exits_10_on_one_line_and_leaves_the_findings_to_the_report() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));
    let review = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    let out = sandbox.factory_in(&repo, &["wait", "review", "t1-r1"]);

    assert_eq!(review.code, 10, "{review:?}");
    assert_eq!(out.code, 10, "{out:?}");
    assert_eq!(
        out.stdout,
        review.stdout_lines()[0].to_owned() + "\n",
        "{out:?}"
    );
    assert_eq!(out.stderr, "");
}

#[test]
fn it_blocks_silent_while_the_review_runs_then_prints_its_result() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let gate = gated_codex(&sandbox, CLEAN_REPORT);
    let mut review = sandbox.factory_command(&["review", "main", "t1-r1"]);
    review.current_dir(&repo);
    let review = spawn(review);
    assert!(
        poll_until(|| !sandbox.calls("codex").is_empty()),
        "codex never started"
    );
    let _herdr = shop(&sandbox);
    let mut waiter = sandbox.factory_command(&["wait", "review", "t1-r1"]);
    waiter.current_dir(&repo).env("HERDR_PANE_ID", "w1:p1");
    let mut waiter = spawn(waiter);

    // Once the daemon holds the wait it has read the ledger, and the review has not ended.
    await_registered(&sandbox, "t1-r1");
    assert!(waiter.try_wait().unwrap().is_none(), "answered early");
    fs::write(&gate, "").unwrap();
    let review = finish(review);
    let out = finish(waiter);

    assert_eq!(review.code, 0, "{review:?}");
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, review.stdout.as_str(), ""),
        "{out:?}"
    );
}

#[test]
fn a_rerun_under_the_same_label_is_waited_for_not_answered_from_the_last_run() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    let first = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);
    assert_eq!(first.code, 0, "{first:?}");
    let gate = gated_codex(&sandbox, FINDINGS_REPORT);
    let mut rerun = sandbox.factory_command(&["review", "main", "t1-r1"]);
    rerun.current_dir(&repo);
    let rerun = spawn(rerun);
    assert!(
        poll_until(|| sandbox.calls("codex").len() == 2),
        "codex never started"
    );
    let _herdr = shop(&sandbox);
    let mut waiter = sandbox.factory_command(&["wait", "review", "t1-r1"]);
    waiter.current_dir(&repo).env("HERDR_PANE_ID", "w1:p1");
    let mut waiter = spawn(waiter);

    await_registered(&sandbox, "t1-r1");
    assert!(
        waiter.try_wait().unwrap().is_none(),
        "answered from the first run"
    );
    fs::write(&gate, "").unwrap();
    let rerun = finish(rerun);
    let out = finish(waiter);

    assert_eq!(rerun.code, 10, "{rerun:?}");
    assert_eq!(out.code, 10, "{out:?}");
    assert!(
        out.stdout.starts_with("review t1-r1: findings ("),
        "{out:?}"
    );
}

#[test]
fn a_wait_carries_on_across_a_daemon_restart_and_hears_what_happened_meanwhile() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let mut waiter = sandbox.factory_command(&["wait", "review", "t1-r1"]);
    waiter.current_dir(&repo);
    let waiter = spawn(waiter);
    assert!(
        poll_until(|| waited_on(&sandbox, "review", "t1-r1")),
        "the daemon never held the wait"
    );

    // The CLI sleeps through the restart; the review ends before it wakes.
    freeze(&waiter);
    stop_daemon(&sandbox);
    let logged = sandbox.factory(&[
        "log",
        "review",
        "repo=repo",
        "label=t1-r1",
        "outcome=ok",
        "verdict=clean",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");
    thaw(&waiter);
    let out = finish(waiter);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.starts_with("review t1-r1: clean"), "{out:?}");
    assert_eq!(out.stderr, "");
}

#[test]
fn a_wait_whose_cli_went_away_is_not_brought_back_by_a_restart() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let mut waiter = sandbox.factory_command(&["wait", "review", "t1-r1"]);
    waiter.current_dir(&repo);
    let waiter = spawn(waiter);
    assert!(
        poll_until(|| waited_on(&sandbox, "review", "t1-r1")),
        "the daemon never held the wait"
    );

    kill_group(waiter.id(), rustix::process::Signal::KILL);
    let _ = finish(waiter);
    assert!(
        poll_until(|| !waited_on(&sandbox, "review", "t1-r1")),
        "the daemon still holds the wait of a CLI that is gone"
    );
    stop_daemon(&sandbox);

    assert!(
        !waited_on(&sandbox, "review", "t1-r1"),
        "the restart brought it back"
    );
}

#[test]
fn a_wait_no_daemon_holds_any_more_says_to_run_it_again() {
    let sandbox = Sandbox::new();
    let started = sandbox.factory(&["status"]);
    assert_eq!(started.code, 0, "{started:?}");
    let mut stream = UnixStream::connect(sandbox.path("run/factory/factory.sock")).unwrap();
    let request = r#"{"id":1,"method":"wait.review","params":{"label":"t1-r1","repo":null,"waiter":null,"timeout_secs":5,"resume":42}}"#;

    writeln!(stream, "{request}").unwrap();

    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer).unwrap();
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["error"]["code"], 3, "{answer}");
    assert_eq!(
        answer["error"]["message"],
        "the daemon restarted and no longer holds wait 42; run the wait again"
    );
}

#[test]
fn a_restarted_wait_on_a_rerun_still_waits_for_the_rerun_not_the_last_run() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    let first = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);
    assert_eq!(first.code, 0, "{first:?}");
    let gate = gated_codex(&sandbox, FINDINGS_REPORT);
    let mut rerun = sandbox.factory_command(&["review", "main", "t1-r1"]);
    rerun.current_dir(&repo);
    let rerun = spawn(rerun);
    assert!(
        poll_until(|| sandbox.calls("codex").len() == 2),
        "codex never started"
    );
    let mut waiter = sandbox.factory_command(&["wait", "review", "t1-r1", "--timeout", "5"]);
    waiter.current_dir(&repo);
    let waiter = spawn(waiter);
    assert!(
        poll_until(|| waited_on(&sandbox, "review", "t1-r1")),
        "the daemon never held the wait"
    );

    // The restarted daemon never saw the rerun begin; the wait remembers it did.
    freeze(&waiter);
    stop_daemon(&sandbox);
    thaw(&waiter);
    let out = finish(waiter);
    fs::write(&gate, "").unwrap();
    finish(rerun);

    assert_eq!(out.code, 124, "answered from the first run: {out:?}");
    assert_eq!(
        out.stderr,
        "factory: no review t1-r1 ended within 5s; wait again\n"
    );
}

#[test]
fn a_review_that_failed_ends_the_wait_with_exit_10_and_the_reason() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", "exit 1");
    let review = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);
    assert_eq!(review.code, 3, "{review:?}");

    let out = sandbox.factory_in(&repo, &["wait", "review", "t1-r1"]);

    assert_eq!(out.code, 10, "{out:?}");
    assert!(
        out.stdout
            .starts_with("review t1-r1: error (codex exited 1"),
        "{out:?}"
    );
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
}

#[test]
fn a_review_that_never_comes_ends_in_exit_124_naming_the_label() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["wait", "review", "t9-r1", "--timeout", "1s"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: ") && out.stderr.contains("t9-r1"),
        "{out:?}"
    );
}

#[test]
fn another_repos_review_of_the_same_label_does_not_answer() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let logged = sandbox.factory(&[
        "log",
        "review",
        "repo=elsewhere",
        "label=t1-r1",
        "outcome=ok",
        "verdict=clean",
    ]);
    assert_eq!(logged.code, 0, "{logged:?}");

    let out = sandbox.factory_in(&repo, &["wait", "review", "t1-r1", "--timeout", "1s"]);

    assert_eq!(out.code, 124, "{out:?}");
}

#[test]
fn json_carries_the_event_and_the_exit() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));
    sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    let out = sandbox.factory_in(&repo, &["wait", "review", "t1-r1", "--json"]);

    assert_eq!(out.code, 10, "{out:?}");
    let body: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["label"], "t1-r1", "{body}");
    assert_eq!(body["verdict"], "findings", "{body}");
    assert_eq!(body["blocking"], 2, "{body}");
    assert_eq!(body["exit"], 10, "{body}");
    assert_eq!(
        body["report"],
        sandbox.reviews().join("t1-r1.md").to_str().unwrap(),
        "{body}"
    );
}

#[test]
fn a_bad_label_is_refused_with_exit_2() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["wait", "review", "t1/r1"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
}
