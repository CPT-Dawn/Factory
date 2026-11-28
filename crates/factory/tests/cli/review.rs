//! `factory review <base> <label> [spec] [prev]`: a Codex review over a prepared bundle.

use std::fs;

use serde_json::Value;

use crate::router::configure;
use crate::support::Sandbox;

/// Codex the one backend, so a test about Codex's review meets no agy pool on the way: an agy
/// whose usage can't be read is skipped with a line on stderr.
fn codex_only(sandbox: &Sandbox) {
    configure(sandbox, "[[review.backend]]\ncli = \"codex\"\n");
}

pub(crate) const CLEAN_REPORT: &str = r#"## Standards

Nothing hard.

## Findings ledger

```json
{"findings":[{"severity":"advisory","axis":"standards","tag":"mysterious-name","location":"a.txt:1","summary":"a is a vague name"}]}
```

FINDINGS: standards_hard=0 standards_judgement=1 spec_findings=0 spec_advisory=0 questions=0 fixed=0 still_present=0 tags=mysterious-name
VERDICT: clean
"#;

pub(crate) const FINDINGS_REPORT: &str = r#"## Spec

Two things.

## Findings ledger

```json
{"findings":[
  {"severity":"blocking","axis":"spec","tag":"partial-requirement","location":"a.txt:1","summary":"a holds no b"},
  {"severity":"advisory","axis":"standards","tag":"mysterious-name","location":"a.txt:1","summary":"a is a vague name"},
  {"severity":"question","axis":"spec","tag":"spec-conflict","location":"README.md:1","summary":"which title?"},
  {"severity":"blocking","axis":"standards","tag":"missing-test","location":"a.txt:1","summary":"nothing tests a"}
]}
```

FINDINGS: standards_hard=1 standards_judgement=1 spec_findings=1 spec_advisory=0 questions=1 fixed=0 still_present=0 tags=partial-requirement,mysterious-name,spec-conflict,missing-test
VERDICT: findings
"#;

/// A fake `codex` body that keeps the prompt it reads on stdin beside itself, as
/// `bin/codex.stdin`, and writes `report` as its final message, to the file after `-o`.
pub(crate) fn codex_writes(report: &str) -> String {
    format!(
        "out=\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\n\
         cat > \"$0.stdin\"\n\
         echo '{{\"type\":\"turn.completed\"}}'\n\
         cat > \"$out\" <<'REPORT'\n{report}REPORT"
    )
}

#[test]
fn a_clean_review_prints_one_line_naming_the_report_and_exits_0() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    codex_only(&sandbox);
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let report = sandbox.reviews().join("t1-r1.md");
    assert_eq!(
        out.stdout,
        format!(
            "review t1-r1: clean (0 blocking, 1 advisory, 0 question) → {}\n",
            report.display()
        )
    );
    assert_eq!(out.stderr, "");
    assert_eq!(fs::read_to_string(&report).unwrap(), CLEAN_REPORT);
}

#[test]
fn a_report_path_past_the_line_limit_is_shown_whole_so_a_shim_can_read_it() {
    let base = tempfile::Builder::new()
        .prefix("factory-test-")
        .tempdir()
        .unwrap();
    let state = base
        .path()
        .join("s".repeat(60))
        .join("t".repeat(60))
        .join("u".repeat(60));
    let sandbox = Sandbox::new().env("FACTORY_STATE", &state);
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let report = state.join("reviews/repo/t1-r1.md");
    assert!(
        report.to_str().unwrap().len() > 200,
        "the test's path is long enough"
    );
    assert_eq!(
        out.stdout,
        format!(
            "review t1-r1: clean (0 blocking, 1 advisory, 0 question) → {}\n",
            report.display()
        )
    );
}

#[test]
fn blocking_findings_follow_the_line_and_exit_10() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    codex_only(&sandbox);
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 10, "{out:?}");
    let report = sandbox.reviews().join("t1-r1.md");
    assert_eq!(
        out.stdout_lines(),
        vec![
            format!(
                "review t1-r1: findings (2 blocking, 1 advisory, 1 question) → {}",
                report.display()
            )
            .as_str(),
            "- [spec/partial-requirement] a.txt:1: a holds no b",
            "- [standards/missing-test] a.txt:1: nothing tests a",
        ]
    );
    assert_eq!(out.stderr, "");
}

#[test]
fn an_unknown_base_or_an_empty_diff_fails_with_exit_2_before_codex_starts() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    for (base, says) in [
        ("nope", "factory: code-review: unknown ref: nope"),
        (
            "work",
            "factory: code-review: no committed changes since work",
        ),
    ] {
        let out = sandbox.factory_in(&repo, &["review", base, "t1-r1"]);

        assert_eq!(out.code, 2, "{base}: {out:?}");
        assert_eq!(out.stdout, "", "{base}");
        assert_eq!(out.stderr_lines().len(), 1, "{base}: {out:?}");
        assert!(out.stderr.starts_with(says), "{base}: {out:?}");
    }
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn a_label_outside_letters_digits_underscore_and_dash_is_refused_with_exit_2() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["review", "main", "t1/r1"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: code-review: label may only use letters, digits, _ and -, got: \"t1/r1\"\n"
    );
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn the_tier_a_caller_names_lands_in_the_review_event_and_none_is_detected_from_the_diff() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let tiered = sandbox.factory_in(&repo, &["review", "--tier", "hard", "main", "t1-r1"]);
    let untiered = sandbox.factory_in(&repo, &["review", "main", "t2-r1"]);

    assert_eq!(
        (tiered.code, untiered.code),
        (0, 0),
        "{tiered:?} {untiered:?}"
    );
    let events: Vec<Value> = sandbox
        .ledger_lines()
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events[0]["tier"], "hard", "{}", events[0]);
    assert_eq!(events[0]["tier_source"], "caller", "{}", events[0]);
    // The sandbox repo's diff is one line in a.txt, so with no tier it is trivial.
    assert_eq!(events[1]["tier"], "trivial", "{}", events[1]);
    assert_eq!(events[1]["tier_source"], "detected", "{}", events[1]);
}

#[test]
fn a_tier_outside_hard_standard_and_trivial_is_refused_with_exit_2() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["review", "--tier", "urgent", "main", "t1-r1"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("urgent"), "{out:?}");
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn the_bundle_holds_commits_diff_spec_previous_review_standards_and_evidence() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    fs::write(
        repo.join("CODING_STANDARDS.md"),
        "# Standards\n\nName things well.\n",
    )
    .unwrap();
    fs::write(repo.join(".git/factory-evidence.md"), "cargo test: pass\n").unwrap();
    let spec = sandbox.path("spec.md");
    fs::write(&spec, "# #5 Add a\n\nAdd a.txt.\n").unwrap();
    let previous = sandbox.path("t1-r1.md");
    fs::write(&previous, "a lacks a test\nVERDICT: findings\n").unwrap();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(
        &repo,
        &[
            "review",
            "main",
            "t1-r2",
            spec.to_str().unwrap(),
            previous.to_str().unwrap(),
        ],
    );

    assert_eq!(out.code, 0, "{out:?}");
    let bundle_path = sandbox.reviews().join("t1-r2.bundle.md");
    let bundle = fs::read_to_string(&bundle_path).unwrap();
    for part in [
        "feat: add a",
        "a.txt | 1 +",
        "+++ b/a.txt",
        "Add a.txt.",
        "a lacks a test",
        "Name things well.",
        "cargo test: pass",
    ] {
        assert!(bundle.contains(part), "{part:?} missing from:\n{bundle}");
    }
    let prompt = fs::read_to_string(sandbox.reviews().join("t1-r2.prompt")).unwrap();
    assert!(prompt.starts_with("$code-review since main.\n"), "{prompt}");
    assert!(
        prompt.contains(&format!("whole input: {}", bundle_path.display())),
        "{prompt}"
    );
    assert_eq!(
        fs::read_to_string(sandbox.path("bin/codex.stdin")).unwrap(),
        prompt
    );
    let call = &sandbox.calls("codex")[0];
    assert_eq!(call[..4], ["exec", "--json", "-s", "read-only"], "{call:?}");
}

#[test]
fn an_issue_ref_as_the_spec_is_fetched_with_gh_and_saved_as_a_file() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    sandbox.fake("gh", "printf '# #12 Add a\\n\\nAdd a.txt, from gh.\\n'");

    for (n, spec) in ["#12", "12", "https://github.com/o/r/issues/12", "o/r#12"]
        .into_iter()
        .enumerate()
    {
        let label = format!("s{n}-r1");
        let out = sandbox.factory_in(&repo, &["review", "main", &label, spec]);

        assert_eq!(out.code, 0, "{spec}: {out:?}");
        let saved = sandbox.reviews().join(format!("{label}.spec.md"));
        assert_eq!(
            fs::read_to_string(&saved).unwrap(),
            "# #12 Add a\n\nAdd a.txt, from gh.\n",
            "{spec}"
        );
        let bundle =
            fs::read_to_string(sandbox.reviews().join(format!("{label}.bundle.md"))).unwrap();
        assert!(bundle.contains("Add a.txt, from gh."), "{spec}: {bundle}");
        let prompt = fs::read_to_string(sandbox.reviews().join(format!("{label}.prompt"))).unwrap();
        assert!(
            prompt.contains(&format!("Spec: {}\n", saved.display())),
            "{spec}: {prompt}"
        );
    }
    // A URL goes to gh whole, and `owner/repo#n` as one, so an issue of another repo is
    // fetched from that repo.
    let template = r#"# #{{.number}} {{.title}}{{"\n\n"}}{{.body}}{{"\n"}}"#;
    let expected: Vec<Vec<&str>> = [
        "12",
        "12",
        "https://github.com/o/r/issues/12",
        "https://github.com/o/r/issues/12",
    ]
    .into_iter()
    .map(|issue| {
        vec![
            "issue",
            "view",
            issue,
            "--json",
            "number,title,body",
            "--template",
            template,
        ]
    })
    .collect();
    assert_eq!(sandbox.calls("gh"), expected);
}

#[test]
fn a_spec_gh_cannot_fetch_fails_with_exit_2_before_codex_starts() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    sandbox.fake(
        "gh",
        "echo 'GraphQL: Could not resolve to an issue' >&2; exit 1",
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1", "#99"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.starts_with(
            "factory: code-review: could not fetch spec #99 with gh: GraphQL: Could not resolve"
        ),
        "{out:?}"
    );
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn a_timeout_kills_the_reviewers_whole_process_group_and_exits_124() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let child_pid = sandbox.path("child.pid");
    sandbox.fake(
        "codex",
        &format!("sleep 300 &\necho $! > '{}'\nwait", child_pid.display()),
    );

    let out = sandbox.factory_in(&repo, &["review", "--timeout", "1", "main", "t1-r1"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: codex timed out after 1s"),
        "{out:?}"
    );
    let pid = fs::read_to_string(&child_pid).unwrap();
    let proc = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    assert!(
        eventually(|| !is_alive(&proc)),
        "the fake reviewer's child {} outlived the timeout",
        pid.trim()
    );
}

/// A process is alive while its /proc entry exists and it isn't a zombie waiting to be reaped.
fn is_alive(proc: &std::path::Path) -> bool {
    fs::read_to_string(proc.join("stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

/// Whether `done` holds within a few seconds, checked as it changes.
fn eventually(done: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::yield_now();
    }
    done()
}

#[test]
fn each_run_logs_one_review_event_with_backend_model_round_outcome_and_counts() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    fs::create_dir(sandbox.path("home/.codex")).unwrap();
    fs::write(
        sandbox.path("home/.codex/config.toml"),
        "model = \"gpt-test\"\nmodel_reasoning_effort = \"medium\"\n\n[profiles.x]\nmodel = \"other\"\n",
    )
    .unwrap();
    // Only codex, so its model and effort are what its own config leaves.
    fs::create_dir_all(sandbox.path("config/factory")).unwrap();
    fs::write(
        sandbox.path("config/factory/config.toml"),
        "[[review.backend]]\ncli = \"codex\"\n",
    )
    .unwrap();
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 10, "{out:?}");
    let lines = sandbox.ledger_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let event: Value = serde_json::from_str(&lines[0]).unwrap();
    let reviews = sandbox.reviews();
    for (key, expected) in [
        ("type", Value::from("review")),
        ("repo", Value::from("repo")),
        ("label", Value::from("t1-r1")),
        ("round", Value::from(1)),
        ("base", Value::from("main")),
        ("outcome", Value::from("ok")),
        ("verdict", Value::from("findings")),
        ("backend", Value::from("codex")),
        ("pool", Value::from("codex")),
        ("reviewer", Value::from("codex")),
        ("tier", Value::from("trivial")),
        ("tier_source", Value::from("detected")),
        ("model", Value::from("gpt-test")),
        ("effort", Value::from("medium")),
        ("blocking", Value::from(2)),
        ("advisory", Value::from(1)),
        ("question", Value::from(1)),
        ("standards_hard", Value::from(1)),
        (
            "tags",
            Value::from("partial-requirement,mysterious-name,spec-conflict,missing-test"),
        ),
        (
            "report",
            Value::from(reviews.join("t1-r1.md").to_str().unwrap()),
        ),
        (
            "stream",
            Value::from(reviews.join("t1-r1.jsonl").to_str().unwrap()),
        ),
    ] {
        assert_eq!(event[key], expected, "{key} in {event}");
    }
    assert!(event["duration_s"].is_u64(), "{event}");
}

#[test]
fn a_failed_run_logs_its_event_with_outcome_error_and_the_reason() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", "echo 'stream so far'; exit 1");
    let previous = sandbox.path("t1-r1.md");
    fs::write(&previous, "a lacks a test\nVERDICT: findings\n").unwrap();

    let out = sandbox.factory_in(
        &repo,
        &[
            "review",
            "main",
            "t1-r2",
            "none",
            previous.to_str().unwrap(),
        ],
    );

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: codex exited 1; see "),
        "{out:?}"
    );
    let lines = sandbox.ledger_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let event: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["outcome"], "error", "{event}");
    assert_eq!(event["round"], 2, "{event}");
    assert!(
        event["reason"]
            .as_str()
            .unwrap()
            .starts_with("codex exited 1"),
        "{event}"
    );
    assert_eq!(event.get("verdict"), None, "{event}");
    let stream = fs::read_to_string(sandbox.reviews().join("t1-r2.jsonl")).unwrap();
    assert_eq!(stream, "stream so far\n");
}

#[test]
fn the_findings_ledger_is_kept_as_json_beside_the_report_and_json_output_names_it() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1-r1"]);

    assert_eq!(out.code, 10, "{out:?}");
    let reviews = sandbox.reviews();
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(out.stdout.lines().count(), 1, "{out:?}");
    assert_eq!(shown["verdict"], "findings");
    assert_eq!(shown["blocking"], 2);
    assert_eq!(shown["report"], reviews.join("t1-r1.md").to_str().unwrap());
    let ledger_path = reviews.join("t1-r1.findings.json");
    assert_eq!(shown["findings"], ledger_path.to_str().unwrap());
    let ledger: Value = serde_json::from_str(&fs::read_to_string(&ledger_path).unwrap()).unwrap();
    assert_eq!(ledger["label"], "t1-r1");
    assert_eq!(ledger["round"], 1);
    assert_eq!(ledger["backend"], "codex");
    assert_eq!(ledger["verdict"], "findings");
    let findings = ledger["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 4, "{ledger}");
    assert_eq!(
        findings[3],
        serde_json::json!({"severity": "blocking", "axis": "standards", "tag": "missing-test",
            "location": "a.txt:1", "summary": "nothing tests a"})
    );
    let report = fs::read_to_string(reviews.join("t1-r1.md")).unwrap();
    assert!(report.contains("FINDINGS: standards_hard=1"), "{report}");
    assert!(report.ends_with("VERDICT: findings\n"), "{report}");
    for kept in ["bundle.md", "prompt", "jsonl"] {
        assert!(reviews.join(format!("t1-r1.{kept}")).is_file(), "{kept}");
    }
}

#[test]
fn a_report_without_a_ledger_is_read_by_its_findings_and_verdict_lines() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake(
        "codex",
        &codex_writes(
            "Prose only.\n\nFINDINGS: standards_hard=1 standards_judgement=2 spec_findings=1 spec_advisory=0 questions=1 tags=missing-test\nVERDICT: findings\n",
        ),
    );

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 10, "{out:?}");
    let report = sandbox.reviews().join("t1.md");
    assert_eq!(
        out.stdout_lines(),
        [
            format!(
                "review t1: findings (2 blocking, 2 advisory, 1 question) → {}",
                report.display()
            ),
            format!(
                "the report has no findings ledger: read its blocking findings in {}",
                report.display()
            ),
        ]
    );
}

#[test]
fn a_report_with_neither_ledger_nor_verdict_fails_with_exit_3_naming_it() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes("I looked around.\n"));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.starts_with(
            "factory: code-review: the report has no findings ledger and no VERDICT line: "
        ),
        "{out:?}"
    );
}

#[test]
fn a_rerun_under_the_same_label_does_not_read_the_last_runs_report() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    assert_eq!(
        sandbox
            .factory_in(&repo, &["review", "--standalone", "main", "t1"])
            .code,
        0
    );
    sandbox.fake("codex", "exit 0");

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: codex finished without a report ("),
        "{out:?}"
    );
    assert!(!sandbox.reviews().join("t1.findings.json").exists());
}

#[test]
fn a_run_that_fails_before_the_reviewer_starts_logs_no_report_or_stream() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    fs::create_dir(sandbox.path("home/.codex")).unwrap();
    fs::write(
        sandbox.path("home/.codex/config.toml"),
        "model = \"gpt-test\"\n",
    )
    .unwrap();
    fs::create_dir_all(sandbox.path("config/factory")).unwrap();
    fs::write(
        sandbox.path("config/factory/config.toml"),
        "[[review.backend]]\ncli = \"codex\"\n",
    )
    .unwrap();

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "nope", "t1"]);

    assert_eq!(out.code, 2, "{out:?}");
    let lines = sandbox.ledger_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let event: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["outcome"], "error", "{event}");
    assert!(
        event["reason"]
            .as_str()
            .unwrap()
            .starts_with("unknown ref: nope"),
        "{event}"
    );
    for absent in [
        "backend", "pool", "reviewer", "model", "report", "stream", "findings", "verdict",
    ] {
        assert_eq!(event.get(absent), None, "{absent} in {event}");
    }
}

#[test]
fn an_event_that_cannot_be_logged_fails_the_run_in_one_line_naming_the_report() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    // The daemon must start, as it routes the review, but no append to the ledger lands.
    fs::create_dir_all(sandbox.ledger().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/dev/full", sandbox.ledger()).unwrap();

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "", "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    let report = sandbox.reviews().join("t1.md");
    assert!(
        out.stderr.starts_with(&format!(
            "factory: code-review: review t1: clean (0 blocking, 1 advisory, 0 question) → {}; but its event was not logged: ",
            report.display()
        )),
        "{out:?}"
    );
}

/// Reviews `work` as round 1 with a ledger of `report`, then commits `fix` (a file and its
/// text) and returns the round-2 run's output. The fake codex answers round 2 with `second`.
fn fix_round(
    sandbox: &Sandbox,
    repo: &std::path::Path,
    report: &str,
    second: &str,
) -> crate::support::Run {
    sandbox.fake("codex", &codex_writes(report));
    let first = sandbox.factory_in(repo, &["review", "main", "t1-r1"]);
    assert!(first.code == 0 || first.code == 10, "{first:?}");
    fs::write(repo.join("b.txt"), "b\n").unwrap();
    sandbox.git(repo, &["add", "."]);
    sandbox.git(repo, &["commit", "-q", "-m", "fix: add b"]);
    sandbox.fake("codex", &codex_writes(second));
    let previous = sandbox.reviews().join("t1-r1.md");
    sandbox.factory_in(
        repo,
        &[
            "review",
            "main",
            "t1-r2",
            "none",
            previous.to_str().unwrap(),
        ],
    )
}

#[test]
fn a_fix_round_bundles_only_the_diff_since_the_previous_reviews_head() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = fix_round(&sandbox, &repo, FINDINGS_REPORT, FIXED_REPORT);

    assert_eq!(out.code, 0, "{out:?}");
    let round_one = fs::read_to_string(sandbox.reviews().join("t1-r1.bundle.md")).unwrap();
    let round_two = fs::read_to_string(sandbox.reviews().join("t1-r2.bundle.md")).unwrap();
    assert!(round_one.contains("+++ b/a.txt"), "{round_one}");
    assert!(round_two.contains("+++ b/b.txt"), "{round_two}");
    assert!(!round_two.contains("+++ b/a.txt"), "{round_two}");
    assert!(round_two.contains("fix: add b"), "{round_two}");
    assert!(!round_two.contains("feat: add a"), "{round_two}");
}

#[test]
fn a_fix_round_without_a_reviewed_head_reviews_the_whole_diff_and_says_so() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    codex_only(&sandbox);
    let previous = sandbox.path("t1-r1.md");
    fs::write(&previous, "a lacks a test\nVERDICT: findings\n").unwrap();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(
        &repo,
        &[
            "review",
            "main",
            "t1-r2",
            "none",
            previous.to_str().unwrap(),
        ],
    );

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        out.stderr,
        format!(
            "factory: no reviewed head for {} on this branch; round 2 reviews the whole diff\n",
            previous.display()
        )
    );
    let bundle = fs::read_to_string(sandbox.reviews().join("t1-r2.bundle.md")).unwrap();
    assert!(bundle.contains("+++ b/a.txt"), "{bundle}");
    assert!(bundle.contains("a lacks a test"), "{bundle}");
}

#[test]
fn a_fix_round_with_nothing_committed_since_the_previous_head_fails_with_exit_2() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));
    sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);
    let previous = sandbox.reviews().join("t1-r1.md");
    let calls = sandbox.calls("codex").len();

    let out = sandbox.factory_in(
        &repo,
        &[
            "review",
            "main",
            "t1-r2",
            "none",
            previous.to_str().unwrap(),
        ],
    );

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(
            "factory: code-review: no committed changes since the previous review's head "
        ),
        "{out:?}"
    );
    assert_eq!(sandbox.calls("codex").len(), calls);
}

#[test]
fn a_fix_rounds_bundle_carries_the_previous_open_findings_as_structured_input() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = fix_round(&sandbox, &repo, FINDINGS_REPORT, FIXED_REPORT);

    assert_eq!(out.code, 0, "{out:?}");
    let bundle = fs::read_to_string(sandbox.reviews().join("t1-r2.bundle.md")).unwrap();
    let (_, tail) = bundle
        .split_once("## Previous findings")
        .unwrap_or_else(|| panic!("no Previous findings in:\n{bundle}"));
    let json = tail
        .split("```json\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    let carried: Value = serde_json::from_str(json).unwrap();
    let ids: Vec<&str> = carried["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["f1", "f2", "f3", "f4"]);
    assert_eq!(carried["findings"][3]["summary"], "nothing tests a");
    assert_eq!(carried["findings"][3]["severity"], "blocking");
    // The previous report itself stays above them, under its own heading.
    let (before, _) = bundle.split_once("## Previous findings").unwrap();
    assert!(
        before
            .split_once("## Previous review")
            .is_some_and(|(_, report)| report.contains("Two things.")),
        "{bundle}"
    );
}

/// Round 2 of `FINDINGS_REPORT` after a fix that settled all four of its findings.
const FIXED_REPORT: &str = r#"## Findings ledger

```json
{"findings":[
  {"id":"f1","status":"fixed","severity":"blocking","axis":"spec","tag":"partial-requirement","location":"a.txt:1","summary":"a holds no b"},
  {"id":"f2","status":"fixed","severity":"advisory","axis":"standards","tag":"mysterious-name","location":"a.txt:1","summary":"a is a vague name"},
  {"id":"f3","status":"fixed","severity":"question","axis":"spec","tag":"spec-conflict","location":"README.md:1","summary":"which title?"},
  {"id":"f4","status":"fixed","severity":"blocking","axis":"standards","tag":"missing-test","location":"a.txt:1","summary":"nothing tests a"}
]}
```

FINDINGS: standards_hard=0 standards_judgement=0 spec_findings=0 spec_advisory=0 questions=0 fixed=4 still_present=0 tags=
VERDICT: clean
"#;

/// Round 2 of `FINDINGS_REPORT` after a fix that added b.txt: f1 and f3 fixed, f2 and f4 still
/// there, and three new findings, two of them on lines the fix never touched.
const SECOND_REPORT: &str = r#"## Spec

Checked.

## Findings ledger

```json
{"findings":[
  {"id":"f1","status":"fixed","severity":"blocking","axis":"spec","tag":"partial-requirement","location":"a.txt:1","summary":"a holds no b"},
  {"id":"f2","status":"still_present","severity":"advisory","axis":"standards","tag":"mysterious-name","location":"a.txt:1","summary":"a is a vague name"},
  {"id":"f3","status":"fixed","severity":"question","axis":"spec","tag":"spec-conflict","location":"README.md:1","summary":"which title?"},
  {"id":"f4","status":"still_present","severity":"blocking","axis":"standards","tag":"missing-test","location":"a.txt:1","summary":"nothing tests a"},
  {"status":"new","severity":"blocking","axis":"spec","tag":"incorrect-behavior","location":"b.txt:1","summary":"b is wrong"},
  {"status":"new","severity":"blocking","axis":"spec","tag":"incorrect-behavior","location":"a.txt:1","summary":"a is wrong too"},
  {"severity":"advisory","axis":"standards","tag":"mysterious-name","location":"a.txt:5","summary":"a stays vague"}
]}
```

FINDINGS: standards_hard=1 standards_judgement=2 spec_findings=2 spec_advisory=0 questions=0 fixed=2 still_present=2 tags=x
VERDICT: findings
"#;

#[test]
fn a_fix_rounds_ledger_marks_each_old_finding_and_drops_new_ones_off_the_fix() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = fix_round(&sandbox, &repo, FINDINGS_REPORT, SECOND_REPORT);

    assert_eq!(out.code, 10, "{out:?}");
    let report = sandbox.reviews().join("t1-r2.md");
    assert_eq!(
        out.stdout_lines(),
        vec![
            format!(
                "review t1-r2: findings (2 blocking, 1 advisory, 0 question; 2 fixed, 2 still present, 2 dropped) → {}",
                report.display()
            )
            .as_str(),
            "- [standards/missing-test] a.txt:1: nothing tests a",
            "- [spec/incorrect-behavior] b.txt:1: b is wrong",
        ]
    );
    let ledger: Value = serde_json::from_str(
        &fs::read_to_string(sandbox.reviews().join("t1-r2.findings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ledger["fixed"], 2);
    assert_eq!(ledger["still_present"], 2);
    assert_eq!(ledger["dropped"], 2);
    let status: Vec<(&str, &str)> = ledger["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["location"].as_str().unwrap(),
                f["status"].as_str().unwrap_or("new"),
            )
        })
        .collect();
    assert_eq!(
        status,
        [
            ("a.txt:1", "fixed"),
            ("a.txt:1", "still_present"),
            ("README.md:1", "fixed"),
            ("a.txt:1", "still_present"),
            ("b.txt:1", "new"),
        ]
    );
    let dropped: Vec<&str> = ledger["dropped_findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["summary"].as_str().unwrap())
        .collect();
    assert_eq!(dropped, ["a is wrong too", "a stays vague"]);
    // The ledger's head is the one round 3 would start from.
    let head = sandbox.git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(ledger["head"], head.trim());
    let first: Value = serde_json::from_str(
        &fs::read_to_string(sandbox.reviews().join("t1-r1.findings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ledger["from"], first["head"]);
}

#[test]
fn a_fix_rounds_event_carries_what_was_fixed_kept_and_dropped() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    fix_round(&sandbox, &repo, FINDINGS_REPORT, SECOND_REPORT);

    let lines = sandbox.ledger_lines();
    let event: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!(event["label"], "t1-r2");
    assert_eq!(event["round"], 2);
    for (key, expected) in [
        ("fixed", 2),
        ("still_present", 2),
        ("dropped", 2),
        ("blocking", 2),
    ] {
        assert_eq!(event[key], expected, "{key} in {event}");
    }
    let first: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["from"], first["head"], "{event}");
}

#[test]
fn a_fix_round_keeps_an_old_finding_the_reviewer_left_unmarked_as_still_present() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = fix_round(&sandbox, &repo, FINDINGS_REPORT, CLEAN_REPORT);

    // The reviewer marked none of the four old findings: nothing shows them fixed, so they
    // stay open as still present. Its one new finding is off the fix.
    assert_eq!(out.code, 10, "{out:?}");
    assert!(
        out.stdout.starts_with("review t1-r2: findings (2 blocking, 1 advisory, 1 question; 4 still present, 1 dropped) → "),
        "{out:?}"
    );
}

#[test]
fn a_fix_rounds_bundle_is_smaller_than_the_first_rounds() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    fs::write(repo.join("big.txt"), "line\n".repeat(400)).unwrap();
    sandbox.git(&repo, &["add", "."]);
    sandbox.git(&repo, &["commit", "-q", "-m", "feat: add a lot"]);

    fix_round(&sandbox, &repo, FINDINGS_REPORT, CLEAN_REPORT);

    let size = |label: &str| {
        fs::metadata(sandbox.reviews().join(format!("{label}.bundle.md")))
            .unwrap()
            .len()
    };
    assert!(
        size("t1-r2") < size("t1-r1"),
        "{} vs {}",
        size("t1-r2"),
        size("t1-r1")
    );
}

#[test]
fn a_fix_rounds_prompt_asks_for_each_old_finding_marked_and_new_ones_only_on_the_fix() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let first = sandbox.git(&repo, &["rev-parse", "--short=8", "HEAD"]);

    fix_round(&sandbox, &repo, FINDINGS_REPORT, FIXED_REPORT);

    let prompt = fs::read_to_string(sandbox.reviews().join("t1-r2.prompt")).unwrap();
    assert!(
        prompt.starts_with(&format!("$code-review since {}.\n", first.trim())),
        "{prompt}"
    );
    for part in [
        "still_present",
        "id",
        "only on a line the fix touched",
        "hard",
    ] {
        assert!(prompt.contains(part), "{part:?} missing from:\n{prompt}");
    }
    let first_round = fs::read_to_string(sandbox.reviews().join("t1-r1.prompt")).unwrap();
    assert!(!first_round.contains("still_present"), "{first_round}");
}

#[test]
fn a_later_round_without_a_previous_report_continues_from_the_round_before_it() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(FINDINGS_REPORT));
    sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);
    fs::write(repo.join("b.txt"), "b\n").unwrap();
    sandbox.git(&repo, &["add", "."]);
    sandbox.git(&repo, &["commit", "-q", "-m", "fix: add b"]);
    sandbox.fake("codex", &codex_writes(FIXED_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r2", "none"]);

    assert_eq!(out.code, 0, "{out:?}");
    let bundle = fs::read_to_string(sandbox.reviews().join("t1-r2.bundle.md")).unwrap();
    assert!(bundle.contains("+++ b/b.txt"), "{bundle}");
    assert!(!bundle.contains("+++ b/a.txt"), "{bundle}");
    assert!(bundle.contains("## Previous findings"), "{bundle}");
    let ledger: Value = serde_json::from_str(
        &fs::read_to_string(sandbox.reviews().join("t1-r2.findings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ledger["fixed"], 4, "{ledger}");
    assert!(
        ledger["from"].as_str().is_some_and(|from| !from.is_empty()),
        "{ledger}"
    );
}

#[test]
fn a_later_round_whose_round_before_has_no_report_is_refused_naming_it_not_reviewed_fresh() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r2", "none"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "", "{out:?}");
    assert_eq!(
        out.stderr,
        format!(
            "factory: code-review: t1-r2 continues t1-r1, which has no report at {}: run t1-r1 first, or pass its report as the previous report\n",
            sandbox.reviews().join("t1-r1.md").display()
        )
    );
    assert!(sandbox.calls("codex").is_empty(), "{out:?}");
}

/// Runs `labels` in turn, each after a fix commit of its own (`<label>.txt`), the fake codex
/// answering each with `report`, and returns the last run's output.
fn rounds(
    sandbox: &Sandbox,
    repo: &std::path::Path,
    report: &str,
    labels: &[&[&str]],
) -> crate::support::Run {
    sandbox.fake("codex", &codex_writes(report));
    let mut last = None;
    for (at, args) in labels.iter().enumerate() {
        if at > 0 {
            fs::write(repo.join(format!("{}.txt", args[0])), "fix\n").unwrap();
            sandbox.git(repo, &["add", "."]);
            sandbox.git(
                repo,
                &["commit", "-q", "-m", &format!("fix: before {}", args[0])],
            );
        }
        let mut argv = vec!["review", "main", args[0], "none"];
        argv.extend_from_slice(&args[1..]);
        last = Some(sandbox.factory_in(repo, &argv));
    }
    last.unwrap()
}

#[test]
fn round_3_without_escalation_is_refused_in_one_line_that_says_to_report_what_is_open() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = rounds(
        &sandbox,
        &repo,
        FINDINGS_REPORT,
        &[&["t1-r1"], &["t1-r2"], &["t1-r3"]],
    );

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(out.stdout, "", "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: code-review: t1-r3 is past the 2-round review cap: report what is still open as open in your final report (t1-r2-light may review a last fix; rounds 3 and 4 are an escalation's, with --escalation)\n"
    );
    assert_eq!(sandbox.calls("codex").len(), 2, "{out:?}");
    assert!(!sandbox.reviews().join("t1-r3.bundle.md").exists());
}

#[test]
fn an_escalations_round_3_continues_round_2s_ledger_and_round_5_is_refused() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    escalate(&sandbox, &repo, "t1");
    let third = rounds(
        &sandbox,
        &repo,
        FINDINGS_REPORT,
        &[&["t1-r1"], &["t1-r2"], &["t1-r3", "--escalation"]],
    );

    assert_eq!(third.code, 10, "{third:?}");
    let bundle = fs::read_to_string(sandbox.reviews().join("t1-r3.bundle.md")).unwrap();
    assert!(bundle.contains("+++ b/t1-r3.txt"), "{bundle}");
    assert!(!bundle.contains("+++ b/t1-r2.txt"), "{bundle}");
    assert!(bundle.contains("## Previous findings"), "{bundle}");
    assert!(
        bundle.contains(&sandbox.reviews().join("t1-r2.md").display().to_string()),
        "{bundle}"
    );
    let ledger: Value = serde_json::from_str(
        &fs::read_to_string(sandbox.reviews().join("t1-r3.findings.json")).unwrap(),
    )
    .unwrap();
    assert!(ledger.get("still_present").is_some(), "{ledger}");

    let fifth = sandbox.factory_in(&repo, &["review", "main", "t1-r5", "none", "--escalation"]);
    assert_eq!(fifth.code, 2, "{fifth:?}");
    assert_eq!(
        fifth.stderr,
        "factory: code-review: t1-r5 is past the review cap (2 rounds, 4 for an escalation): report what is still open as open in your final report\n"
    );
}

/// Logs the escalation event the lead lands when it re-dispatches `prefix`'s ticket.
fn escalate(sandbox: &Sandbox, repo: &std::path::Path, prefix: &str) {
    let out = sandbox.factory_in(
        repo,
        &[
            "log",
            "escalation",
            "repo=repo",
            &format!("prefix={prefix}"),
        ],
    );
    assert_eq!(out.code, 0, "{out:?}");
}

#[test]
fn an_escalation_with_no_escalation_event_for_its_prefix_is_refused() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    escalate(&sandbox, &repo, "t2");

    let out = rounds(
        &sandbox,
        &repo,
        FINDINGS_REPORT,
        &[&["t1-r1"], &["t1-r2"], &["t1-r3", "--escalation"]],
    );

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: code-review: --escalation needs an escalation event for t1 in the ledger, which the lead logs when it re-dispatches the ticket (factory log escalation repo=repo prefix=t1): without one, report what is still open as open in your final report\n"
    );
    assert_eq!(sandbox.calls("codex").len(), 2, "{out:?}");
    assert!(!sandbox.reviews().join("t1-r3.bundle.md").exists());
}

#[test]
fn a_label_with_no_round_is_refused_unless_the_review_is_standalone() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    for label in ["retro-lab-light2", "t434-r5-light-2", "t1-r02-light"] {
        let out = sandbox.factory_in(&repo, &["review", "main", label]);
        assert_eq!(out.code, 2, "{out:?}");
        assert_eq!(
            out.stderr,
            format!(
                "factory: code-review: {label} names no review round: label a ticket's review <prefix>-rN (<prefix>-rN-light after the last round), or pass --standalone for a review outside a ticket, such as a retro's\n"
            )
        );
    }
    assert!(sandbox.calls("codex").is_empty());

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "retro-lab"]);
    assert_eq!(out.code, 0, "{out:?}");
}

#[test]
fn a_new_round_1_on_work_a_prefix_reviewed_to_its_cap_needs_a_follow_up_issue() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    rounds(&sandbox, &repo, FINDINGS_REPORT, &[&["t1-r1"], &["t1-r2"]]);
    let refused = |label: &str, out: &crate::support::Run| {
        assert_eq!(out.code, 2, "{out:?}");
        assert_eq!(out.stdout, "", "{out:?}");
        assert!(
            out.stderr
                .starts_with(&format!("factory: code-review: {label}: ")),
            "{out:?}"
        );
        assert!(
            out.stderr.ends_with(
                ", and t1 has used its review cap: file a follow-up issue for what is still open and review its work with --follow-up #<issue>\n"
            ),
            "{out:?}"
        );
    };

    let same_head = sandbox.factory_in(&repo, &["review", "main", "t1fix-r1", "none"]);
    refused("t1fix-r1", &same_head);
    let retro = sandbox.factory_in(&repo, &["review", "--standalone", "main", "retro"]);
    refused("retro", &retro);
    // A commit that changes nothing is the same work.
    sandbox.git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "chore: nothing"],
    );
    let empty_commit = sandbox.factory_in(&repo, &["review", "main", "t1again-r1", "none"]);
    refused("t1again-r1", &empty_commit);
    // So is the same base under another name: the commit main names.
    let main = sandbox.git(&repo, &["rev-parse", "main"]);
    let base_sha = sandbox.factory_in(&repo, &["review", main.trim(), "t1sha-r1", "none"]);
    refused("t1sha-r1", &base_sha);
    assert_eq!(sandbox.calls("codex").len(), 2);

    sandbox.fake(
        "gh",
        "printf '# #7 Follow up t1\\n\\nWhat t1 left open.\\n'",
    );
    let follow_up = sandbox.factory_in(
        &repo,
        &["review", "main", "t1fix-r1", "none", "--follow-up", "#7"],
    );
    assert_eq!(follow_up.code, 10, "{follow_up:?}");
    let waited = sandbox.factory_in(&repo, &["wait", "review", "t1fix-r1", "--json"]);
    let event: Value = serde_json::from_str(&waited.stdout).unwrap();
    assert_eq!(event["follow_up"], "7", "{event}");
}

#[test]
fn a_follow_up_gh_cannot_fetch_or_that_is_the_spec_is_refused() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    sandbox.fake(
        "gh",
        "echo 'GraphQL: Could not resolve to an issue' >&2; exit 1",
    );

    let missing = sandbox.factory_in(
        &repo,
        &["review", "main", "t1-r1", "none", "--follow-up", "#99"],
    );
    assert_eq!(missing.code, 2, "{missing:?}");
    assert!(
        missing.stderr.starts_with(
            "factory: code-review: could not fetch follow-up issue #99 with gh: GraphQL: Could not resolve"
        ),
        "{missing:?}"
    );
    // Issue 12 of this repo, under two spellings, is the same issue.
    sandbox.fake("gh", "printf '# #12 Add a\\n\\nAdd a.txt.\\n'");
    let spec = sandbox.factory_in(
        &repo,
        &["review", "main", "t1-r1", "#12", "--follow-up", "o/r#12"],
    );
    assert_eq!(spec.code, 2, "{spec:?}");
    assert_eq!(
        spec.stderr,
        "factory: code-review: --follow-up names the new issue filed for what is still open, not the ticket under review (o/r#12)\n"
    );
    let junk = sandbox.factory_in(
        &repo,
        &["review", "main", "t1-r1", "none", "--follow-up", "soon"],
    );
    assert_eq!(junk.code, 2, "{junk:?}");
    assert_eq!(
        junk.stderr,
        "factory: code-review: --follow-up names an issue (#12, 12, owner/repo#12 or an issue URL), got: soon\n"
    );
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn escalation_is_refused_on_rounds_1_and_2() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1", "none", "--escalation"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: code-review: --escalation is for rounds 3 and 4, not t1-r1\n"
    );
}

#[test]
fn the_light_review_after_round_2_reviews_only_the_last_fix_and_runs_once() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let light = rounds(
        &sandbox,
        &repo,
        FINDINGS_REPORT,
        &[&["t1-r1"], &["t1-r2"], &["t1-r2-light"]],
    );

    assert_eq!(light.code, 10, "{light:?}");
    let bundle = fs::read_to_string(sandbox.reviews().join("t1-r2-light.bundle.md")).unwrap();
    assert!(bundle.contains("+++ b/t1-r2-light.txt"), "{bundle}");
    assert!(!bundle.contains("+++ b/t1-r2.txt"), "{bundle}");
    assert!(bundle.contains("## Previous findings"), "{bundle}");
    let event: Value = serde_json::from_str(sandbox.ledger_lines().last().unwrap()).unwrap();
    assert_eq!(event["label"], "t1-r2-light", "{event}");
    assert_eq!(event["round"], 3, "{event}");

    let again = sandbox.factory_in(&repo, &["review", "main", "t1-r2-light", "none"]);
    assert_eq!(again.code, 2, "{again:?}");
    assert_eq!(
        again.stderr,
        "factory: code-review: t1-r2-light has run, and the light review is the last: report what is still open as open in your final report\n"
    );
    assert_eq!(sandbox.calls("codex").len(), 3, "{again:?}");
}

#[test]
fn a_light_review_after_any_round_but_the_last_is_refused() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1-light", "none"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: code-review: t1-r1-light: a light review follows the last round, t1-r2 or an escalation's t1-r4\n"
    );
}

#[test]
fn a_round_on_a_new_branch_reviews_the_whole_diff_but_still_marks_the_round_befores_findings() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    rounds(&sandbox, &repo, FINDINGS_REPORT, &[&["t1-r1"], &["t1-r2"]]);
    // The escalation starts over on a branch of its own: round 2's head is not on it.
    sandbox.git(&repo, &["checkout", "-q", "-b", "again", "main"]);
    fs::write(repo.join("c.txt"), "c\n").unwrap();
    sandbox.git(&repo, &["add", "."]);
    sandbox.git(&repo, &["commit", "-q", "-m", "feat: add c"]);
    sandbox.fake("codex", &codex_writes(SECOND_REPORT));
    escalate(&sandbox, &repo, "t1");

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r3", "none", "--escalation"]);

    assert_eq!(out.code, 10, "{out:?}");
    assert!(
        out.stderr.contains("round 3 reviews the whole diff"),
        "{out:?}"
    );
    let bundle = fs::read_to_string(sandbox.reviews().join("t1-r3.bundle.md")).unwrap();
    assert!(bundle.contains("+++ b/c.txt"), "{bundle}");
    assert!(bundle.contains("## Previous findings"), "{bundle}");
    let ledger: Value = serde_json::from_str(
        &fs::read_to_string(sandbox.reviews().join("t1-r3.findings.json")).unwrap(),
    )
    .unwrap();
    // Round 2's five open findings: SECOND_REPORT fixes f1 and f3, keeps f2 and f4, and leaves
    // f5 unmarked, which stays open.
    assert_eq!(ledger["fixed"], 2, "{ledger}");
    assert_eq!(ledger["still_present"], 3, "{ledger}");
}
