//! `factory review <base> <label> [spec] [prev]`: a Codex review over a prepared bundle.

use std::fs;

use serde_json::Value;

use crate::support::Sandbox;

const CLEAN_REPORT: &str = r#"## Standards

Nothing hard.

## Findings ledger

```json
{"findings":[{"severity":"advisory","axis":"standards","tag":"mysterious-name","location":"a.txt:1","summary":"a is a vague name"}]}
```

FINDINGS: standards_hard=0 standards_judgement=1 spec_findings=0 spec_advisory=0 questions=0 fixed=0 still_present=0 tags=mysterious-name
VERDICT: clean
"#;

const FINDINGS_REPORT: &str = r#"## Spec

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
fn codex_writes(report: &str) -> String {
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
fn blocking_findings_follow_the_line_and_exit_10() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
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
        ("work", "factory: code-review: no committed changes since work"),
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

    for (n, spec) in ["#12", "12", "https://github.com/o/r/issues/12"]
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
    // A URL goes to gh whole, so an issue of another repo is fetched from that repo.
    let template = r#"# #{{.number}} {{.title}}{{"\n\n"}}{{.body}}{{"\n"}}"#;
    let expected: Vec<Vec<&str>> = ["12", "12", "https://github.com/o/r/issues/12"]
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
        out.stderr
            .starts_with("factory: code-review: could not fetch spec #99 with gh: GraphQL: Could not resolve"),
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
        out.stderr.starts_with("factory: code-review: codex timed out after 1s"),
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
        ("reviewer", Value::from("codex")),
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

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r2", "none"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: code-review: codex exited 1; see "),
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

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

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

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: the report has no findings ledger and no VERDICT line: "),
        "{out:?}"
    );
}

#[test]
fn a_rerun_under_the_same_label_does_not_read_the_last_runs_report() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    assert_eq!(sandbox.factory_in(&repo, &["review", "main", "t1"]).code, 0);
    sandbox.fake("codex", "exit 0");

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

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

    let out = sandbox.factory_in(&repo, &["review", "nope", "t1"]);

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
    assert_eq!(event["backend"], "codex", "{event}");
    assert_eq!(event["model"], "gpt-test", "{event}");
    for absent in ["report", "stream", "findings", "verdict"] {
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

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

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
