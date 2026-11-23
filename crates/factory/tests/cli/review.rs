//! `factory review <base> <label> [spec] [prev]`: a Codex review over a prepared bundle.

use std::fs;

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
    let template = r#"# #{{.number}} {{.title}}{{"\n\n"}}{{.body}}{{"\n"}}"#;
    let expected = [
        "issue",
        "view",
        "12",
        "--json",
        "number,title,body",
        "--template",
        template,
    ];
    assert_eq!(sandbox.calls("gh"), vec![expected; 3]);
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
