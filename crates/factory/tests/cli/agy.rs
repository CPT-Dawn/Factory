//! Antigravity (`agy`) as the first review backend: its free usage query before launch, its
//! read-only and offline invocation, its sandbox check, its quota error and its auth modes.

use std::fs;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};

use crate::support::{BWRAP_PASSES, PROXY_LISTENS, Run, Sandbox, finish, spawn};

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

/// What agy logs at startup when its `--sandbox` is in effect, as `agy -p` 1.5 logs it.
const SANDBOX_STARTED: &str =
    "I1123 09:37:00.811680 1 session.go:87] Print mode: enabling terminal sandbox for this session";

/// A fake `agy`: `agy -p /usage` prints `usage`; any other run keeps its prompt (the
/// value of `-p`) as `bin/agy.prompt` and its working directory as `bin/agy.cwd`, logs that
/// its sandbox started to the file its `--log-file` names, and runs `review`.
fn agy(usage: &str, review: &str) -> String {
    agy_logging(usage, SANDBOX_STARTED, review)
}

/// A fake `agy` as [`agy`], that logs `line` instead of its sandbox starting.
fn agy_logging(usage: &str, line: &str, review: &str) -> String {
    format!(
        "if [ \"$2\" = /usage ]; then\ncat <<'USAGE'\n{usage}\nUSAGE\nexit 0\nfi\n\
         printf '%s' \"$2\" > \"$0.prompt\"\npwd > \"$0.cwd\"\n\
         log=; prev=; for a in \"$@\"; do [ \"$prev\" = --log-file ] && log=$a; prev=$a; done\n\
         [ -n \"$log\" ] && cat >> \"$log\" <<'LOG'\n{line}\nLOG\n{review}"
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

/// A sandbox whose user namespaces work and whose write guard starts, so agy's sandbox is
/// active.
/// A sandbox in which agy's sandbox and its write guard start, on a session bus.
fn sandboxed() -> Sandbox {
    let sandbox = Sandbox::new();
    let bus = format!("unix:path={},guid=0", sandbox.path("run/bus").display());
    let sandbox = sandbox.env("DBUS_SESSION_BUS_ADDRESS", bus);
    sandbox.fake("unshare", "exit 0");
    sandbox.fake("bwrap", BWRAP_PASSES);
    sandbox.fake("xdg-dbus-proxy", PROXY_LISTENS);
    sandbox
}

fn configure(sandbox: &Sandbox, toml: &str) {
    let dir = sandbox.path("config/factory");
    fs::create_dir_all(&dir).unwrap_or_default();
    fs::write(dir.join("config.toml"), toml).unwrap_or_default();
}

/// The value that follows `flag` in `argv`.
/// The old default backends, agy then codex, each on its own model: what these tests are about,
/// whatever pools and chains the defaults hold.
fn agy_then_codex(sandbox: &Sandbox) {
    configure(
        sandbox,
        "[[review.backend]]\ncli = \"agy\"\n\n[[review.backend]]\ncli = \"codex\"\n",
    );
}

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
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\nmodel = \"gemini-4-neon\"\neffort = \"high\"\n\n[[review.backend]]\ncli = \"codex\"\n",
    );
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
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    let reset = in_two_hours();
    sandbox.fake("agy", &agy(&usage(0.0, reset), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

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

    let later = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t2"]);

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

    let first = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);
    let second = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t2"]);

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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

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
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    sandbox.fake(
        "unshare",
        "echo 'unshare: unshare failed: Operation not permitted' >&2\nexit 1",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);
    let json = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t2"]);

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
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    let failed = agy_fails(
        "RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h13m5s.",
        "error",
    );
    sandbox.fake("agy", &agy(&usage_left(), &failed));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let before = now_millis();
    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);
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

    let later = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t2"]);

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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: agy exited 3; see "),
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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: agy timed out after 5s"),
        "{out:?}"
    );
    assert_eq!(
        after(&reviews_run(&sandbox)[0], "--print-timeout"),
        Some("5s")
    );
}

/// The repo's worktrees, as `git worktree list --porcelain` names them.
fn worktrees(sandbox: &Sandbox, repo: &std::path::Path) -> Vec<String> {
    let out = sandbox
        .command("git", &["worktree", "list", "--porcelain"])
        .current_dir(repo)
        .output()
        .map(|out| out.stdout)
        .unwrap_or_default();
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(str::to_owned)
        .collect()
}

/// The repo's status, as `git status --porcelain --ignored` prints it.
fn status(sandbox: &Sandbox, repo: &std::path::Path) -> String {
    let out = sandbox
        .command("git", &["status", "--porcelain", "--ignored"])
        .current_dir(repo)
        .output()
        .map(|out| out.stdout)
        .unwrap_or_default();
    String::from_utf8_lossy(&out).into_owned()
}

/// Where the fake agy ran its review.
fn agys_cwd(sandbox: &Sandbox) -> std::path::PathBuf {
    std::path::PathBuf::from(
        fs::read_to_string(sandbox.path("bin/agy.cwd"))
            .unwrap_or_default()
            .trim_end(),
    )
}

/// A review that writes into its working directory and moves its checkout's HEAD, as a
/// terminal command run outside a sandbox could, and notes the commit it saw; then `then`.
fn agy_scribbles(then: &str) -> String {
    format!(
        "echo scribbled > probe.txt\necho changed > a.txt\nmkdir -p sub && echo x > sub/new\n\
         git rev-parse HEAD > \"$0.head\"\ngit checkout -q --detach HEAD~1\n{then}"
    )
}

#[test]
fn agy_reviews_a_throwaway_checkout_of_the_head_so_its_writes_leave_the_callers_tree_alone() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    sandbox.fake(
        "agy",
        &agy(&usage_left(), &agy_scribbles(&agy_writes(CLEAN_REPORT))),
    );
    let head = sandbox.git(&repo, &["rev-parse", "HEAD"]);
    let before = status(&sandbox, &repo);

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        status(&sandbox, &repo),
        before,
        "the caller's tree is unchanged"
    );
    assert!(!repo.join("probe.txt").exists());
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert_eq!(sandbox.git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(sandbox.path("bin/agy.head")).unwrap(),
        head,
        "agy saw the reviewed head"
    );
    let cwd = agys_cwd(&sandbox);
    let top = fs::canonicalize(&repo).unwrap();
    assert!(!cwd.starts_with(&top), "agy ran outside the repo: {cwd:?}");
    assert!(!cwd.exists(), "its checkout is removed: {cwd:?}");
    assert_eq!(worktrees(&sandbox, &repo).len(), 1, "no worktree is left");
    let argv = &reviews_run(&sandbox)[0];
    let shared = after(argv, "--add-dir").and_then(|dir| fs::canonicalize(dir).ok());
    assert_eq!(shared, fs::canonicalize(repo.join(".git")).ok(), "{argv:?}");
    let prompt = fs::read_to_string(sandbox.path("bin/agy.prompt")).unwrap();
    let bundle = sandbox.reviews().join("t1.bundle.md");
    assert!(
        prompt.contains(&format!("{}.", bundle.display())),
        "the prompt names the bundle by its full path: {prompt}"
    );
    assert!(bundle.is_file(), "the bundle resolves from anywhere");
    assert_eq!(
        fs::read_to_string(sandbox.reviews().join("t1.md")).unwrap(),
        CLEAN_REPORT
    );
}

/// A guard's own arguments and the command it runs: `argv` split at its first `--`.
fn guarded(argv: &[String]) -> (&[String], &[String]) {
    let at = argv
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(argv.len());
    (&argv[..at], argv.get(at + 1..).unwrap_or_default())
}

/// The guard calls that ran `program`.
fn guards_of(sandbox: &Sandbox, program: &str) -> Vec<Vec<String>> {
    sandbox
        .calls("bwrap")
        .into_iter()
        .filter(|argv| {
            guarded(argv)
                .1
                .first()
                .is_some_and(|first| first == program)
        })
        .collect()
}

/// The paths each `flag` of a guard mounts, as its `(source, target)` pairs.
fn mounts(guard: &[String], flag: &str) -> Vec<(String, String)> {
    guard
        .iter()
        .enumerate()
        .filter(|(_, arg)| *arg == flag)
        .filter_map(|(at, _)| Some((guard.get(at + 1)?.clone(), guard.get(at + 2)?.clone())))
        .collect()
}

#[test]
fn agy_reviews_inside_a_write_guard_that_leaves_only_its_checkout_its_log_and_its_state_writable() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let runs = guards_of(&sandbox, "agy");
    assert_eq!(runs.len(), 1, "the review runs under the guard: {runs:?}");
    let (guard, program) = guarded(&runs[0]);
    assert_eq!(
        program[1..],
        reviews_run(&sandbox)[0],
        "agy runs under it as it would bare"
    );
    assert_eq!(
        guard[..3],
        ["--ro-bind", "/", "/"],
        "the whole filesystem is read-only first: {guard:?}"
    );
    let path = |rel: &str| sandbox.path(rel).display().to_string();
    let mut writable: Vec<String> = mounts(guard, "--bind")
        .into_iter()
        .map(|(source, target)| {
            assert_eq!(source, target, "{guard:?}");
            target
        })
        .collect();
    writable.sort();
    let mut expected = vec![
        agys_cwd(&sandbox).display().to_string(),
        sandbox
            .reviews()
            .join("t1.agy.run.log")
            .display()
            .to_string(),
    ];
    expected.sort();
    assert_eq!(
        writable, expected,
        "its checkout and its own log: {guard:?}"
    );
    let state: Vec<(String, String)> = [
        path("home/.gemini"),
        path("config/antigravity"),
        path("home/.cache/antigravity"),
    ]
    .into_iter()
    .map(|dir| (dir.clone(), dir))
    .collect();
    assert_eq!(
        mounts(guard, "--bind-try"),
        state,
        "agy's own state: {guard:?}"
    );
    let tmpfs: Vec<&String> = guard
        .iter()
        .enumerate()
        .filter(|(_, arg)| *arg == "--tmpfs")
        .filter_map(|(at, _)| guard.get(at + 1))
        .collect();
    assert!(
        tmpfs.contains(&&path("run")),
        "the runtime dir and its sockets are hidden: {guard:?}"
    );
    assert!(
        mounts(guard, "--ro-bind-try")
            .contains(&("/dev/null".to_owned(), "/run/docker.sock".to_owned())),
        "Docker's socket is masked: {guard:?}"
    );
    assert!(
        guard.windows(2).any(|pair| pair == ["--dev", "/dev"]),
        "a private /dev, not the host's: {guard:?}"
    );
    assert!(!guard.contains(&"--dev-bind".to_owned()), "{guard:?}");

    // The session bus comes back through a proxy that lets it reach the keyring alone.
    let proxies = sandbox.calls("xdg-dbus-proxy");
    assert_eq!(
        proxies.len(),
        2,
        "one for the probe, one for the review: {proxies:?}"
    );
    let proxy = &proxies[1];
    assert_eq!(proxy[0], format!("unix:path={},guid=0", path("run/bus")));
    assert_eq!(
        proxy[2..],
        ["--filter", "--talk=org.freedesktop.secrets", "--fd=1"],
        "{proxy:?}"
    );
    assert!(
        mounts(guard, "--ro-bind").contains(&(proxy[1].clone(), path("run/bus"))),
        "the proxy's socket stands in for the bus: {guard:?}"
    );
    assert!(
        guard.windows(3).any(|three| three
            == [
                "--setenv".to_owned(),
                "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                format!("unix:path={}", path("run/bus")),
            ]),
        "{guard:?}"
    );
    let pids = fs::read_to_string(sandbox.path("bin/proxy.pids")).unwrap();
    for pid in pids.split_whitespace() {
        // Gone, or dead and not yet reaped by whoever adopted it.
        let state = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = state.rsplit(") ").next().unwrap_or_default();
        assert!(
            state.is_empty() || state.starts_with('Z'),
            "a proxy's whole group is stopped with its guard: {pid} {state}"
        );
    }
}

#[test]
fn an_api_key_review_reaches_no_session_bus() {
    let sandbox = sandboxed().env("GEMINI_API_KEY", "test-key");
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\nauth = \"api-key\"\n",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        sandbox.calls("xdg-dbus-proxy").is_empty(),
        "no keyring to reach: {:?}",
        sandbox.calls("xdg-dbus-proxy")
    );
    let runs = guards_of(&sandbox, "agy");
    let (guard, _) = guarded(&runs[0]);
    assert!(
        guard
            .windows(2)
            .any(|pair| pair == ["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"]),
        "{guard:?}"
    );
}

#[test]
fn a_bus_proxy_that_cannot_start_skips_agy_in_its_words() {
    let sandbox = sandboxed();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    sandbox.fake(
        "xdg-dbus-proxy",
        "echo 'Failed to connect to bus: No such file or directory' >&2\nexit 1",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        reviews_run(&sandbox).is_empty(),
        "agy never reviews on an open bus"
    );
    let shown = shown(&out.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(
        shown["skipped"][0]["reason"],
        "its sandbox is not active (its write guard can't start: \
         xdg-dbus-proxy: Failed to connect to bus: No such file or directory)",
        "{shown}"
    );
    assert!(
        sandbox.calls("bwrap").is_empty(),
        "{:?}",
        sandbox.calls("bwrap")
    );
}

#[test]
fn a_write_guard_that_cannot_start_skips_agy_and_codex_reviews() {
    let sandbox = Sandbox::new();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    sandbox.fake("unshare", "exit 0");
    sandbox.fake(
        "bwrap",
        "echo 'bwrap: setting up uid map: Permission denied' >&2\nexit 1",
    );
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        reviews_run(&sandbox).is_empty(),
        "agy never reviews unguarded"
    );
    let shown = shown(&out.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(shown["skipped"][0]["backend"], "agy", "{shown}");
    assert_eq!(
        shown["skipped"][0]["reason"],
        "its sandbox is not active (its write guard can't start: bwrap: setting up uid map: Permission denied)",
        "{shown}"
    );
    let probes = sandbox.calls("bwrap");
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert_eq!(
        guarded(&probes[0]).1,
        ["unshare", "--user", "--map-root-user", "true"],
        "the probe starts a user namespace inside the guard, as agy's sandbox will"
    );
}

#[test]
fn with_no_user_namespaces_an_agy_only_chain_refuses_the_review_in_one_line() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[[review.backend]]\ncli = \"agy\"\n");
    let repo = sandbox.repo();
    sandbox.fake(
        "unshare",
        "echo 'unshare: unshare failed: Operation not permitted' >&2\nexit 1",
    );
    sandbox.fake("bwrap", BWRAP_PASSES);
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stdout, "", "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: no review backend can take the "),
        "{out:?}"
    );
    assert!(
        out.stderr.contains(
            "agy its sandbox is not active (unprivileged user namespaces are unavailable: \
             unshare: unshare failed: Operation not permitted)"
        ),
        "{out:?}"
    );
    assert!(
        reviews_run(&sandbox).is_empty(),
        "agy never reviews unguarded"
    );
    assert!(
        sandbox.calls("bwrap").is_empty(),
        "nothing runs under a guard: {:?}",
        sandbox.calls("bwrap")
    );
}

#[test]
fn agys_state_behind_a_symlink_is_given_back_where_it_lives() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let dotfiles = sandbox.path("home/dotfiles/gemini");
    fs::create_dir_all(&dotfiles).unwrap();
    std::os::unix::fs::symlink(&dotfiles, sandbox.path("home/.gemini")).unwrap();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let runs = guards_of(&sandbox, "agy");
    let (guard, _) = guarded(&runs[0]);
    let real = fs::canonicalize(&dotfiles).unwrap().display().to_string();
    assert!(
        mounts(guard, "--bind-try").contains(&(real.clone(), real)),
        "bwrap can't mount on a symlink: {guard:?}"
    );
}

#[test]
fn agy_reviews_from_the_same_subdirectory_of_its_checkout() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    fs::create_dir(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/spec.md"), "# spec\n").unwrap();
    sandbox.git(&repo, &["add", "."]);
    sandbox.git(&repo, &["commit", "-q", "-m", "docs: add a spec"]);
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(
        &repo.join("docs"),
        &["review", "--standalone", "main", "t1", "spec.md"],
    );

    assert_eq!(out.code, 0, "{out:?}");
    let cwd = agys_cwd(&sandbox);
    assert_eq!(cwd.file_name().and_then(|name| name.to_str()), Some("docs"));
    assert!(
        !cwd.starts_with(fs::canonicalize(&repo).unwrap()),
        "{cwd:?}"
    );
}

#[test]
fn agys_checkout_is_removed_after_a_print_timeout() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    let timed_out = agy_fails("print timeout after 1800s", "print_timeout");
    sandbox.fake("agy", &agy(&usage_left(), &agy_scribbles(&timed_out)));
    let before = status(&sandbox, &repo);

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert!(!agys_cwd(&sandbox).exists(), "its checkout is removed");
    assert_eq!(worktrees(&sandbox, &repo).len(), 1, "no worktree is left");
    assert_eq!(status(&sandbox, &repo), before);
}

#[test]
fn a_checkout_left_by_a_review_that_died_is_swept() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    // No process has an ID past the kernel's largest, 2^22.
    let left = sandbox.path("state/factory/checkouts/repo-4194999");
    fs::create_dir_all(&left).unwrap();
    fs::write(left.join("a.txt"), "left\n").unwrap();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(!left.exists(), "the dead review's checkout is swept");
}

#[test]
fn a_review_whose_sandbox_did_not_start_is_reported_and_codex_reruns_it() {
    let sandbox = sandboxed();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    let unavailable = "W1123 09:37:00.811680 1 session.go:80] Terminal sandbox unavailable: \
                       AppArmor restricts unprivileged user namespaces \
                       (kernel.apparmor_restrict_unprivileged_userns = 1)";
    sandbox.fake(
        "agy",
        &agy_logging(
            &usage_left(),
            unavailable,
            &agy_writes("## agy's unsandboxed report\n"),
        ),
    );
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let shown = shown(&out.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(shown["skipped"][0]["backend"], "agy", "{shown}");
    let reason = shown["skipped"][0]["reason"].as_str().unwrap_or_default();
    assert!(reason.starts_with("its sandbox did not start"), "{shown}");
    assert!(reason.contains("AppArmor"), "{shown}");
    assert_eq!(
        fs::read_to_string(sandbox.reviews().join("t1.md")).unwrap(),
        CLEAN_REPORT,
        "the unsandboxed report is not the review"
    );
}

#[test]
fn a_review_whose_log_never_says_its_sandbox_started_is_not_trusted() {
    let sandbox = sandboxed();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    sandbox.fake(
        "agy",
        &agy_logging(
            &usage_left(),
            "I1123 printmode.go:209] Print mode: starting",
            &agy_writes(CLEAN_REPORT),
        ),
    );
    sandbox.fake("codex", "cat > /dev/null\nexit 1");

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .contains("agy skipped: its sandbox did not start"),
        "{out:?}"
    );
    assert!(
        !sandbox.reviews().join("t1.md").exists(),
        "no report is kept"
    );
}

/// agy's settings file in the sandbox's home, with `model` as the model chosen in agy.
fn agy_settings(sandbox: &Sandbox, model: &str) {
    let dir = sandbox.path("home/.gemini/antigravity-cli");
    fs::create_dir_all(&dir).unwrap_or_default();
    fs::write(
        dir.join("settings.json"),
        json!({"model": model, "trustedWorkspaces": ["~"]}).to_string(),
    )
    .unwrap_or_default();
}

/// agy's usage with the Gemini group full and the Claude and GPT group's five-hour bucket
/// empty until `reset`.
fn usage_claude_empty(reset: Timestamp) -> String {
    let mut usage: Value = serde_json::from_str(&usage(1.0, in_two_hours())).unwrap_or_default();
    usage["command"]["data"]["groups"][1]["buckets"][1]["remaining_fraction"] = json!(0.0);
    usage["command"]["data"]["groups"][1]["buckets"][1]["reset_time"] = json!(reset.to_string());
    usage.to_string()
}

#[test]
fn with_no_model_configured_the_quota_checked_is_that_of_the_model_chosen_in_agy() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\n\n[[review.backend]]\ncli = \"codex\"\n",
    );
    agy_settings(&sandbox, "Claude Opus 5.5 (Thinking)");
    sandbox.fake(
        "agy",
        &agy(
            &usage_claude_empty(in_two_hours()),
            &agy_writes(CLEAN_REPORT),
        ),
    );
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let shown = shown(&out.stdout);
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(shown["cooled"][0]["backend"], "agy", "{shown}");
    assert!(reviews_run(&sandbox).is_empty(), "agy never reviews");
}

#[test]
fn with_no_model_chosen_anywhere_the_quota_checked_is_that_of_agys_default_gemini() {
    for chosen in [None, Some("")] {
        let sandbox = sandboxed();
        let repo = sandbox.repo();
        configure(
            &sandbox,
            "[[review.backend]]\ncli = \"agy\"\n\n[[review.backend]]\ncli = \"codex\"\n",
        );
        if let Some(model) = chosen {
            agy_settings(&sandbox, model);
        }
        sandbox.fake(
            "agy",
            &agy(&usage(0.0, in_two_hours()), &agy_writes(CLEAN_REPORT)),
        );
        sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

        let out = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);

        assert_eq!(out.code, 0, "{chosen:?}: {out:?}");
        assert_eq!(
            shown(&out.stdout)["backend"],
            "codex",
            "{chosen:?}: {out:?}"
        );
        assert!(reviews_run(&sandbox).is_empty(), "{chosen:?}");
    }
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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("auth"), "{out:?}");
    assert!(sandbox.calls("codex").is_empty());
}

#[test]
fn an_inactive_sandbox_stays_reported_when_the_next_backend_fails() {
    let sandbox = Sandbox::new();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    sandbox.fake("unshare", "exit 1");
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", "cat > /dev/null\nexit 1");

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);
    let json = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t2"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: code-review: codex exited 1; "),
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
         else : > \"$0.asked\"; cat <<'USAGE'\n{first}\nUSAGE\nfi\nexit 0\nfi\n{}",
        agy(later, review)
    )
}

#[test]
fn a_cached_usage_whose_empty_bucket_has_reset_is_asked_again() {
    let sandbox = sandboxed();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    let reset = Timestamp::from_unix_millis(now_millis() + 1500);
    sandbox.fake(
        "agy",
        &agy_usage_changes(&usage(0.0, reset), &usage_left(), &agy_writes(CLEAN_REPORT)),
    );
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    let first = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t1"]);
    assert_eq!(shown(&first.stdout)["backend"], "codex", "{first:?}");
    // The daemon cools agy down until the reset the cached answer names; wait until it no
    // longer does.
    assert!(
        until_it(|| agy_quota(&sandbox) != "cooling"),
        "agy cools down for good"
    );

    let later = sandbox.factory_in(&repo, &["review", "--standalone", "--json", "main", "t2"]);

    assert_eq!(later.code, 0, "{later:?}");
    assert_eq!(shown(&later.stdout)["backend"], "agy", "{later:?}");
    // Two by routing, which asks again once its own cache is stale, and one by the daemon's
    // display of agy's quota, which the polling of `factory status` set going.
    assert_eq!(usage_queries(&sandbox), 3, "{:?}", sandbox.calls("agy"));
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

    let out = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);

    assert_eq!(out.code, 124, "{out:?}");
    let pid = fs::read_to_string(sandbox.path("bin/agy.straggler")).unwrap();
    assert!(died(pid.trim()), "sleep {} still runs", pid.trim());
}

#[test]
fn a_timeout_longer_than_a_week_is_refused_before_any_backend_runs() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    sandbox.fake("agy", &agy(&usage_left(), &agy_writes(CLEAN_REPORT)));

    let out = sandbox.factory_in(
        &repo,
        &[
            "review",
            "--standalone",
            "--timeout",
            "18446744073709551615",
            "main",
            "t1",
        ],
    );
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\ntimeout = 604801\n",
    );
    let configured = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t2"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "factory: code-review: --timeout 18446744073709551615 is longer than a week; give at most 604800\n"
    );
    assert_eq!(configured.code, 3, "{configured:?}");
    assert_eq!(
        configured.stderr,
        "factory: code-review: backend agy has a timeout of 604801s, longer than a week; set at most 604800 in the factory config\n"
    );
    assert!(
        sandbox.calls("agy").is_empty(),
        "{:?}",
        sandbox.calls("agy")
    );
}

/// agy's quota as `factory status` shows it: `unknown`, `available` or `cooling`.
fn agy_quota(sandbox: &Sandbox) -> String {
    let out = sandbox.factory(&["status", "--json"]);
    assert_eq!(out.code, 0, "{out:?}");
    shown(&out.stdout)["review"]
        .as_array()
        .and_then(|backends| backends.iter().find(|b| b["name"] == "agy"))
        .and_then(|agy| agy["quota"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Waits, within a deadline, for `done` to hold.
fn until_it(done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn agy_out_of_quota_is_not_granted_again_while_the_review_waits_for_codex() {
    let sandbox = sandboxed();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"agy\"\n\n[[review.backend]]\ncli = \"codex\"\nconcurrency = 1\nwhen_busy = \"queue\"\n",
    );
    let failed = agy_fails(
        "RESOURCE_EXHAUSTED (code 429): Individual quota reached. Resets in 1s.",
        "error",
    );
    sandbox.fake("agy", &agy(&usage_left(), &failed));
    // codex holds its one slot until the test opens the gate.
    let gate =
        "i=0\nwhile [ ! -e \"$0.gate\" ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\n";
    sandbox.fake("codex", &format!("{gate}{}", codex_writes(CLEAN_REPORT)));
    let review = |label: &str| {
        let mut cmd = sandbox.factory_command(&["review", "--standalone", "--json", "main", label]);
        cmd.current_dir(&repo);
        spawn(cmd)
    };
    let holder = review("t1");
    assert!(
        until_it(|| sandbox.calls("codex").len() == 1),
        "t1 reaches codex"
    );
    let waiter = review("t2");
    assert!(
        until_it(|| reviews_run(&sandbox).len() == 2),
        "t2 tries agy"
    );
    // Past the second cooldown, t2's own: a router that still offered agy would grant it now.
    assert!(
        until_it(|| agy_quota(&sandbox) == "cooling"),
        "t2's try cools agy down"
    );
    assert!(
        until_it(|| agy_quota(&sandbox) != "cooling"),
        "agy's second cooldown ends"
    );
    fs::write(sandbox.path("bin/codex.gate"), "").unwrap();

    let (holder, waiter) = (finish(holder), finish(waiter));

    assert_eq!(holder.code, 0, "{holder:?}");
    assert_eq!(waiter.code, 0, "{waiter:?}");
    assert_eq!(shown(&waiter.stdout)["backend"], "codex", "{waiter:?}");
    assert_eq!(
        reviews_run(&sandbox).len(),
        2,
        "agy is not granted to t2 again"
    );
}

/// agy's usage with two quota groups, each with a 5-hour and a weekly window, as agy prints it:
/// Gemini 30% and 7% spent, Claude and GPT 24% and 63%, all resetting far ahead.
fn two_groups() -> String {
    let bucket = |window: &str, left: f64| json!({"id": window, "name": window, "window": window, "remaining_fraction": left, "reset_time": "2099-01-01T00:00:00Z"});
    json!({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": [
        {"name": "Gemini Models", "buckets": [bucket("weekly", 0.93), bucket("5h", 0.7)]},
        {"name": "Claude and GPT models", "buckets": [bucket("5h", 0.76), bucket("weekly", 0.37)]},
    ]}}})
    .to_string()
}

/// A fake agy that answers its usage query with `answer`.
fn agy_says(answer: &str) -> String {
    format!("cat <<'USAGE'\n{answer}\nUSAGE")
}

fn quota_agy(sandbox: &Sandbox) -> Run {
    sandbox.factory(&["quota", "agy"])
}

/// Starts the daemon, which the bar never does: `factory status` does.
fn start(sandbox: &Sandbox) {
    let out = sandbox.factory(&["status"]);
    assert_eq!(out.code, 0, "{out:?}");
}

fn bar_json(sandbox: &Sandbox) -> Value {
    let out = sandbox.factory(&["bar", "--json"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    serde_json::from_str(&out.stdout).unwrap_or(Value::Null)
}

fn agy_chip(feed: &Value) -> Value {
    feed["glance"]
        .as_array()
        .and_then(|chips| chips.iter().find(|chip| chip["id"] == "agy"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn agy_card(feed: &Value) -> Value {
    feed["sections"]
        .as_array()
        .and_then(|cards| cards.iter().find(|card| card["id"] == "agy"))
        .cloned()
        .unwrap_or(Value::Null)
}

#[test]
fn quota_agy_lists_each_quota_groups_windows() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", &agy_says(&two_groups()));

    let out = quota_agy(&sandbox);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(
        out.stdout,
        "agy: Gemini Models: 5h 30%, weekly 7%; Claude and GPT models: 5h 24%, weekly 63%\n"
    );
    let json = sandbox.factory(&["quota", "agy", "--json"]);
    let shown = shown(&json.stdout);
    assert_eq!(shown["groups"].as_array().map(Vec::len), Some(2), "{shown}");
    assert_eq!(shown["error"], Value::Null, "{shown}");
}

/// The usage history's agy rows, as `group window used`.
fn agy_history(sandbox: &Sandbox) -> Vec<String> {
    let out = sandbox.factory(&["quota", "history", "--json"]);
    assert_eq!(out.code, 0, "{out:?}");
    let document = shown(&out.stdout);
    document["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|row| row["harness"] == "agy")
        .map(|row| {
            format!(
                "{} {} {}",
                row["group"].as_str().unwrap_or("-"),
                row["window"],
                row["used_percent"]
            )
        })
        .collect()
}

#[test]
fn each_agy_reading_joins_the_usage_history_by_group_and_window_once() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", &agy_says(&two_groups()));

    assert_eq!(quota_agy(&sandbox).code, 0);
    assert_eq!(
        quota_agy(&sandbox).code,
        0,
        "a second look reads the same reading"
    );

    let mut rows = agy_history(&sandbox);
    rows.sort();
    assert_eq!(
        rows,
        [
            "Claude and GPT models \"five_hour\" 24.0",
            "Claude and GPT models \"weekly\" 63.0",
            "Gemini Models \"five_hour\" 30.0",
            "Gemini Models \"weekly\" 7.0",
        ],
        "one row per group and window"
    );
}

#[test]
fn a_failed_usage_query_adds_nothing_to_the_history() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", "exit 1");

    let _ = quota_agy(&sandbox);

    assert_eq!(agy_history(&sandbox), Vec::<String>::new());
}

#[test]
fn the_bar_shows_the_worst_window_and_a_block_per_quota_group() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", &agy_says(&two_groups()));
    start(&sandbox);

    let feed = bar_json(&sandbox);

    let chip = agy_chip(&feed);
    assert_eq!(
        (&chip["text"], &chip["tone"]),
        (&json!("63%"), &json!("ok")),
        "{feed}"
    );
    let rows = agy_card(&feed)["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let meters: Vec<(&str, &str)> = rows
        .iter()
        .filter(|row| row["kind"] == "meter")
        .map(|row| {
            (
                row["label"].as_str().unwrap_or(""),
                row["text"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        meters,
        [
            ("5h", "30%"),
            ("weekly", "7%"),
            ("5h", "24%"),
            ("weekly", "63%")
        ]
    );
    let groups: Vec<&str> = rows
        .iter()
        .filter(|row| row["kind"] == "text")
        .map(|row| row["label"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(groups, ["Gemini Models", "Claude and GPT models"]);
    assert_eq!(
        sandbox.factory(&["bar"]).stdout,
        "claude ?/? · codex ?/? · agy 30/63 · ? leads · lab 0/0\n"
    );
}

/// What a failed or garbled usage query shows everywhere: not a share, but "agy usage
/// unreadable" and why.
fn assert_unreadable(sandbox: &Sandbox, why: &str) {
    let said = format!("agy usage unreadable: {why}");
    start(sandbox);
    let feed = bar_json(sandbox);
    let chip = agy_chip(&feed);
    assert_eq!(
        (&chip["text"], &chip["tone"]),
        (&json!("?"), &json!("muted")),
        "{feed}"
    );
    assert_eq!(agy_card(&feed)["error"], said, "{feed}");
    assert!(!agy_card(&feed).to_string().contains('%'), "{feed}");
    assert_eq!(
        sandbox.factory(&["bar"]).stdout,
        "claude ?/? · codex ?/? · agy unreadable · ? leads · lab 0/0\n"
    );

    let status = sandbox.factory(&["status"]);
    assert!(status.stdout_lines().contains(&said.as_str()), "{status:?}");
    let quota = quota_agy(sandbox);
    assert_eq!(
        (quota.code, quota.stdout.as_str()),
        (0, format!("{said}\n").as_str()),
        "{quota:?}"
    );
}

#[test]
fn a_failed_usage_query_is_unreadable_everywhere_never_a_share() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", "exit 3");

    assert_unreadable(&sandbox, "exited 3");
}

#[test]
fn a_garbled_usage_answer_is_unreadable_everywhere_never_a_share() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", &agy_says("this is not json"));

    assert_unreadable(&sandbox, "its answer is not JSON");
}

#[test]
fn a_used_bucket_with_no_reset_is_unreadable_never_a_reset_that_passed() {
    let sandbox = Sandbox::new();
    let used = two_groups().replace("2099-01-01T00:00:00Z", "1970-01-01T00:00:00Z");
    sandbox.fake("agy", &agy_says(&used));

    let out = quota_agy(&sandbox);

    // Every bucket of that answer is used and without a reset: each window is unknown.
    assert_eq!(
        out.stdout,
        "agy: Gemini Models: 5h unknown, weekly unknown; Claude and GPT models: 5h unknown, weekly unknown\n",
        "{out:?}"
    );
}

#[test]
fn a_second_bar_within_five_minutes_asks_agy_nothing_and_routing_asks_again_after_two() {
    let sandbox = sandboxed();
    agy_then_codex(&sandbox);
    let repo = sandbox.repo();
    sandbox.fake("agy", &agy(&two_groups(), &agy_writes(CLEAN_REPORT)));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    start(&sandbox);
    bar_json(&sandbox);
    bar_json(&sandbox);
    sandbox.factory(&["bar"]);
    sandbox.factory(&["status"]);
    quota_agy(&sandbox);

    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));

    // A review asks on its own cache, which the daemon's display never fills, and keeps it for
    // 120 s: a second review close behind asks nothing, and one after them asks again.
    let first = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t1"]);
    let second = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t2"]);
    assert_eq!((first.code, second.code), (0, 0), "{first:?} {second:?}");
    assert_eq!(usage_queries(&sandbox), 2, "{:?}", sandbox.calls("agy"));
    let cache = sandbox.path("state/factory/agy-usage.json");
    let stale = SystemTime::now() - Duration::from_secs(121);
    fs::File::options()
        .write(true)
        .open(&cache)
        .and_then(|file| file.set_modified(stale))
        .expect("age the routing cache");
    let third = sandbox.factory_in(&repo, &["review", "--standalone", "main", "t3"]);
    assert_eq!(third.code, 0, "{third:?}");
    assert_eq!(usage_queries(&sandbox), 3, "{:?}", sandbox.calls("agy"));

    bar_json(&sandbox);
    assert_eq!(
        usage_queries(&sandbox),
        3,
        "the bar's reading is still fresh"
    );
}

/// A fake agy whose usage query waits for `gate` to exist before it answers with `answer`.
fn agy_says_when(gate: &std::path::Path, answer: &str) -> String {
    format!(
        "until [ -e '{}' ]; do sleep 0.05; done\n{}",
        gate.display(),
        agy_says(answer)
    )
}

#[test]
fn a_slow_usage_query_is_asked_once_and_holds_no_bar_up() {
    let sandbox = Sandbox::new();
    let gate = sandbox.path("agy-may-answer");
    sandbox.fake("agy", &agy_says_when(&gate, &two_groups()));
    start(&sandbox);

    let begun = Instant::now();
    let first = bar_json(&sandbox);
    let waited = begun.elapsed();
    let second = bar_json(&sandbox);

    // The bar gave up on the query after a few seconds, not its ten, and showed no reading.
    assert!(waited < Duration::from_secs(8), "the bar waited {waited:?}");
    assert_eq!(agy_chip(&first)["text"], "?", "{first}");
    assert_eq!(agy_chip(&second)["text"], "?", "{second}");
    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));

    // The query finishes on its own once agy answers: the next look has it, and asks nothing.
    fs::write(&gate, "").expect("let agy answer");
    assert!(
        until_it(|| agy_chip(&bar_json(&sandbox))["text"] == "63%"),
        "the late answer never landed"
    );
    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));
}

#[test]
fn a_pool_held_at_the_configs_mark_is_a_critical_bar_chip() {
    let sandbox = Sandbox::new();
    sandbox.fake("agy", &agy_says(&two_groups()));
    configure(&sandbox, "[agy]\nhold_at_percent = 40\n");
    start(&sandbox);

    // Claude and GPT's weekly 63% is past the 40% mark, where routing holds the pool.
    assert!(
        until_it(|| agy_chip(&bar_json(&sandbox))["text"] == "63%"),
        "no reading landed"
    );
    let chip = agy_chip(&bar_json(&sandbox));
    assert_eq!(chip["tone"], "critical", "{chip}");
    assert!(
        chip["tooltip"]
            .as_str()
            .is_some_and(|tip| tip.ends_with(" · held from 40%")),
        "{chip}"
    );
}

#[test]
fn a_reading_is_shown_for_five_minutes_and_asked_again_after() {
    let skew = Sandbox::new();
    let clock = skew.path("clock-skew");
    let sandbox = skew.env("FACTORY_CLOCK_SKEW_FILE", &clock);
    sandbox.fake("agy", &agy_says(&two_groups()));
    start(&sandbox);
    bar_json(&sandbox);
    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));

    fs::write(&clock, "299").expect("move the clock on");
    bar_json(&sandbox);
    quota_agy(&sandbox);
    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));

    fs::write(&clock, "301").expect("move the clock on");
    let out = quota_agy(&sandbox);
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(usage_queries(&sandbox), 2, "{:?}", sandbox.calls("agy"));
}

#[test]
fn a_failed_usage_query_is_not_repeated_for_five_minutes_either() {
    let skew = Sandbox::new();
    let clock = skew.path("clock-skew");
    let sandbox = skew.env("FACTORY_CLOCK_SKEW_FILE", &clock);
    sandbox.fake("agy", "exit 3");
    start(&sandbox);

    bar_json(&sandbox);
    sandbox.factory(&["status"]);
    let quota = quota_agy(&sandbox);
    assert_eq!(
        quota.stdout, "agy usage unreadable: exited 3\n",
        "{quota:?}"
    );
    assert_eq!(usage_queries(&sandbox), 1, "{:?}", sandbox.calls("agy"));

    fs::write(&clock, "301").expect("move the clock on");
    quota_agy(&sandbox);
    assert_eq!(usage_queries(&sandbox), 2, "{:?}", sandbox.calls("agy"));
}

#[test]
fn many_quota_groups_are_a_summary_line_naming_a_file_with_a_row_each() {
    let sandbox = Sandbox::new();
    let bucket = |window: &str, left: f64| json!({"window": window, "remaining_fraction": left, "reset_time": "2099-01-01T00:00:00Z"});
    let groups: Vec<Value> = (1..=12)
        .map(|n| json!({"name": format!("Quota group number {n} of the plan"), "buckets": [bucket("5h", 0.5), bucket("weekly", 0.9)]}))
        .collect();
    let answer = json!({"command": {"data": {"groups": groups}}}).to_string();
    sandbox.fake("agy", &agy_says(&answer));

    let out = quota_agy(&sandbox);

    assert_eq!(out.code, 0, "{out:?}");
    let listing = sandbox.path("state/factory/agy-quota.txt");
    assert_eq!(
        out.stdout,
        format!(
            "agy: 12 quota groups, worst window 50%; listing in {}\n",
            listing.display()
        ),
        "{out:?}"
    );
    assert!(out.stdout.chars().count() <= 200, "{out:?}");
    let rows = fs::read_to_string(&listing).expect("the listing");
    let rows: Vec<&str> = rows.lines().collect();
    assert_eq!(rows.len(), 12, "{rows:?}");
    assert_eq!(
        rows[0],
        "Quota group number 1 of the plan: 5h 50%, weekly 10%"
    );
    let json = sandbox.factory(&["quota", "agy", "--json"]);
    assert_eq!(
        shown(&json.stdout)["groups"].as_array().map(Vec::len),
        Some(12)
    );
}

#[test]
fn a_listing_that_cannot_be_written_is_an_error_while_json_still_has_every_group() {
    let sandbox = Sandbox::new();
    let bucket = |window: &str| json!({"window": window, "remaining_fraction": 0.5, "reset_time": "2099-01-01T00:00:00Z"});
    let groups: Vec<Value> = (1..=12)
        .map(|n| json!({"name": format!("Quota group number {n} of the plan"), "buckets": [bucket("5h"), bucket("weekly")]}))
        .collect();
    let answer = json!({"command": {"data": {"groups": groups}}}).to_string();
    sandbox.fake("agy", &agy_says(&answer));
    // A directory where the listing goes: nothing can be written there.
    fs::create_dir_all(sandbox.path("state/factory/agy-quota.txt")).expect("block the listing");

    let out = quota_agy(&sandbox);

    assert_ne!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout, "", "{out:?}");
    assert!(out.stderr.contains("cannot write"), "{out:?}");
    assert!(out.stderr.contains("use --json"), "{out:?}");
    let json = sandbox.factory(&["quota", "agy", "--json"]);
    assert_eq!(json.code, 0, "{json:?}");
    assert_eq!(
        shown(&json.stdout)["groups"].as_array().map(Vec::len),
        Some(12)
    );
}

#[test]
fn a_group_name_with_a_line_break_is_still_one_line() {
    let sandbox = Sandbox::new();
    let answer = two_groups().replace("Gemini Models", "Gemini\\nModels");
    sandbox.fake("agy", &agy_says(&answer));

    let out = quota_agy(&sandbox);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    assert!(
        out.stdout.starts_with("agy: Gemini Models: 5h 30%"),
        "{out:?}"
    );
}

/// The daemon's log.
fn daemon_log(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.path("run/factory/daemon.log")).unwrap_or_default()
}

/// A sandbox whose PATH is its fake tools alone, with `missing` taken off it: agy's write guard
/// needs bwrap and xdg-dbus-proxy, and the host's own would show through /usr/bin.
// cfg(test), as the harness is, so clippy lets it unwrap.
#[cfg(test)]
fn without(missing: &str) -> Sandbox {
    let sandbox = Sandbox::new();
    let bin = sandbox.path("bin");
    let sandbox = sandbox.env("PATH", &bin);
    fs::remove_file(bin.join(missing)).expect("remove a fake tool");
    sandbox
}

#[test]
fn a_daemon_without_xdg_dbus_proxy_says_agy_reviews_are_skipped_in_log_status_and_bar() {
    let sandbox = without("xdg-dbus-proxy");

    let status = sandbox.factory(&["status"]);

    assert_eq!(status.code, 0, "{status:?}");
    let why = "agy reviews skipped, missing: xdg-dbus-proxy (package xdg-dbus-proxy)";
    assert!(status.stdout_lines().contains(&why), "{status:?}");
    let log = daemon_log(&sandbox);
    assert_eq!(log.matches(why).count(), 1, "{log}");
    sandbox.factory(&["status"]);
    assert_eq!(
        daemon_log(&sandbox).matches(why).count(),
        1,
        "said once, at start"
    );
    let bar = sandbox.factory(&["bar", "--json"]);
    let feed: Value = serde_json::from_str(&bar.stdout).expect("the bar feed is JSON");
    let chip = feed["glance"]
        .as_array()
        .and_then(|chips| chips.iter().find(|chip| chip["id"] == "agy"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(chip["tone"], "critical", "{chip}");
    assert_eq!(chip["tooltip"], why, "{chip}");
}

#[test]
fn a_daemon_with_both_guard_tools_says_nothing_of_them() {
    let sandbox = Sandbox::new();

    let status = sandbox.factory(&["status"]);

    assert_eq!(status.code, 0, "{status:?}");
    assert!(!status.stdout.contains("agy reviews skipped"), "{status:?}");
    assert!(!daemon_log(&sandbox).contains("agy reviews skipped"));
}
