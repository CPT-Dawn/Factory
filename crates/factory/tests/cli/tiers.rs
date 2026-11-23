//! review.sh's acceptance list (its test.sh, as of review-diet), at the binary seam: tiers and
//! their chains of pool, model and effort; the Gemini reserve; agy's usage gating per pool; a
//! quota hit that moves to the next model or the next pool; two slots per pool; the bundle,
//! the one-session prompt and round 2 on the fix with its ledger.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::support::{Run, Sandbox, finish, spawn, world};

/// The report every fake reviewer writes: review.sh's test report, which has the FINDINGS and
/// VERDICT lines and no JSON ledger.
const REPORT: &str = "## Standards\nnone\n## Spec\nnone\nFINDINGS: standards_hard=0 standards_judgement=0 spec_findings=0 spec_advisory=0 questions=0 fixed=0 still_present=0 tags=\nVERDICT: clean\n";

/// The reviewer skill, as the one-session prompt carries it.
const SKILL: &str = "---\nname: code-review\ndescription: the reviewer\n---\n# Code review\n\n## Severity and tags\n\nBlocking, advisory, question.\n";

/// What agy logs at startup when its sandbox is in effect.
const SANDBOX_STARTED: &str =
    "I1123 09:37:00.811680 1 session.go:87] Print mode: enabling terminal sandbox for this session";

/// A sandbox with fake agy, codex and claude that play the lines of `plan` in turn, one per
/// review run: `ok` writes the report, `quota` is agy's exit 3 quota error, `limit` Codex's
/// usage limit, `hold` (agy) waits for the file `release` before writing the report, and fails
/// (exit 9) a minute on without it, so a failed test leaves no agy behind. Each
/// run appends a line to `runs`: `agy <model>`, or `codex`/`claude` and its arguments.
struct Lab {
    sandbox: Sandbox,
    repo: PathBuf,
}

impl Lab {
    /// A lab whose repo's branch `work` holds `change` over the branch `base`, whose agy
    /// usage shows the Gemini 5-hour bucket, the Argon weekly bucket and the Claude 5-hour
    /// bucket with these fractions left, and whose reviewers play `plan`.
    fn new(change: &str, gemini: f64, argon: f64, claude: f64, plan: &[&str]) -> Lab {
        let sandbox = Sandbox::new().env("TZ", "UTC");
        let repo = sandbox.path("repo");
        must(fs::create_dir(&repo), "create the repo");
        let git = |args: &[&str]| sandbox.git(&repo, args);
        git(&["init", "-q", "-b", "base"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&["checkout", "-q", "-b", "work"]);
        let lines = |n: usize| (1..=n).map(|i| format!("{i}\n")).collect::<String>();
        let (path, text) = match change {
            "code" => ("src/a.ts", lines(40)),
            "docs" => ("notes.md", lines(100)),
            "tiny" => ("src.ts", lines(5)),
            "small" => ("src.ts", lines(6)),
            "tests" => ("test/a.test.ts", lines(3)),
            other => panic!("no change {other}"),
        };
        let file = repo.join(path);
        must(
            fs::create_dir_all(file.parent().unwrap_or(&repo)),
            "create the change's dir",
        );
        must(fs::write(&file, text), "write the change");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "feat: work"]);
        let skill = sandbox.path("home/.agents/skills/code-review/SKILL.md");
        let skills = sandbox.path("home/.agents/skills/code-review");
        must(fs::create_dir_all(&skills), "create the skill's dir");
        must(fs::write(&skill, SKILL), "write the skill");
        let lab = Lab { sandbox, repo };
        lab.usage(gemini, argon, claude);
        lab.plan(plan);
        lab.fakes();
        lab
    }

    fn file(&self, name: &str) -> PathBuf {
        self.sandbox.path(name)
    }

    /// agy's usage, as in [`Lab::new`]. Every bucket resets far off, so an empty one holds.
    fn usage(&self, gemini: f64, argon: f64, claude: f64) {
        let bucket = |window: &str, left: f64| json!({"window": window, "remaining_fraction": left, "reset_time": "2099-01-01T00:00:00Z"});
        let usage = json!({"command": {"data": {"groups": [
            {"name": "Gemini Models", "buckets": [bucket("weekly", 0.8), bucket("5h", gemini)]},
            {"name": "Claude and GPT models", "buckets": [bucket("5h", claude)]},
            {"name": "Gemini 4 Argon", "buckets": [bucket("weekly", argon)]},
        ]}}});
        must(
            fs::write(self.file("usage.json"), usage.to_string()),
            "write the usage",
        );
    }

    fn plan(&self, plan: &[&str]) {
        let text: String = plan.iter().map(|line| format!("{line}\n")).collect();
        must(fs::write(self.file("plan"), text), "write the plan");
        let _ = fs::remove_file(self.file("runs"));
    }

    fn fakes(&self) {
        let (plan, runs, release) = (self.file("plan"), self.file("runs"), self.file("release"));
        let (plan, runs, release) = (plan.display(), runs.display(), release.display());
        let pop = format!("step=$(head -n1 '{plan}'); sed -i 1d '{plan}'");
        let agy_result = json!({"status": "SUCCESS", "response": REPORT, "usage": {"input_tokens": 100, "cache_read_tokens": 50, "output_tokens": 10}});
        let agy_quota = json!({"status": "ERROR", "response": "", "error": "Individual quota reached. Resets in 2h0m0s"});
        self.sandbox.fake(
            "agy",
            &format!(
                "if [ \"$2\" = /usage ]; then cat '{usage}'; exit 0; fi\n\
                 printf '%s' \"$2\" > \"$0.prompt\"\n\
                 model=; log=; prev=; for a in \"$@\"; do [ \"$prev\" = --model ] && model=$a; [ \"$prev\" = --log-file ] && log=$a; prev=$a; done\n\
                 echo \"agy $model\" >> '{runs}'\n\
                 [ -n \"$log\" ] && echo '{SANDBOX_STARTED}' >> \"$log\"\n\
                 {pop}\n\
                 case $step in\n\
                 hold) timeout 60 sh -c 'until [ -e \"$1\" ]; do sleep 0.05; done' hold '{release}' || exit 9; cat <<'R'\n{agy_result}\nR\n;;\n\
                 ok) cat <<'R'\n{agy_result}\nR\n;;\n\
                 quota) cat <<'R'\n{agy_quota}\nR\n\
                 echo 'AGY_ERROR: Individual quota reached. Resets in 2h0m0s' >&2; exit 3;;\n\
                 *) exit 9;;\n\
                 esac",
                usage = self.file("usage.json").display(),
            ),
        );
        let limit = json!({"type": "turn.failed", "error": {"message": "You have hit your usage limit. Upgrade or try again at 11:59 PM."}});
        self.sandbox.fake(
            "codex",
            &format!(
                "echo \"codex $*\" >> '{runs}'\n\
                 out=; prev=; for a in \"$@\"; do [ \"$prev\" = -o ] && out=$a; prev=$a; done\n\
                 cat > \"$0.stdin\"\n\
                 {pop}\n\
                 case $step in\n\
                 ok) cat > \"$out\" <<'R'\n{REPORT}R\n\
                 echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1000,\"cached_input_tokens\":900,\"output_tokens\":20}}}}';;\n\
                 limit) echo '{limit}'; exit 1;;\n\
                 *) exit 9;;\n\
                 esac"
            ),
        );
        let claude_result = json!({"result": REPORT, "usage": {"input_tokens": 5, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 5, "output_tokens": 5}});
        self.sandbox.fake(
            "claude",
            &format!(
                "echo \"claude $*\" >> '{runs}'\ncat > \"$0.stdin\"\n{pop}\ncat <<'R'\n{claude_result}\nR"
            ),
        );
        self.sandbox.fake("unshare", "exit 0");
    }

    fn review(&self, args: &[&str]) -> Run {
        let mut all = vec!["review"];
        all.extend_from_slice(args);
        self.sandbox.factory_in(&self.repo, &all)
    }

    /// The runs since the plan was last set, one per line.
    fn runs(&self) -> Vec<String> {
        fs::read_to_string(self.file("runs"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The model of each agy run, `codex` or `claude` for the others.
    fn who(&self) -> Vec<String> {
        self.runs()
            .iter()
            .map(|run| match run.strip_prefix("agy ") {
                Some(model) => format!("agy {model}"),
                None => run.split(' ').next().unwrap_or_default().to_owned(),
            })
            .collect()
    }

    fn last_event(&self) -> Value {
        let lines = self.sandbox.ledger_lines();
        let last = lines.last().map_or("", String::as_str);
        must(serde_json::from_str(last), "read the last event")
    }

    fn reviews(&self) -> PathBuf {
        self.sandbox.reviews()
    }

    fn git(&self, args: &[&str]) -> String {
        self.sandbox.git(&self.repo, args)
    }
}

/// The value of `result`, failing the test with `what` when there is none.
fn must<T, E: std::fmt::Display>(result: Result<T, E>, what: &str) -> T {
    result.unwrap_or_else(|e| panic!("{what}: {e}"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn hard_runs_gemini_4_neon_high_first() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who(), ["agy gemini-4-neon-high"]);
}

#[test]
fn the_event_names_tier_pool_reviewer_model_effort_and_tokens() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let event = lab.last_event();
    for (field, want) in [
        ("tier", json!("hard")),
        ("tier_source", json!("caller")),
        ("pool", json!("agy-gemini")),
        ("reviewer", json!("agy")),
        ("model", json!("gemini-4-neon-high")),
        ("effort", json!("high")),
        ("tokens", json!(160)),
        ("branch", json!("work")),
        ("files", json!("src/a.ts")),
    ] {
        assert_eq!(event[field], want, "{field}: {event}");
    }
    assert!(event["bundle_bytes"].as_u64().unwrap_or(0) > 0, "{event}");
}

#[test]
fn the_ledger_keeps_the_reviewed_head() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let ledger: Value =
        serde_json::from_str(&read(&lab.reviews().join("t1-r1.ledger.json"))).unwrap();
    assert_eq!(ledger["head"], lab.git(&["rev-parse", "HEAD"]).trim());
    assert_eq!(ledger["pool"], "agy-gemini");
    assert_eq!(ledger["verdict"], "clean");
}

#[test]
fn the_bundle_holds_commits_stat_diff_spec_touched_docs_standards_and_evidence() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let bundle = read(&lab.reviews().join("t1-r1.bundle.md"));
    for section in [
        "Commits",
        "Diff stat",
        "Diff",
        "Spec",
        "Touched docs",
        "Coding standards",
        "Evidence",
    ] {
        assert!(
            bundle.lines().any(|line| line == format!("## {section}")),
            "{section}: {bundle}"
        );
    }
    assert!(bundle.lines().any(|line| line == "+40"), "{bundle}");
    assert!(
        bundle.contains("test@example.invalid 10 feat: work"),
        "{bundle}"
    );
}

#[test]
fn the_prompt_carries_the_skill_and_the_bundle() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let prompt = read(&lab.file("bin/agy.prompt"));
    let bundle = lab.reviews().join("t1-r1.bundle.md");
    assert!(prompt.contains("## Severity and tags"), "{prompt}");
    assert!(
        !prompt.contains("name: code-review"),
        "the front matter is left out: {prompt}"
    );
    assert!(
        prompt.contains(&format!("Review the bundle at {}", bundle.display())),
        "{prompt}"
    );
}

#[test]
fn a_touched_doc_travels_whole() {
    let lab = Lab::new("docs", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["base", "t1b-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let bundle = read(&lab.reviews().join("t1b-r1.bundle.md"));
    assert!(
        bundle.lines().any(|line| line == "### notes.md"),
        "{bundle}"
    );
}

#[test]
fn standard_skips_gemini_under_the_20_percent_reserve_and_takes_codex_medium() {
    let lab = Lab::new("code", 0.1, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["base", "t2-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let runs = lab.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        runs[0].contains("-m gpt-6.2-sol -c model_reasoning_effort=\"medium\""),
        "{runs:?}"
    );
    let event = lab.last_event();
    assert_eq!(
        (event["tier"].as_str(), event["tier_source"].as_str()),
        (Some("standard"), Some("default")),
        "{event}"
    );
}

#[test]
fn standard_with_codex_at_its_limit_takes_agys_opus_high() {
    let lab = Lab::new("code", 0.1, 1.0, 1.0, &["limit", "ok"]);

    let out = lab.review(&["base", "t2b-r1", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who(), ["codex", "agy claude-opus-5-5-high"]);
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(shown["cooled"][0]["backend"], "codex", "{shown}");
}

#[test]
fn a_spec_review_is_hard_and_may_spend_the_reserve() {
    let lab = Lab::new("code", 0.1, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["base", "spec-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who(), ["agy gemini-4-neon-high"]);
    assert_eq!(lab.last_event()["tier_source"], "spec");
}

#[test]
fn an_agy_quota_hit_cools_its_pool_and_the_next_review_skips_it() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["limit", "quota", "ok"]);

    let out = lab.review(&["--tier", "standard", "base", "t3-r1", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        lab.who(),
        [
            "codex",
            "agy gemini-4-neon-medium",
            "agy claude-opus-5-5-high"
        ]
    );
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    let cooled: Vec<&str> = shown["cooled"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["backend"].as_str())
        .collect();
    assert_eq!(cooled, ["codex", "agy-gemini"], "{shown}");

    lab.plan(&["ok"]);
    let next = lab.review(&["base", "t3b-r1"]);

    assert_eq!(next.code, 0, "{next:?}");
    assert_eq!(
        lab.who(),
        ["agy claude-opus-5-5-high"],
        "the cooled pools are skipped"
    );
}

#[test]
fn a_quota_hit_tries_the_chains_next_model_in_the_same_pool_before_cooling_the_pool() {
    let lab = Lab::new("docs", 0.9, 1.0, 1.0, &["quota", "ok"]);

    let out = lab.review(&["base", "t3c-r1", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        lab.who(),
        ["agy gemini-3.8-flash-high", "agy gemini-4-neon-low"]
    );
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(shown["cooled"], json!([]), "{shown}");
}

#[test]
fn the_pools_last_model_hitting_its_quota_cools_the_pool() {
    let lab = Lab::new("docs", 0.9, 1.0, 1.0, &["quota", "quota", "ok"]);

    let out = lab.review(&["base", "t3d-r1", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        lab.who(),
        [
            "agy gemini-3.8-flash-high",
            "agy gemini-4-neon-low",
            "codex"
        ]
    );
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(shown["cooled"][0]["backend"], "agy-gemini", "{shown}");
}

#[test]
fn a_spec_path_that_is_no_file_still_reviews() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t4-r1", "x"]);

    assert_eq!(out.code, 0, "{out:?}");
    let bundle = read(&lab.reviews().join("t4-r1.bundle.md"));
    assert!(bundle.contains("## Spec\n\nnone"), "{bundle}");
}

#[test]
fn an_empty_bucket_skips_its_pool() {
    let lab = Lab::new("code", 0.0, 0.0, 1.0, &["ok"]);

    let out = lab.review(&["--tier", "hard", "base", "t4-r1", "--json"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who(), ["agy claude-opus-5-5-high"]);
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    let cooled: Vec<&str> = shown["cooled"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["backend"].as_str())
        .collect();
    assert_eq!(cooled, ["agy-gemini", "agy-argon"], "{shown}");
}

#[test]
fn a_docs_only_diff_is_trivial_and_under_the_reserve_goes_to_codex_at_low() {
    let lab = Lab::new("docs", 0.1, 0.0, 0.0, &["ok"]);

    let out = lab.review(&["base", "t5-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let runs = lab.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        runs[0].contains("-m gpt-6.2-sol -c model_reasoning_effort=\"low\""),
        "{runs:?}"
    );
    let event = lab.last_event();
    assert_eq!(
        (event["tier"].as_str(), event["tier_source"].as_str()),
        (Some("trivial"), Some("detected")),
        "{event}"
    );
    assert_eq!(event["tokens"], 1020, "Codex's tokens are counted: {event}");
}

#[test]
fn codex_runs_without_web_search_or_sub_agents() {
    let lab = Lab::new("docs", 0.1, 0.0, 0.0, &["ok"]);

    let out = lab.review(&["base", "t5-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let runs = lab.runs();
    assert!(runs[0].contains("-c web_search=\"disabled\""), "{runs:?}");
    assert!(runs[0].contains("--disable multi_agent"), "{runs:?}");
}

#[test]
fn a_5_line_diff_is_trivial_flash_high() {
    let lab = Lab::new("tiny", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["base", "t6-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who(), ["agy gemini-3.8-flash-high"]);
}

#[test]
fn a_6_line_diff_or_a_tests_only_diff_is_standard() {
    for change in ["small", "tests"] {
        let lab = Lab::new(change, 0.9, 1.0, 1.0, &["ok"]);

        let out = lab.review(&["base", "t6b-r1"]);

        assert_eq!(out.code, 0, "{change}: {out:?}");
        assert_eq!(lab.who(), ["codex"], "{change}");
        assert_eq!(lab.last_event()["tier"], "standard", "{change}");
    }
}

#[test]
fn the_callers_hard_tag_wins_over_detection_even_given_last() {
    let lab = Lab::new("tiny", 0.9, 1.0, 1.0, &["ok"]);

    let out = lab.review(&["base", "t7-r1", "none", "--tier", "hard"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who(), ["agy gemini-4-neon-high"]);
}

#[test]
fn codex_at_its_limit_hands_a_standard_review_to_claude_sonnet_high() {
    let lab = Lab::new("code", 0.0, 0.0, 0.0, &["limit", "ok"]);

    let out = lab.review(&["base", "t8-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let runs = lab.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(
        runs[1].starts_with("claude -p --model sonnet --effort high"),
        "{runs:?}"
    );
    assert_eq!(lab.last_event()["pool"], "claude");
}

#[test]
fn a_pool_with_both_slots_taken_passes_to_the_next() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["hold", "hold", "ok", "ok", "ok"]);
    let holding = |label: &str| {
        let mut cmd = lab
            .sandbox
            .factory_command(&["review", "--tier", "hard", "base", label]);
        cmd.current_dir(&lab.repo);
        spawn(cmd)
    };
    let first = holding("t9a-r1");
    assert!(
        world::poll_until(|| lab.runs().len() == 1),
        "the first review started"
    );
    let second = holding("t9b-r1");
    assert!(
        world::poll_until(|| lab.runs().len() == 2),
        "the second review started"
    );

    let out = lab.review(&["--tier", "hard", "base", "t9-r1"]);
    fs::write(lab.file("release"), "").unwrap();

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.who()[2], "agy gemini-4-argon-high", "{:?}", lab.who());
    assert_eq!(finish(first).code, 0);
    assert_eq!(finish(second).code, 0);
}

#[test]
fn round_2_reviews_only_the_fix_with_the_previous_report_and_logs_where_it_started() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok", "ok", "ok"]);
    let first = lab.review(&["base", "t10-r1"]);
    assert_eq!(first.code, 0, "{first:?}");
    let r1_head = lab.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(lab.repo.join("src/b.ts"), "fix\n").unwrap();
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "-q", "-m", "fix: the finding"]);
    let previous = lab.reviews().join("t10-r1.md");

    let out = lab.review(&["base", "t10-r2", "none", previous.to_str().unwrap()]);

    assert_eq!(out.code, 0, "{out:?}");
    let bundle = read(&lab.reviews().join("t10-r2.bundle.md"));
    assert!(bundle.lines().any(|line| line == "+fix"), "{bundle}");
    assert!(!bundle.lines().any(|line| line == "+40"), "{bundle}");
    assert!(
        bundle.contains("fix: the finding") && !bundle.contains("feat: work"),
        "{bundle}"
    );
    assert!(
        bundle.lines().any(|line| line == "## Previous review"),
        "{bundle}"
    );
    assert!(
        bundle
            .lines()
            .any(|line| line.starts_with("FINDINGS: standards_hard=0")),
        "{bundle}"
    );
    let event = lab.last_event();
    assert_eq!(
        (event["round"].as_u64(), event["from"].as_str()),
        (Some(2), Some(&r1_head[..8])),
        "{event}"
    );

    let again = lab.review(&[
        "base",
        "t10-r3",
        "none",
        lab.reviews().join("t10-r2.md").to_str().unwrap(),
    ]);
    assert_eq!(again.code, 2, "{again:?}");
    assert!(again.stderr.contains("commit the fix first"), "{again:?}");
}

#[test]
fn round_2_finds_the_head_in_a_review_sh_ledger() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);
    let r1_head = lab.git(&["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(lab.repo.join("src/b.ts"), "fix\n").unwrap();
    lab.git(&["add", "-A"]);
    lab.git(&["commit", "-q", "-m", "fix: the finding"]);
    let dir = lab.reviews();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("old-r1.md"), REPORT).unwrap();
    fs::write(
        dir.join("old-r1.ledger.json"),
        json!({"label": "old-r1", "head": r1_head}).to_string(),
    )
    .unwrap();

    let out = lab.review(&[
        "base",
        "old-r2",
        "none",
        dir.join("old-r1.md").to_str().unwrap(),
    ]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(lab.last_event()["from"], &r1_head[..8]);
}

#[test]
fn a_bad_label_an_unknown_tier_or_a_missing_previous_report_exits_2() {
    let lab = Lab::new("code", 0.9, 1.0, 1.0, &["ok"]);

    let label = lab.review(&["base", "bad label"]);
    let tier = lab.review(&["--tier", "easy", "base", "t11-r1"]);
    let previous = lab.review(&["base", "t11-r1", "none", "/nope.md"]);

    assert_eq!(label.code, 2, "{label:?}");
    assert_eq!(tier.code, 2, "{tier:?}");
    assert!(tier.stderr.contains("tier must be"), "{tier:?}");
    assert_eq!(previous.code, 2, "{previous:?}");
    assert!(lab.runs().is_empty());
}
