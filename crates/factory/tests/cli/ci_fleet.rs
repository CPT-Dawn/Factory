//! The CI watcher: the daemon reads each configured repo's self-hosted runner fleet, its queued
//! and running jobs and its open `factory/*` pull requests' checks, against a fake GitHub behind
//! the fake `gh`, and only while something needs watching.

use serde_json::{Value, json};

use crate::support::github::{FORBIDDEN, FakeGitHub, Job, RATE_LIMITED, TIMED_OUT};
use crate::support::world::poll_until;
use crate::support::{Sandbox, stop_daemon};

/// A sandbox whose config watches acme/shop's CI, with `expected` runners, on a clock a test
/// moves with [`move_clock`].
fn watching(expected: u32) -> Sandbox {
    let sandbox = Sandbox::new();
    let skew = sandbox.path("clock-skew");
    let sandbox = sandbox.env("FACTORY_CLOCK_SKEW_FILE", &skew);
    let config = sandbox.path("config/factory");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!("[ci]\npoll = \"1s\"\nrepos = [\"acme/shop\"]\nexpected = {expected}\n"),
    )
    .unwrap();
    sandbox
}

fn status(sandbox: &Sandbox) -> Value {
    let out = sandbox.factory(&["status", "--json"]);
    assert_eq!(out.code, 0, "{out:?}");
    serde_json::from_str(&out.stdout).unwrap()
}

/// What `factory status --json` shows of acme/shop's CI once the watcher has read it.
fn shop(sandbox: &Sandbox) -> Value {
    let mut seen = Value::Null;
    assert!(
        poll_until(|| {
            seen = status(sandbox)["ci"]["repos"][0].clone();
            !seen["seen"].is_null()
        }),
        "never read: {seen}"
    );
    seen
}

#[test]
fn status_shows_the_fleet_and_each_open_factory_prs_checks_once_a_wait_wakes_the_watcher() {
    let sandbox = watching(4);
    let github = FakeGitHub::install(&sandbox);
    github.runners(&[
        (1, "ci-1", "online", true),
        (2, "ci-2", "online", false),
        (3, "ci-3", "offline", false),
    ]);
    github.pulls(&[(12, "factory/s1-t2"), (13, "feature/by-hand")]);
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "completed", Some("failure")),
    ]);

    // A wait on one of the repo's pull requests is something to watch.
    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 10);
    let shop = shop(&sandbox);

    assert_eq!(status(&sandbox)["ci"]["expected"], 4);
    assert_eq!(shop["repo"], "acme/shop");
    assert_eq!(shop["error"], Value::Null, "{shop}");
    assert_eq!(
        shop["runners"],
        json!([
            {"id": 1, "name": "ci-1", "state": "busy"},
            {"id": 2, "name": "ci-2", "state": "idle"},
            {"id": 3, "name": "ci-3", "state": "offline"},
        ]),
        "{shop}"
    );
    assert_eq!(
        shop["pulls"],
        json!([{
            "number": 12,
            "branch": "factory/s1-t2",
            "url": "https://github.com/acme/shop/pull/12",
            "state": "failing",
        }]),
        "only factory/* pull requests: {shop}"
    );
}

/// Moves the daemon's clock to `secs` seconds past the system's.
fn move_clock(sandbox: &Sandbox, secs: u64) {
    std::fs::write(sandbox.path("clock-skew"), secs.to_string()).unwrap();
}

/// How many times the watcher has listed acme/shop's open pull requests.
fn pull_reads(github: &FakeGitHub, sandbox: &Sandbox) -> usize {
    github
        .watcher_reads(sandbox)
        .iter()
        .filter(|path| path.contains("/pulls?"))
        .count()
}

/// What `factory status --json` shows of the CI once the watcher has read acme/shop and found
/// nothing left to watch: it reads next when nothing is due, only when a wait wakes it.
fn idle(sandbox: &Sandbox) -> Value {
    let mut ci = Value::Null;
    assert!(
        poll_until(|| {
            ci = status(sandbox)["ci"].clone();
            !ci["repos"][0]["seen"].is_null() && ci["next"].is_null()
        }),
        "never idle: {ci}"
    );
    ci
}

/// Waits until the watcher has read acme/shop and set when it reads next.
fn awaits_next_read(sandbox: &Sandbox) -> Value {
    let mut ci = Value::Null;
    assert!(
        poll_until(|| {
            ci = status(sandbox)["ci"].clone();
            !ci["next"].is_null()
        }),
        "no next read: {ci}"
    );
    ci
}

/// The watcher's reads of acme/shop's runners that sent the ETag of the last answer.
fn conditional_runner_reads(sandbox: &Sandbox) -> usize {
    sandbox
        .calls("gh")
        .iter()
        .filter(|call| call.iter().any(|arg| arg.contains("/actions/runners?")))
        .filter(|call| call.iter().any(|arg| arg.starts_with("If-None-Match: ")))
        .count()
}

/// Waits until the watcher has listed the open pull requests `count` times.
fn await_reads(github: &FakeGitHub, sandbox: &Sandbox, count: usize) {
    assert!(
        poll_until(|| pull_reads(github, sandbox) >= count),
        "the watcher read {} times, not {count}",
        pull_reads(github, sandbox)
    );
}

fn bar_ci(sandbox: &Sandbox) -> (Value, Value) {
    let out = sandbox.factory(&["bar", "--json"]);
    assert_eq!(out.code, 0, "{out:?}");
    let feed: Value = serde_json::from_str(&out.stdout).unwrap();
    let chip = feed["glance"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chip| chip["id"] == "ci")
        .cloned()
        .unwrap_or(Value::Null);
    let card = feed["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["id"] == "ci")
        .cloned()
        .unwrap_or(Value::Null);
    (chip, card)
}

#[test]
fn an_idle_factory_costs_github_nothing_across_minutes_of_daemon_time() {
    let sandbox = watching(2);
    let github = FakeGitHub::install(&sandbox);

    // The daemon runs, with no wait, no open factory pull request and no job: no read is due.
    for minutes in [1, 3, 10] {
        move_clock(&sandbox, minutes * 60);
        let ci = status(&sandbox)["ci"].clone();
        assert!(ci["next"].is_null(), "{ci}");
        assert!(ci["repos"][0]["seen"].is_null(), "{ci}");
    }
    assert_eq!(sandbox.calls("gh"), Vec::<Vec<String>>::new());

    // A wait on a pull request no longer open wakes one read, which finds nothing to watch.
    github.checks(&[(1, "test", "completed", Some("success"))]);
    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 0);
    idle(&sandbox);
    let reads = github.watcher_reads(&sandbox);
    assert!(!reads.is_empty());
    for minutes in [13, 20, 60] {
        move_clock(&sandbox, minutes * 60);
        assert!(status(&sandbox)["ci"]["next"].is_null());
    }
    assert_eq!(github.watcher_reads(&sandbox), reads);
}

#[test]
fn the_watcher_reads_each_minute_while_a_factory_pr_is_open_and_stops_once_none_is() {
    let sandbox = watching(2);
    let github = FakeGitHub::install(&sandbox);
    github.pulls(&[(12, "factory/s1-t2")]);
    github.checks(&[(1, "test", "completed", Some("success"))]);

    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 0);
    let ci = awaits_next_read(&sandbox);
    assert_eq!(
        pull_reads(&github, &sandbox),
        1,
        "a minute has not passed: {ci}"
    );
    assert_eq!(
        conditional_runner_reads(&sandbox),
        0,
        "nothing to send back yet"
    );

    move_clock(&sandbox, 61);
    await_reads(&github, &sandbox, 2);
    assert_eq!(
        conditional_runner_reads(&sandbox),
        1,
        "the second read sends back the first answer's ETag"
    );

    // A restarted daemon resumes the watch from what it kept, with no wait to wake it.
    stop_daemon(&sandbox);
    github.pulls(&[]);
    assert_eq!(status(&sandbox)["ci"]["repos"][0]["pulls"][0]["number"], 12);
    await_reads(&github, &sandbox, 3);
    assert_eq!(shop(&sandbox)["pulls"], json!([]));

    // Nothing is left to watch.
    idle(&sandbox);
    for minutes in [3, 10] {
        move_clock(&sandbox, minutes * 60);
        assert!(status(&sandbox)["ci"]["next"].is_null());
    }
    assert_eq!(pull_reads(&github, &sandbox), 3);
}

#[test]
fn each_jobs_queued_started_and_finished_times_are_stored() {
    let sandbox = watching(2);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "completed", Some("success"))]);
    let queued = Job {
        id: 70,
        name: "test",
        status: "queued",
        conclusion: None,
        created: "2026-01-01T10:00:00Z",
        // GitHub sets started_at on a job that still waits.
        started: Some("2026-01-01T10:00:00Z"),
        completed: None,
        runner: None,
    };
    github.workflow_runs("queued", &[7]);
    github.jobs(7, &[queued]);

    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 0);
    let shop = shop(&sandbox);
    assert_eq!(
        shop["jobs"],
        json!([{
            "id": 70, "run_id": 7, "name": "test", "status": "queued", "conclusion": null,
            "runner": null, "queued": 1_767_261_600_000_i64, "started": null, "finished": null,
        }]),
        "{shop}"
    );
    let (chip, card) = bar_ci(&sandbox);
    assert_eq!(chip["text"], "busy", "{chip}");
    let row = &card["rows"][1];
    assert_eq!(row["label"], "queue", "{card}");
    assert!(
        row["text"]
            .as_str()
            .unwrap()
            .starts_with("1 queued, oldest "),
        "{card}"
    );

    github.workflow_runs("queued", &[]);
    github.workflow_runs("in_progress", &[7]);
    let running = Job {
        status: "in_progress",
        started: Some("2026-01-01T10:02:00Z"),
        runner: Some("ci-1"),
        ..queued
    };
    github.jobs(7, &[running]);
    move_clock(&sandbox, 61);
    await_reads(&github, &sandbox, 2);

    // The run is no longer active, but its job was: its end is still read.
    github.workflow_runs("in_progress", &[]);
    github.jobs(
        7,
        &[Job {
            status: "completed",
            conclusion: Some("success"),
            completed: Some("2026-01-01T10:07:00Z"),
            ..running
        }],
    );
    move_clock(&sandbox, 122);
    await_reads(&github, &sandbox, 3);
    let finished = json!([{
        "id": 70, "run_id": 7, "name": "test", "status": "completed", "conclusion": "success",
        "runner": "ci-1", "queued": 1_767_261_600_000_i64, "started": 1_767_261_720_000_i64,
        "finished": 1_767_262_020_000_i64,
    }]);
    let shop = status(&sandbox)["ci"]["repos"][0].clone();
    assert_eq!(shop["jobs"], json!([]), "{shop}");
    assert_eq!(shop["finished"], finished, "{shop}");

    // The times are kept across a restart.
    stop_daemon(&sandbox);
    assert_eq!(status(&sandbox)["ci"]["repos"][0]["finished"], finished);
}

#[test]
fn a_run_that_queued_and_completed_between_two_reads_keeps_its_jobs_times() {
    let sandbox = watching(2);
    let github = FakeGitHub::install(&sandbox);
    github.pulls(&[(12, "factory/s1-t2")]);
    github.checks(&[(1, "test", "completed", Some("success"))]);
    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 0);
    awaits_next_read(&sandbox);

    // Never queued nor in progress at a read: only the runs created since the last one name it.
    github.created_runs(&[8]);
    github.jobs(
        8,
        &[Job {
            id: 80,
            name: "lint",
            status: "completed",
            conclusion: Some("failure"),
            created: "2026-01-01T10:00:00Z",
            started: Some("2026-01-01T10:00:30Z"),
            completed: Some("2026-01-01T10:01:00Z"),
            runner: Some("ci-2"),
        }],
    );
    move_clock(&sandbox, 61);
    let mut shop = Value::Null;
    assert!(
        poll_until(|| {
            shop = status(&sandbox)["ci"]["repos"][0].clone();
            shop["finished"] != json!([])
        }),
        "never read: {shop}"
    );
    assert!(
        github
            .watcher_reads(&sandbox)
            .iter()
            .any(|path| path.contains("/actions/runs?created=%3E%3D")),
        "asked for the runs created since"
    );
    assert_eq!(
        shop["finished"],
        json!([{
            "id": 80, "run_id": 8, "name": "lint", "status": "completed",
            "conclusion": "failure", "runner": "ci-2", "queued": 1_767_261_600_000_i64,
            "started": 1_767_261_630_000_i64, "finished": 1_767_261_660_000_i64,
        }]),
        "{shop}"
    );
}

#[test]
fn the_card_counts_busy_runners_against_the_expected_and_links_each_factory_pr() {
    let sandbox = watching(4);
    let github = FakeGitHub::install(&sandbox);
    github.runners(&[(1, "ci-1", "online", true), (2, "ci-2", "online", false)]);
    github.pulls(&[(12, "factory/s1-t2")]);
    github.checks(&[(1, "test", "completed", Some("success"))]);

    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 0);
    shop(&sandbox);

    let (chip, card) = bar_ci(&sandbox);
    assert_eq!(chip["text"], "✓", "{chip}");
    assert_eq!(chip["tone"], "ok", "{chip}");
    assert_eq!(card["title"], "CI");
    assert_eq!(card["rows"][0]["kind"], "meter", "{card}");
    assert_eq!(card["rows"][0]["text"], "1/4 busy, 2 missing", "{card}");
    assert_eq!(card["rows"][1]["text"], "none queued", "{card}");
    assert_eq!(
        card["rows"][2],
        json!({
            "kind": "text", "label": "shop#12", "text": "passing", "tone": "ok",
            "url": "https://github.com/acme/shop/pull/12",
        }),
        "{card}"
    );
    let line = sandbox.factory(&["bar"]);
    assert!(line.stdout.ends_with(" · CI ✓\n"), "{line:?}");
}

/// After a good read, a read that fails as `body` does keeps the good numbers, stale since
/// that read, and says why; the next good read clears it.
fn stale_after(body: &str, why: &str) {
    let sandbox = watching(2);
    let github = FakeGitHub::install(&sandbox);
    github.runners(&[(1, "ci-1", "online", true), (2, "ci-2", "online", false)]);
    github.pulls(&[(12, "factory/s1-t2")]);
    github.checks(&[(1, "test", "completed", Some("failure"))]);
    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 10);
    let good = shop(&sandbox);

    github.fail_actions(body);
    move_clock(&sandbox, 61);
    let mut seen = Value::Null;
    assert!(
        poll_until(|| {
            seen = status(&sandbox)["ci"]["repos"][0].clone();
            !seen["error"].is_null()
        }),
        "the failure never showed: {seen}"
    );
    let error = seen["error"].as_str().unwrap();
    assert!(error.contains(why), "{seen}");
    assert!(
        error.contains("gh auth status"),
        "names a next step: {seen}"
    );
    assert_eq!(seen["seen"], good["seen"]);
    assert_eq!(seen["runners"], good["runners"], "never no runners");
    assert_eq!(seen["pulls"], good["pulls"]);

    let (chip, card) = bar_ci(&sandbox);
    assert_eq!(chip["text"], "✗", "the last good answer: {chip}");
    let since =
        factory_core::timestamp::Timestamp::from_unix_millis(good["seen"].as_i64().unwrap());
    assert_eq!(card["stale_since"], since.to_string(), "{card}");
    assert_eq!(card["rows"][0]["text"], "1/2 busy", "{card}");

    github.heal_actions();
    move_clock(&sandbox, 122);
    assert!(
        poll_until(|| status(&sandbox)["ci"]["repos"][0]["error"].is_null()),
        "the good read never cleared it"
    );
    assert!(bar_ci(&sandbox).1["stale_since"].is_null());
}

#[test]
fn a_403_keeps_the_last_good_answer_marked_stale() {
    stale_after(FORBIDDEN, "HTTP 403");
}

#[test]
fn a_spent_rate_limit_keeps_the_last_good_answer_marked_stale() {
    stale_after(RATE_LIMITED, "rate limit");
}

#[test]
fn a_gh_timeout_keeps_the_last_good_answer_marked_stale() {
    stale_after(TIMED_OUT, "Timeout");
}

#[test]
fn a_repo_that_never_answered_shows_why_and_no_numbers() {
    let sandbox = watching(2);
    let github = FakeGitHub::install(&sandbox);
    github.fail_actions(FORBIDDEN);
    github.checks(&[(1, "test", "completed", Some("success"))]);

    assert_eq!(sandbox.factory(&["wait", "ci", "12"]).code, 0);
    let mut card = Value::Null;
    assert!(
        poll_until(|| {
            card = bar_ci(&sandbox).1;
            !card["error"].is_null()
        }),
        "{card}"
    );
    assert!(
        card["error"].as_str().unwrap().starts_with("acme/shop: "),
        "{card}"
    );
    assert_eq!(
        card["rows"],
        json!([]),
        "no numbers rather than none: {card}"
    );
    assert_eq!(bar_ci(&sandbox).0["text"], "?");
}
