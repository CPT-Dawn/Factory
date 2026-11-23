//! `factory wait review`, against reviews run by the binary and a fake codex.

use std::fs;

use serde_json::Value;

use crate::review::{CLEAN_REPORT, FINDINGS_REPORT, codex_writes};
use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;
use crate::support::{Sandbox, finish, spawn};

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
