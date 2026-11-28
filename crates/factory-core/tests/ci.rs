//! Which check runs pass, skip, fail or are pending, and when a wait on them is over (spec #1,
//! user story 3).

use factory_core::ci::{self, Branch, Bucket, CheckRun, PullRequest, Verdict, Watched};
use factory_core::exit::Exit;

fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
    with_id(1, name, status, conclusion)
}

fn with_id(id: u64, name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
    CheckRun {
        id,
        name: name.to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        url: format!("https://github.com/acme/shop/runs/{name}"),
        app: Some("github-actions".to_owned()),
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
        ("completed", Some("cancelled"), Bucket::Cancelled),
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
    assert_eq!(failed, ["test"]);
    assert_eq!(tally.cancelled.len(), 1);
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

#[test]
fn only_the_newest_run_of_each_check_counts() {
    // A re-run's runs come after the ones they replace, with higher ids, in any order.
    let Verdict::Finished(tally) = ci::verdict(&[
        with_id(5, "test", "completed", Some("success")),
        with_id(2, "test", "completed", Some("failure")),
        with_id(3, "lint", "completed", Some("cancelled")),
        with_id(4, "lint", "completed", Some("success")),
    ]) else {
        panic!("every newest run finished");
    };
    assert_eq!(
        (tally.passed, tally.failed.len(), tally.cancelled.len()),
        (2, 0, 0)
    );
    assert_eq!(tally.exit(), Exit::Ok, "superseded runs do not fail a head");
    // An older run that finished does not stand in for a newer one still running.
    assert_eq!(
        ci::verdict(&[
            with_id(1, "test", "completed", Some("success")),
            with_id(9, "test", "in_progress", None),
        ]),
        Verdict::Pending { pending: 1 }
    );
}

#[test]
fn a_newer_run_of_one_name_from_another_app_replaces_the_older() {
    let mut buildkite = with_id(1, "test", "completed", Some("failure"));
    buildkite.app = Some("buildkite".to_owned());
    let Verdict::Finished(tally) =
        ci::verdict(&[buildkite, with_id(2, "test", "completed", Some("success"))])
    else {
        panic!("every run finished");
    };
    assert_eq!((tally.passed, tally.failed.len()), (1, 0));
    assert_eq!(tally.exit(), Exit::Ok);
}

/// A commit status's id counts in a sequence of its own, so it says nothing of which came
/// first, a status or a check run of the same name.
#[test]
fn a_commit_status_and_a_check_run_of_one_name_are_two_checks() {
    let mut status = with_id(1, "ci", "completed", Some("failure"));
    status.app = None;
    let Verdict::Finished(tally) =
        ci::verdict(&[status, with_id(2, "ci", "completed", Some("success"))])
    else {
        panic!("every run finished");
    };
    assert_eq!((tally.passed, tally.failed.len()), (1, 1));
}

#[test]
fn cancelled_runs_count_apart_from_failed_ones() {
    let Verdict::Finished(tally) = ci::verdict(&[
        with_id(1, "test", "completed", Some("failure")),
        with_id(2, "e2e", "completed", Some("cancelled")),
        with_id(3, "build", "completed", Some("cancelled")),
    ]) else {
        panic!("every run finished");
    };
    let names = |runs: &[CheckRun]| runs.iter().map(|run| run.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&tally.failed), ["test"]);
    assert_eq!(names(&tally.cancelled), ["e2e", "build"]);
    assert_eq!(tally.exit(), Exit::NeedsAttention);
}

#[test]
fn a_head_whose_newest_runs_were_cancelled_was_not_verified() {
    let Verdict::Finished(tally) = ci::verdict(&[
        with_id(1, "lint", "completed", Some("success")),
        with_id(2, "test", "completed", Some("cancelled")),
    ]) else {
        panic!("every run finished");
    };
    assert!(tally.failed.is_empty());
    assert_eq!(tally.exit(), Exit::NeedsAttention);
}

#[test]
fn only_an_actions_job_that_ran_has_a_log_to_fetch() {
    let table = [
        (Some("github-actions"), Some("failure"), true),
        (Some("github-actions"), Some("timed_out"), true),
        (Some("github-actions"), Some("cancelled"), false),
        (Some("github-actions"), Some("startup_failure"), false),
        (Some("github-actions"), Some("action_required"), false),
        (Some("github-actions"), Some("stale"), false),
        (Some("buildkite"), Some("failure"), false),
        (None, Some("failure"), false),
    ];
    for (app, conclusion, want) in table {
        let mut check = run("x", "completed", conclusion);
        check.app = app.map(str::to_owned);
        assert_eq!(check.has_log(), want, "{app:?} {conclusion:?}");
    }
}

#[test]
fn a_tail_drops_every_kind_of_escape_sequence() {
    let table = [
        ("csi colour", "\u{1b}[31merror\u{1b}[0m: boom"),
        ("csi private mode", "\u{1b}[?25lerror: boom\u{1b}[?25h"),
        (
            "osc link by bel",
            "\u{1b}]8;;https://x\u{7}error: boom\u{1b}]8;;\u{7}",
        ),
        ("osc title by st", "\u{1b}]0;title\u{1b}\\error: boom"),
        ("dcs by st", "\u{1b}Pq#0\u{1b}\\error: boom"),
        ("apc by st", "\u{1b}_G;x\u{1b}\\error: boom"),
        ("charset", "\u{1b}(Berror: boom"),
        ("two-byte", "\u{1b}=\u{1b}7error: boom\u{1b}8"),
        ("keypad", "error: \u{1b}>boom"),
    ];
    for (kind, line) in table {
        assert_eq!(ci::tail(line, 1), ["error: boom"], "{kind}");
    }
}

#[test]
fn a_tail_drops_terminal_escape_sequences() {
    let log = "\u{1b}[36;1mstep\u{1b}[0m 1\n\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{7}\n\u{1b}[31merror\u{1b}[0m: boom\n";
    assert_eq!(ci::tail(log, 3), ["step 1", "link", "error: boom"]);
}

#[test]
fn a_watched_branch_or_pull_request_reads_back_from_its_url_alone() {
    let main = Branch::of_repo("https://github.com/acme/shop", "main").unwrap();
    assert_eq!(main.url(), "https://github.com/acme/shop/tree/main");
    let nested = Branch::of_repo("https://github.com/acme/shop/", "release/2.x").unwrap();
    for branch in [main, nested] {
        let watched = Watched::Branch(branch.clone());
        assert_eq!(Watched::from_url(&watched.url()), Some(watched));
        assert_eq!(Branch::from_url(&branch.url()), Some(branch));
    }
    let pr = PullRequest::from_url("https://github.com/acme/shop/pull/12").unwrap();
    assert_eq!(
        Watched::from_url("https://github.com/acme/shop/pull/12"),
        Some(Watched::Pull(pr))
    );
    for not in [
        "branch:main",
        "12",
        "https://github.com/acme/shop",
        "https://github.com/acme/shop/tree/",
        "https://github.com/acme/shop/blob/main",
    ] {
        assert_eq!(Watched::from_url(not), None, "{not}");
    }
}
