//! `factory wait ci`, against a fake GitHub behind the fake `gh`.

use std::fs;

use std::os::unix::fs::PermissionsExt;

use crate::support::github::{FakeGitHub, HEAD, PR_URL};
use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;
use crate::support::{Sandbox, finish, freeze, spawn, stop_daemon, thaw};

/// A sandbox whose config polls GitHub every `seconds`.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn polling_every(seconds: u64) -> Sandbox {
    let sandbox = Sandbox::new();
    let config = sandbox.path("config/factory");
    std::fs::create_dir_all(&config).expect("create the config dir");
    std::fs::write(
        config.join("config.toml"),
        format!("[ci]\npoll = \"{seconds}s\"\n"),
    )
    .expect("write the config");
    sandbox
}

#[test]
fn wait_is_silent_while_checks_run_then_exits_0_when_all_pass() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "in_progress", None),
    ]);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(poll_until(|| !github.polls().is_empty()), "never polled");

    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "completed", Some("success")),
    ]);
    let out = finish(child);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "acme/shop#12: 2 passed, 0 failed\n", ""),
        "{out:?}"
    );
}

#[test]
fn failed_checks_exit_10_naming_them_and_a_file_with_each_url_and_log_tail() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    let log: String = (1..=60).map(|n| format!("step {n}\n")).collect();
    github.log(2, &format!("{log}error: 3 tests failed\n\n"));
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "completed", Some("failure")),
        (3, "e2e", "completed", Some("timed_out")),
    ]);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    let file = sandbox.path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md");
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            10,
            format!(
                "acme/shop#12: 1 passed, 2 failed; logs in {}; failed: test, e2e\n",
                file.display()
            )
            .as_str(),
            ""
        ),
        "{out:?}"
    );
    let text = fs::read_to_string(&file).expect("read the failures file");
    let mode =
        |path: &std::path::Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600, "the logs can be a private repo's");
    assert_eq!(mode(file.parent().expect("a dir")), 0o700);
    assert!(
        text.contains("https://github.com/acme/shop/actions/runs/7/job/2\n"),
        "{text}"
    );
    assert!(
        text.contains("https://github.com/acme/shop/actions/runs/7/job/3\n"),
        "{text}"
    );
    assert!(text.contains("step 22\n"), "{text}");
    assert!(text.contains("error: 3 tests failed\n"), "{text}");
    assert!(
        !text.contains("step 21\n"),
        "the tail keeps the last 40 lines: {text}"
    );
    assert!(text.contains("No log: gh: Not Found (HTTP 404)"), "{text}");
}

#[test]
fn cancelled_checks_count_apart_and_their_logs_are_not_fetched() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.log(2, "error: 1 test failed\n");
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "completed", Some("failure")),
        (3, "e2e", "completed", Some("cancelled")),
        (4, "build", "completed", Some("cancelled")),
        (5, "deploy", "completed", Some("startup_failure")),
    ]);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    let file = sandbox.path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md");
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            10,
            format!(
                "acme/shop#12: 1 passed, 2 failed, 2 cancelled; logs in {}; failed: test, deploy\n",
                file.display()
            )
            .as_str(),
            ""
        ),
        "{out:?}"
    );
    assert_eq!(github.log_reads(), [2], "only a job that ran has a log");
    let text = fs::read_to_string(&file).expect("read the failures file");
    assert!(text.contains("2 failed, 2 cancelled\n"), "{text}");
    assert!(text.contains("## e2e (cancelled)\n"), "{text}");
    assert!(text.contains("## build (cancelled)\n"), "{text}");
    assert!(text.contains("error: 1 test failed\n"), "{text}");
    assert!(!text.contains("Not Found"), "{text}");
}

#[test]
fn a_head_whose_newest_runs_passed_passes_whatever_they_superseded() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[
        (1, "lint", "completed", Some("cancelled")),
        (2, "test", "completed", Some("failure")),
        (3, "lint", "completed", Some("success")),
        (4, "test", "completed", Some("success")),
    ]);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "acme/shop#12: 2 passed, 0 failed\n", ""),
        "{out:?}"
    );
}

#[test]
fn a_head_whose_newest_run_was_cancelled_needs_attention() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "completed", Some("cancelled")),
    ]);

    let out = sandbox.factory(&["--json", "wait", "ci", "12"]);

    assert_eq!(out.code, 10, "{out:?}");
    let answer: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(answer["cancelled"], serde_json::json!(["test"]), "{answer}");
    assert_eq!(answer["failed"], serde_json::json!([]), "{answer}");
}

#[test]
fn a_job_log_full_of_escape_sequences_lands_plain() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.log(
        2,
        "\u{1b}[36;1mrun tests\u{1b}[0m\n\u{1b}[31merror\u{1b}[0m: 3 tests failed\n",
    );
    github.checks(&[(2, "test", "completed", Some("failure"))]);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(out.code, 10, "{out:?}");
    let file = sandbox.path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md");
    let text = fs::read_to_string(&file).expect("read the failures file");
    assert!(
        text.contains("run tests\nerror: 3 tests failed\n"),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'), "{text}");
}

#[test]
fn skipped_and_neutral_checks_are_not_failures() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "deploy", "completed", Some("skipped")),
        (3, "lint-docs", "completed", Some("neutral")),
    ]);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "acme/shop#12: 1 passed, 0 failed, 2 skipped\n"),
        "{out:?}"
    );
}

#[test]
fn a_pending_check_beside_a_failed_one_is_waited_for_not_reported() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[
        (1, "lint", "completed", Some("failure")),
        (2, "test", "queued", None),
    ]);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(
        poll_until(|| github.polls().len() >= 2),
        "never polled twice"
    );

    github.checks(&[
        (1, "lint", "completed", Some("failure")),
        (2, "test", "completed", Some("success")),
    ]);
    let out = finish(child);

    assert_eq!(out.code, 10, "{out:?}");
    assert!(
        out.stdout
            .starts_with("acme/shop#12: 1 passed, 1 failed; logs in ")
            && out.stdout.ends_with("; failed: lint\n"),
        "{out:?}"
    );
}

#[test]
fn a_head_without_checks_yet_is_pending() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(poll_until(|| !github.polls().is_empty()), "never polled");

    github.checks(&[(1, "lint", "completed", Some("success"))]);
    let out = finish(child);

    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "acme/shop#12: 1 passed, 0 failed\n"),
        "{out:?}"
    );
}

#[test]
fn a_wait_carries_on_across_a_daemon_restart_and_hears_what_happened_meanwhile() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(poll_until(|| !github.polls().is_empty()), "never polled");

    // The CLI sleeps through the restart; the checks finish while no daemon runs.
    freeze(&child);
    stop_daemon(&sandbox);
    github.checks(&[(1, "test", "completed", Some("success"))]);
    thaw(&child);
    let out = finish(child);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "acme/shop#12: 1 passed, 0 failed\n", ""),
        "{out:?}"
    );
}

#[test]
fn checks_still_running_at_the_timeout_exit_124_with_nothing_on_stdout() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);

    let out = sandbox.factory(&["wait", "ci", "12", "--timeout", "2s"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            124,
            "",
            "factory: acme/shop#12 still has 1 check running after 2s; wait again\n"
        ),
        "{out:?}"
    );
}

#[test]
fn a_pr_gh_cannot_find_is_refused_with_its_reason() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.forget_pr();

    let out = sandbox.factory(&["wait", "ci", "work"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            3,
            "",
            "factory: cannot find pull request work: no pull requests found for branch \"work\"; \
             name it by number from its repo's checkout, or by URL\n"
        ),
        "{out:?}"
    );
}

#[test]
fn repeated_polls_of_unchanged_checks_send_the_previous_etag() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(
        poll_until(|| github.polls().len() >= 3),
        "never polled 3 times"
    );

    github.checks(&[(1, "test", "completed", Some("success"))]);
    let out = finish(child);

    assert_eq!(out.code, 0, "{out:?}");
    let statuses: Vec<u16> = github.polls().iter().map(|poll| poll.status).collect();
    assert_eq!(statuses.first(), Some(&200), "{statuses:?}");
    assert_eq!(statuses.last(), Some(&200), "{statuses:?}");
    assert!(
        statuses[1..statuses.len() - 1].iter().all(|s| *s == 304),
        "{statuses:?}"
    );
    let reads: Vec<Vec<String>> = sandbox
        .calls("gh")
        .into_iter()
        .filter(|call| call.first().map(String::as_str) == Some("api"))
        .filter(|call| call.last().is_some_and(|path| path.contains("/check-runs")))
        .collect();
    let etag = |call: &Vec<String>| {
        call.iter()
            .find_map(|arg| arg.strip_prefix("If-None-Match: ").map(str::to_owned))
    };
    assert_eq!(etag(&reads[0]), None, "{reads:?}");
    let first = etag(&reads[1]).expect("the second read sends an ETag");
    assert!(first.starts_with("W/\""), "{reads:?}");
    assert!(
        reads[1..]
            .iter()
            .all(|call| etag(call).as_ref() == Some(&first)),
        "{reads:?}"
    );
}

/// Shows the lead in `pane` (`lead-<slug>`) as waiting once its wait is registered.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn await_waiting(sandbox: &Sandbox, slug: &str) {
    let want = format!("lead-{slug} waiting: ci acme/shop#12\n");
    let mut last = None;
    let seen = poll_until(|| {
        let out = sandbox.factory(&["lead", "status", slug]);
        let done = out.stdout == want;
        last = Some(out);
        done
    });
    assert!(seen, "never saw {want:?}; last {last:?}");
}

#[test]
fn three_waiters_on_one_pr_share_one_poll_per_interval() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let herdr = FakeHerdr::start(&sandbox);
    herdr.workspace("w1", "shop");
    let leads = [("a", "w1:p1"), ("b", "w1:p2"), ("c", "w1:p3")];
    for (slug, pane) in leads {
        herdr.agent(Some(&format!("lead-{slug}")), pane, "working");
    }
    let children: Vec<_> = leads
        .iter()
        .map(|(_, pane)| {
            let mut cmd = sandbox.factory_command(&["wait", "ci", "12"]);
            cmd.env("HERDR_PANE_ID", pane);
            spawn(cmd)
        })
        .collect();
    for (slug, _) in leads {
        await_waiting(&sandbox, slug);
    }
    assert!(
        poll_until(|| github.polls().len() >= 3),
        "never polled 3 times"
    );

    github.checks(&[(1, "test", "completed", Some("success"))]);
    let outs: Vec<_> = children.into_iter().map(finish).collect();

    for out in &outs {
        assert_eq!(
            (out.code, out.stdout.as_str()),
            (0, "acme/shop#12: 1 passed, 0 failed\n"),
            "{out:?}"
        );
    }
    let polls = github.polls();
    let fresh = polls.iter().filter(|poll| poll.status == 200).count();
    assert_eq!(
        fresh, 2,
        "one first read and one of the finished checks: {polls:?}"
    );
    for pair in polls.windows(2) {
        let gap_ms = (pair[1].at_ns - pair[0].at_ns) / 1_000_000;
        assert!(gap_ms >= 900, "two polls {gap_ms} ms apart: {polls:?}");
    }
    assert_eq!(
        sandbox.factory(&["lead", "status", "a"]).stdout,
        "lead-a working\n",
        "the wait is cleared once answered"
    );
}

#[test]
fn gh_event_listen_drives_a_wait_to_completion_with_zero_polls() {
    // An hour between polls: a wait that ends sooner was driven by events.
    let sandbox = polling_every(3600);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    github.stage_checks(&[(1, "test", "completed", Some("success"))]);
    // Attach, wait for the read that follows, then report the check suite completed.
    github.listen(
        r#"echo $$ > "$G/listener.pid"
echo "Listening for events on https://github.com/acme/shop/pull/12" >&2
while [ ! -s "$G/polls" ]; do sleep 0.05; done
mv "$G/next.json" "$G/checks.json"
echo '{"type":"com.github.pull_request.check_suite.status.changed.v1","data":{"conclusion":"success"}}'
exec tail -f /dev/null"#,
    );

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "acme/shop#12: 1 passed, 0 failed\n"),
        "{out:?}"
    );
    let statuses: Vec<u16> = github.polls().iter().map(|poll| poll.status).collect();
    assert_eq!(
        statuses,
        vec![200, 200],
        "one read on attach, one on the event"
    );
    let listens: Vec<Vec<String>> = sandbox
        .calls("gh")
        .into_iter()
        .filter(|call| call.first().map(String::as_str) == Some("event"))
        .collect();
    assert_eq!(
        listens,
        vec![
            [
                "event",
                "listen",
                "--resource",
                PR_URL,
                "--event",
                "com.github.pull_request.check_suite.status.changed.v1,\
                 com.github.pull_request.head.changed.v1",
                "--follow",
            ]
            .map(str::to_owned)
            .to_vec()
        ]
    );
    let pid = fs::read_to_string(sandbox.path("github/listener.pid")).expect("read the pid");
    let proc = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    assert!(
        poll_until(|| !proc.exists()),
        "the listener outlived the last wait on its PR"
    );
}

#[test]
fn a_listener_that_ends_falls_back_to_polling() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    github.listen(
        r#"echo "Listening for events on https://github.com/acme/shop/pull/12" >&2
while [ ! -s "$G/polls" ]; do sleep 0.05; done
echo "event stream closed: HTTP 502" >&2
exit 1"#,
    );
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(
        poll_until(|| github.polls().len() >= 2),
        "never polled after the listener ended"
    );

    github.checks(&[(1, "test", "completed", Some("success"))]);
    let out = finish(child);

    assert_eq!(
        (out.code, out.stdout.as_str()),
        (0, "acme/shop#12: 1 passed, 0 failed\n"),
        "{out:?}"
    );
}

#[test]
fn a_push_during_the_wait_moves_it_to_the_new_heads_checks() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "in_progress", None)]);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(poll_until(|| !github.polls().is_empty()), "never polled");

    github.push("beef");
    github.checks(&[(2, "test", "completed", Some("success"))]);
    let out = finish(child);

    assert_eq!(out.code, 0, "{out:?}");
    let reads: Vec<String> = sandbox
        .calls("gh")
        .into_iter()
        .filter(|call| call.iter().any(|arg| arg.contains("/check-runs")))
        .map(|call| call.join(" "))
        .collect();
    let first = reads.first().expect("a first read");
    let last = reads.last().expect("a last read");
    assert!(
        first.contains(&format!("commits/{HEAD}/check-runs")),
        "{reads:?}"
    );
    assert!(
        last.contains("commits/beef/check-runs") && !last.contains("If-None-Match"),
        "the new head's checks are read afresh: {reads:?}"
    );
}

#[test]
fn json_carries_the_whole_answer() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[
        (1, "lint", "completed", Some("success")),
        (2, "test", "completed", Some("failure")),
    ]);

    let out = sandbox.factory(&["--json", "wait", "ci", "12"]);

    assert_eq!(out.code, 10, "{out:?}");
    let answer: serde_json::Value = serde_json::from_str(&out.stdout).expect("one JSON line");
    assert_eq!(answer["pr"], "acme/shop#12");
    assert_eq!(answer["url"], PR_URL);
    assert_eq!(answer["head"], HEAD);
    assert_eq!(answer["passed"], 1);
    assert_eq!(answer["failed"], serde_json::json!(["test"]));
    assert_eq!(answer["exit"], 10);
    assert!(
        answer["file"]
            .as_str()
            .is_some_and(|f| f.ends_with("ci/github.com/acme/shop/12-c0ffee123456.md")),
        "{answer}"
    );
}

#[test]
fn commit_statuses_count_as_checks() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "lint", "completed", Some("success"))]);
    github.statuses(&[("ci/circleci", "pending")]);
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(
        poll_until(|| github.polls().len() >= 2),
        "never polled twice"
    );

    github.statuses(&[("ci/circleci", "failure")]);
    let out = finish(child);

    assert_eq!(out.code, 10, "{out:?}");
    assert!(out.stdout.ends_with("; failed: ci/circleci\n"), "{out:?}");
    let text = fs::read_to_string(
        sandbox.path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md"),
    )
    .expect("read the failures file");
    assert!(
        text.contains("https://ci.example.com/acme/shop/ci/circleci\n\nNo log here: a commit status keeps it at the URL."),
        "{text}"
    );
}

#[test]
fn every_page_of_check_runs_is_read() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    let names: Vec<String> = (1..=150).map(|n| format!("shard-{n}")).collect();
    let runs: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let conclusion = if i == 140 { "failure" } else { "success" };
            (
                i as u64 + 1,
                name.as_str(),
                "completed",
                Some(conclusion),
                "github-actions",
            )
        })
        .collect();
    github.runs(&runs);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(out.code, 10, "{out:?}");
    assert!(
        out.stdout
            .starts_with("acme/shop#12: 149 passed, 1 failed;")
            && out.stdout.ends_with("; failed: shard-141\n"),
        "{out:?}"
    );
}

#[test]
fn a_check_from_another_app_points_at_its_url_for_the_log() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.runs(&[
        (1, "lint", "completed", Some("success"), "github-actions"),
        (2, "security", "completed", Some("failure"), "snyk"),
    ]);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(out.code, 10, "{out:?}");
    let text = fs::read_to_string(
        sandbox.path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md"),
    )
    .expect("read the failures file");
    assert!(
        text.contains("No log here: snyk keeps it at the URL."),
        "{text}"
    );
    let log_reads = sandbox
        .calls("gh")
        .into_iter()
        .filter(|call| call.iter().any(|arg| arg.ends_with("/logs")))
        .count();
    assert_eq!(
        log_reads, 0,
        "only an Actions job has a log behind the jobs API"
    );
}

#[test]
fn many_failures_name_as_many_as_fit_and_count_the_rest() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    let names: Vec<String> = (1..=30)
        .map(|n| format!("integration-shard-{n:02}"))
        .collect();
    let runs: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (i as u64 + 1, name.as_str(), "completed", Some("failure")))
        .collect();
    github.checks(&runs);

    let out = sandbox.factory(&["wait", "ci", "12"]);

    assert_eq!(out.code, 10, "{out:?}");
    let line = out.stdout.trim_end();
    assert!(line.chars().count() <= 200, "{line:?}");
    let file = sandbox.path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md");
    assert!(
        line.contains(&format!(
            "; logs in {}; failed: integration-shard-01, ",
            file.display()
        )),
        "{line:?}"
    );
    let (shown, rest) = line
        .rsplit_once(" and ")
        .and_then(|(head, tail)| Some((head, tail.strip_suffix(" more")?.parse::<usize>().ok()?)))
        .expect("a count of the names left out");
    let named = shown
        .split("; failed: ")
        .nth(1)
        .expect("names")
        .split(", ")
        .count();
    assert_eq!(named + rest, 30, "{line:?}");
}

#[test]
fn the_timeout_counts_finding_the_pr() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "lint", "completed", Some("success"))]);
    github.hold_pr_view();

    let out = sandbox.factory(&["wait", "ci", "12", "--timeout", "1s"]);
    github.answer_pr_view();

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            124,
            "",
            "factory: gh found no pull request 12 within 1s; wait again\n"
        ),
        "{out:?}"
    );
}

#[test]
fn a_failures_file_that_cannot_be_written_is_retried_on_the_next_read() {
    let sandbox = polling_every(1);
    let github = FakeGitHub::install(&sandbox);
    github.checks(&[(1, "test", "completed", Some("failure"))]);
    // A file where the dir should be makes the first write fail.
    let blocker = sandbox.path("state/factory/ci");
    fs::create_dir_all(blocker.parent().expect("a parent")).expect("create the state dir");
    fs::write(&blocker, "").expect("block the ci dir");
    let child = spawn(sandbox.factory_command(&["wait", "ci", "12"]));
    assert!(poll_until(|| !github.polls().is_empty()), "never polled");

    fs::remove_file(&blocker).expect("unblock the ci dir");
    let out = finish(child);

    assert_eq!(out.code, 10, "{out:?}");
    assert!(out.stdout.ends_with("; failed: test\n"), "{out:?}");
    assert!(
        sandbox
            .path("state/factory/ci/github.com/acme/shop/12-c0ffee123456.md")
            .exists()
    );
}
