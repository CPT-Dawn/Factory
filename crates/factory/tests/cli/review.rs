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
        ("nope", "factory: unknown ref: nope"),
        ("work", "factory: no committed changes since work"),
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
        "factory: label may only use letters, digits, _ and -, got: \"t1/r1\"\n"
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
            .starts_with("factory: could not fetch spec #99 with gh: GraphQL: Could not resolve"),
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
        out.stderr.starts_with("factory: codex timed out after 1s"),
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
        out.stderr.starts_with("factory: codex exited 1; see "),
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
            .starts_with("factory: the report has no findings ledger and no VERDICT line: "),
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
            .starts_with("factory: codex finished without a report ("),
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
            "factory: review t1: clean (0 blocking, 1 advisory, 0 question) → {}; but its event was not logged: ",
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
        out.stderr
            .starts_with("factory: no committed changes since the previous review's head "),
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
    // The report's prose is not repeated: the findings are the input.
    assert!(!bundle.contains("Two things."), "{bundle}");
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
