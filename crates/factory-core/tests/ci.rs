//! Which check runs pass, skip, fail or are pending, and when a wait on them is over (spec #1,
//! user story 3).

use factory_core::ci::{self, Bucket, CheckRun, PullRequest, Verdict};
use factory_core::exit::Exit;

fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
    CheckRun {
        id: 1,
        name: name.to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        url: format!("https://github.com/acme/shop/runs/{name}"),
    }
}

#[test]
fn only_a_completed_run_counts_and_only_a_finished_one_can_fail() {
    let table = [
        ("queued", None, Bucket::Pending),
        ("in_progress", None, Bucket::Pending),
        ("waiting", None, Bucket::Pending),
        ("in_progress", Some("failure"), Bucket::Pending),
        ("completed", None, Bucket::Pending),
        ("completed", Some("success"), Bucket::Passed),
        ("completed", Some("skipped"), Bucket::Skipped),
        ("completed", Some("neutral"), Bucket::Skipped),
        ("completed", Some("failure"), Bucket::Failed),
        ("completed", Some("cancelled"), Bucket::Failed),
        ("completed", Some("timed_out"), Bucket::Failed),
        ("completed", Some("action_required"), Bucket::Failed),
        ("completed", Some("startup_failure"), Bucket::Failed),
        ("completed", Some("stale"), Bucket::Failed),
        ("completed", Some("something_new"), Bucket::Failed),
    ];
    for (status, conclusion, want) in table {
        assert_eq!(
            run("x", status, conclusion).bucket(),
            want,
            "{status} {conclusion:?}"
        );
    }
}

#[test]
fn a_verdict_waits_for_every_run_and_for_the_first() {
    assert_eq!(ci::verdict(&[]), Verdict::Pending { pending: 0 });
    assert_eq!(
        ci::verdict(&[
            run("lint", "completed", Some("failure")),
            run("test", "queued", None),
        ]),
        Verdict::Pending { pending: 1 }
    );
    let Verdict::Finished(tally) = ci::verdict(&[
        run("lint", "completed", Some("success")),
        run("docs", "completed", Some("skipped")),
        run("test", "completed", Some("failure")),
        run("e2e", "completed", Some("cancelled")),
    ]) else {
        panic!("every run finished");
    };
    assert_eq!((tally.passed, tally.skipped), (1, 1));
    let failed: Vec<&str> = tally.failed.iter().map(|run| run.name.as_str()).collect();
    assert_eq!(failed, ["test", "e2e"]);
    assert_eq!(tally.exit(), Exit::NeedsAttention);
}

#[test]
fn skipped_runs_alone_pass() {
    let Verdict::Finished(tally) = ci::verdict(&[run("deploy", "completed", Some("skipped"))])
    else {
        panic!("every run finished");
    };
    assert_eq!(tally.exit(), Exit::Ok);
}

#[test]
fn a_pull_request_reads_from_its_url() {
    let pr = PullRequest::from_url("https://github.com/acme/shop/pull/12\n").expect("a PR URL");
    assert_eq!(pr.to_string(), "acme/shop#12");
    assert_eq!(pr.url(), "https://github.com/acme/shop/pull/12");
    assert_eq!(
        PullRequest::from_url("https://ghe.example.com/acme/shop/pull/7/")
            .expect("a GHE PR URL")
            .host,
        "ghe.example.com"
    );
    for bad in [
        "",
        "http://github.com/acme/shop/pull/12",
        "https://github.com/acme/shop/issues/12",
        "https://github.com/acme/shop/pull/0",
        "https://github.com/acme/shop/pull/x",
        "https://github.com/acme/pull/12",
        "https://github.com//shop/pull/12",
    ] {
        assert!(PullRequest::from_url(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_tail_keeps_the_last_lines_without_trailing_blanks() {
    assert_eq!(ci::tail("a\nb\nc\n\n  \n", 2), ["b", "c"]);
    assert_eq!(ci::tail("a\nb\n", 5), ["a", "b"]);
    assert!(ci::tail("\n\n", 3).is_empty());
    assert!(ci::tail("", 3).is_empty());
}
