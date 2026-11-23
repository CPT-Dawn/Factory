//! Antigravity (`agy`) as the first review backend: its free usage query before launch, its
//! read-only and offline invocation, its sandbox check, its quota error and its auth modes.

use std::fs;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};

use crate::support::Sandbox;

const CLEAN_REPORT: &str = r#"## Standards

Nothing.

## Findings ledger

```json
{"findings":[]}
```

FINDINGS: standards_hard=0 standards_judgement=0 spec_findings=0 spec_advisory=0 questions=0 fixed=0 still_present=0 tags=
VERDICT: clean
"#;

const DENIED: [&str; 4] = ["write_file(*)", "read_url(*)", "execute_url(*)", "mcp(*)"];

fn now_millis() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    i64::try_from(since).unwrap_or(i64::MAX)
}

/// A whole second about two hours from now.
fn in_two_hours() -> Timestamp {
    Timestamp::from_unix_millis((now_millis() / 1000 + 7200) * 1000)
}

/// agy's usage as its free query prints it: the Gemini group's five-hour bucket has
/// `gemini_5h` of its quota left and resets at `reset`; every other bucket is full.
fn usage(gemini_5h: f64, reset: Timestamp) -> String {
    let bucket = |id: &str, left: f64, reset: &str| json!({"id": id, "name": id, "window": id, "remaining_fraction": left, "reset_time": reset});
    let later = "2099-01-01T00:00:00Z";
    json!({
        "conversation_id": "",
        "status": "SUCCESS",
        "response": "Gemini Models\tFive Hour Limit Remaining\n",
        "command": {"name": "usage", "data": {"groups": [
            {"name": "Gemini Models", "buckets": [
                bucket("gemini-weekly", 0.75, later),
                bucket("gemini-5h", gemini_5h, &reset.to_string()),
            ]},
            {"name": "Claude and GPT models", "buckets": [
                bucket("3p-weekly", 1.0, later),
                bucket("3p-5h", 1.0, later),
            ]},
        ]}},
    })
    .to_string()
}

fn usage_left() -> String {
    usage(0.14, in_two_hours())
}

/// A fake `agy`: `agy -p /usage` prints `usage`; any other run keeps its prompt (the
/// value of `-p`) as `bin/agy.prompt` and runs `review`.
fn agy(usage: &str, review: &str) -> String {
    format!(
        "if [ \"$2\" = /usage ]; then\ncat <<'USAGE'\n{usage}\nUSAGE\nexit 0\nfi\n\
         printf '%s' \"$2\" > \"$0.prompt\"\n{review}"
    )
}

/// A review that prints `report` as agy's JSON result.
fn agy_writes(report: &str) -> String {
    let result = json!({
        "conversation_id": "c1",
        "status": "SUCCESS",
        "response": report,
        "duration_seconds": 1.5,
        "num_turns": 1,
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    format!("cat <<'RESULT'\n{result}\nRESULT")
}

/// A review that ends on an agy error: `message` in its result and on stderr, then exit 3.
fn agy_fails(message: &str, finished: &str) -> String {
    let result = json!({
        "conversation_id": "c1",
        "status": "ERROR",
        "response": "",
        "error": message,
        "finished_reason": finished,
    });
    let error = json!({"status": "RESOURCE_EXHAUSTED", "code": 429, "retryable": false, "message": message});
    format!("cat <<'RESULT'\n{result}\nRESULT\necho 'AGY_ERROR: {error}' >&2\nexit 3")
}

/// A fake `codex` that writes `report` at once.
fn codex_writes(report: &str) -> String {
    format!(
        "out=\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\n\
         cat > /dev/null\n\
         cat > \"$out\" <<'REPORT'\n{report}REPORT"
    )
}

/// A sandbox whose user namespaces work, so agy's sandbox is active.
fn sandboxed() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.fake("unshare", "exit 0");
    sandbox
}

fn configure(sandbox: &Sandbox, toml: &str) {
    let dir = sandbox.path("config/factory");
    fs::create_dir_all(&dir).unwrap_or_default();
    fs::write(dir.join("config.toml"), toml).unwrap_or_default();
}

/// The value that follows `flag` in `argv`.
fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let at = argv.iter().position(|arg| arg == flag)?;
    argv.get(at + 1).map(String::as_str)
}

/// The calls of the fake agy that ran a review, not its usage query.
fn reviews_run(sandbox: &Sandbox) -> Vec<Vec<String>> {
    sandbox
        .calls("agy")
        .into_iter()
        .filter(|argv| after(argv, "-p") != Some("/usage"))
        .collect()
}

fn usage_queries(sandbox: &Sandbox) -> usize {
    sandbox.calls("agy").len() - reviews_run(sandbox).len()
}

fn shown(stdout: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or(Value::Null)
}

fn event(sandbox: &Sandbox, index: usize) -> Value {
    let lines = sandbox.ledger_lines();
    serde_json::from_str(&lines[index]).unwrap_or(Value::Null)
}

fn settings_of(argv: &[String]) -> Value {
    after(argv, "--settings")
        .and_then(|settings| serde_json::from_str(settings).ok())
        .unwrap_or(Value::Null)
}

#[test]
fn agy_reviews_first_in_plan_mode_in_its_sandbox_with_writes_urls_and_mcp_denied() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(shown(&out.stdout)["backend"], "agy", "{out:?}");
    assert!(sandbox.calls("codex").is_empty(), "agy comes first");
    assert_eq!(
        sandbox.calls("agy")[0],
        ["-p", "/usage", "--output-format", "json"],
        "the usage query comes before the review"
    );
    let runs = reviews_run(&sandbox);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let argv = &runs[0];
    assert!(
        after(argv, "-p").is_some_and(|prompt| prompt.starts_with("/code-review since main.\n")),
        "{argv:?}"
    );
    assert_eq!(after(argv, "--mode"), Some("plan"), "{argv:?}");
    assert!(argv.iter().any(|arg| arg == "--sandbox"), "{argv:?}");
    assert_eq!(after(argv, "--output-format"), Some("json"), "{argv:?}");
    assert_eq!(after(argv, "--print-timeout"), Some("1800s"), "{argv:?}");
    assert_eq!(after(argv, "--model"), Some("gemini-4-neon"), "{argv:?}");
    assert_eq!(after(argv, "--effort"), Some("high"), "{argv:?}");
    let shared = after(argv, "--add-dir").and_then(|dir| fs::canonicalize(dir).ok());
    assert_eq!(
        shared,
        fs::canonicalize(repo.join(".git")).ok(),
        "the sandbox sees the git dir: {argv:?}"
    );
    let settings = settings_of(argv);
    assert_eq!(
        settings["useG1Credits"], false,
        "no paid credits: {settings}"
    );
    for rule in DENIED {
        assert!(
            settings["permissions"]["deny"]
                .as_array()
                .is_some_and(|deny| deny.iter().any(|denied| denied == rule)),
            "{rule} denied: {settings}"
        );
    }
    assert!(settings.get("modelProvider").is_none(), "{settings}");
    let reviews = sandbox.reviews();
    assert_eq!(
        fs::read_to_string(reviews.join("t1-r1.md")).unwrap(),
        CLEAN_REPORT
    );
    assert!(
        fs::read_to_string(reviews.join("t1-r1.agy.json"))
            .unwrap()
            .contains("\"SUCCESS\""),
        "agy's result is kept"
    );
    let logged = event(&sandbox, 0);
    assert_eq!(logged["backend"], "agy", "{logged}");
    assert_eq!(logged["model"], "gemini-4-neon", "{logged}");
    assert_eq!(logged["effort"], "high", "{logged}");
    assert_eq!(logged["outcome"], "ok", "{logged}");
}

#[test]
fn an_empty_usage_bucket_skips_agy_until_it_resets() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let reset = in_two_hours();
    sandbox.fake("agy", &agy(&usage(0.0, reset), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let shown = shown(&out.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(
        shown["cooled"],
        json!([{"backend": "agy", "until": reset.to_string()}]),
        "{shown}"
    );
    assert!(reviews_run(&sandbox).is_empty(), "agy never reviews");
    assert_eq!(event(&sandbox, 0)["fallback_from"], "agy");

    let later = sandbox.factory_in(&repo, &["review", "main", "t2"]);

    assert_eq!(later.code, 0, "{later:?}");
    assert_eq!(
        sandbox.calls("agy").len(),
        1,
        "a later review skips agy without asking"
    );
    assert_eq!(sandbox.calls("codex").len(), 2);
}

#[test]
fn the_usage_query_is_cached_for_reviews_close_together() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let first = sandbox.factory_in(&repo, &["review", "main", "t1"]);
    let second = sandbox.factory_in(&repo, &["review", "main", "t2"]);

    assert_eq!((first.code, second.code), (0, 0), "{first:?} {second:?}");
    assert_eq!(reviews_run(&sandbox).len(), 2);
    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));
}

#[test]
fn a_model_in_another_group_is_checked_against_that_groups_quota() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\nname = \"agy-claude\"\nmodel = \"claude-opus-5-5\"\n",
    );
    sandbox.fake(
        "agy",
        &agy(&usage(0.0, in_two_hours()), &agy_writes(CLEAN_REPORT)),
    );

    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(shown(&out.stdout)["backend"], "agy-claude", "{out:?}");
    assert_eq!(
        after(&reviews_run(&sandbox)[0], "--model"),
        Some("claude-opus-5-5")
    );
}

#[test]
fn an_inactive_sandbox_is_reported_and_agy_is_skipped() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake(
        "unshare",
        "echo 'unshare: unshare failed: Operation not permitted' >&2\nexit 1",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);
    let json = sandbox.factory_in(&repo, &["review", "--json", "main", "t2"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout.lines().count(), 1, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: agy skipped: its sandbox is not active"),
        "{out:?}"
    );
    assert!(reviews_run(&sandbox).is_empty(), "agy never reviews");
    assert_eq!(sandbox.calls("codex").len(), 2);
    assert_eq!(event(&sandbox, 0)["skipped"], "agy");
    assert_eq!(json.code, 0, "{json:?}");
    assert_eq!(
        json.stderr, "",
        "--json keeps the notice in its output: {json:?}"
    );
    let shown = shown(&json.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(shown["skipped"][0]["backend"], "agy", "{shown}");
    assert!(
        shown["skipped"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("its sandbox is not active")),
        "{shown}"
    );
    assert_eq!(shown["cooled"], json!([]), "a skip is not a cooldown");
}

#[test]
fn agys_individual_quota_error_cools_it_down_until_its_reset_and_codex_reruns_the_review() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let failed = agy_fails(
        "RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h13m5s.",
        "error",
    );
    sandbox.fake("agy", &agy(&usage_left(), &failed));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let before = now_millis();
    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1"]);
    let after_run = now_millis();

    assert_eq!(out.code, 0, "{out:?}");
    let shown = shown(&out.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(shown["cooled"][0]["backend"], "agy", "{shown}");
    let until = shown["cooled"][0]["until"]
        .as_str()
        .and_then(Timestamp::parse);
    let span = 7_985_000;
    assert!(
        until.is_some_and(|until| {
            (before + span..=after_run + span).contains(&until.unix_millis())
        }),
        "cooled 2h13m5s on: {shown}"
    );
    assert_eq!(event(&sandbox, 0)["fallback_from"], "agy");

    let later = sandbox.factory_in(&repo, &["review", "main", "t2"]);

    assert_eq!(later.code, 0, "{later:?}");
    assert_eq!(reviews_run(&sandbox).len(), 1, "agy is cooling down");
}

#[test]
fn any_other_agy_exit_3_is_a_failed_review() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let failed = agy_fails(
        "INTERNAL: Individual quota reached? no: the stream closed",
        "error",
    );
    sandbox.fake("agy", &agy(&usage_left(), &failed));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: agy exited 3; see "),
        "{out:?}"
    );
    assert!(sandbox.calls("codex").is_empty(), "no fallback");
    assert_eq!(event(&sandbox, 0)["outcome"], "error");
}

#[test]
fn a_print_timeout_ends_the_review_as_a_timeout() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    configure(&sandbox, "[[review.backend]]\ncli = \"agy\"\ntimeout = 5\n");
    sandbox.fake(
        "agy",
        &agy(
            &usage_left(),
            &agy_fails("print timeout after 5s", "print_timeout"),
        ),
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: agy timed out after 5s"),
        "{out:?}"
    );
    assert_eq!(
        after(&reviews_run(&sandbox)[0], "--print-timeout"),
        Some("5s")
    );
}

/// A review that notes the `GEMINI_API_KEY` it sees in `bin/agy.key`, then writes `report`.
fn agy_notes_the_key(report: &str) -> String {
    format!(
        "printf '%s' \"${{GEMINI_API_KEY-unset}}\" > \"$0.key\"\n{}",
        agy_writes(report)
    )
}

#[test]
fn the_subscription_login_keeps_any_api_key_away_from_agy() {
    let sandbox = sandboxed().env("GEMINI_API_KEY", "paid-key");
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\nauth = \"subscription\"\n",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_notes_the_key(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        fs::read_to_string(sandbox.path("bin/agy.key")).unwrap(),
        "unset"
    );
    let settings = settings_of(&reviews_run(&sandbox)[0]);
    assert!(settings.get("modelProvider").is_none(), "{settings}");
}

#[test]
fn an_api_key_runs_agy_on_the_gemini_api_without_a_usage_query() {
    let sandbox = sandboxed().env("GEMINI_API_KEY", "test-key");
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\nauth = \"api-key\"\n",
    );
    sandbox.fake(
        "agy",
        &agy(
            &usage(0.0, in_two_hours()),
            &agy_notes_the_key(CLEAN_REPORT),
        ),
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        usage_queries(&sandbox),
        0,
        "an API key has no account quota"
    );
    assert_eq!(
        fs::read_to_string(sandbox.path("bin/agy.key")).unwrap(),
        "test-key"
    );
    let settings = settings_of(&reviews_run(&sandbox)[0]);
    assert_eq!(settings["modelProvider"], "gemini", "{settings}");
    assert_eq!(settings["useG1Credits"], false, "{settings}");
}

#[test]
fn an_api_key_backend_without_a_key_is_skipped() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\nauth = \"api-key\"\n",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("GEMINI_API_KEY"), "{out:?}");
    assert!(
        sandbox.calls("agy").is_empty(),
        "{:?}",
        sandbox.calls("agy")
    );
}

#[test]
fn auth_is_refused_on_a_backend_other_than_agy() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"codex\"\nauth = \"api-key\"\n",
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("auth"), "{out:?}");
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn an_inactive_sandbox_stays_reported_when_the_next_backend_fails() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    sandbox.fake("unshare", "exit 1");
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", "cat > /dev/null\nexit 1");

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);
    let json = sandbox.factory_in(&repo, &["review", "--json", "main", "t2"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: codex exited 1; "),
        "{out:?}"
    );
    assert!(
        out.stderr
            .contains("agy skipped: its sandbox is not active"),
        "{out:?}"
    );
    assert!(
        shown(&json.stdout)["error"]
            .as_str()
            .is_some_and(|error| error.contains("agy skipped: its sandbox is not active")),
        "{json:?}"
    );
}

/// A fake agy whose usage query answers `first` once, then `later`.
fn agy_usage_changes(first: &str, later: &str, review: &str) -> String {
    format!(
        "if [ \"$2\" = /usage ]; then\n\
         if [ -e \"$0.asked\" ]; then cat <<'USAGE'\n{later}\nUSAGE\n\
         else : > \"$0.asked\"; cat <<'USAGE'\n{first}\nUSAGE\nfi\nexit 0\nfi\n{review}"
    )
}

#[test]
fn a_cached_usage_whose_empty_bucket_has_reset_is_asked_again() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let reset = Timestamp::from_unix_millis(now_millis() + 1500);
    sandbox.fake(
        "agy",
        &agy_usage_changes(&usage(0.0, reset), &usage_left(), &agy_writes(CLEAN_REPORT)),
    );
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    let first = sandbox.factory_in(&repo, &["review", "--json", "main", "t1"]);
    assert_eq!(shown(&first.stdout)["backend"], "codex", "{first:?}");
    // Wait, on the clock, for the reset the cached answer names to pass.
    let deadline = Instant::now() + Duration::from_secs(10);
    while now_millis() <= reset.unix_millis() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }

    let later = sandbox.factory_in(&repo, &["review", "--json", "main", "t2"]);

    assert_eq!(later.code, 0, "{later:?}");
    assert_eq!(shown(&later.stdout)["backend"], "agy", "{later:?}");
    assert_eq!(usage_queries(&sandbox), 2, "{:?}", sandbox.calls("agy"));
}

/// Whether the process `pid` has died (a zombie has), within a deadline.
fn died(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
        if stat.is_empty() || state == Some(Some('Z')) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn what_agy_leaves_running_at_its_print_timeout_is_killed() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let straggler = "sleep 300 </dev/null >/dev/null 2>&1 &\necho $! > \"$0.straggler\"\n";
    let timed_out = agy_fails("print timeout after 1800s", "print_timeout");
    sandbox.fake(
        "agy",
        &agy(&usage_left(), &format!("{straggler}{timed_out}")),
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 124, "{out:?}");
    let pid = fs::read_to_string(sandbox.path("bin/agy.straggler")).unwrap();
    assert!(died(pid.trim()), "sleep {} still runs", pid.trim());
}

#[test]
fn a_timeout_too_long_to_add_a_grace_to_still_runs_the_review() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(
        &repo,
        &["review", "--timeout", "18446744073709551615", "main", "t1"],
    );

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        after(&reviews_run(&sandbox)[0], "--print-timeout"),
        Some("18446744073709551615s")
    );
}
