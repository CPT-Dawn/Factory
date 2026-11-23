//! `factory review` across backends: the configured order, quota cooldowns that outlive the
//! daemon, the fallback from Codex to Claude, and each backend's concurrency limit.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};

use crate::support::{self, Sandbox, world};

pub(crate) const CLEAN_REPORT: &str = r#"## Standards

Nothing.

## Findings ledger

```json
{"findings":[]}
```

FINDINGS: standards_hard=0 standards_judgement=0 spec_findings=0 spec_advisory=0 questions=0 fixed=0 still_present=0 tags=
VERDICT: clean
"#;

/// A fake `codex` body that hits its usage limit, naming `reset` ("12:39 PM") as Codex does.
pub(crate) fn codex_out_of_quota(reset: &str) -> String {
    let message = format!(
        "You\u{2019}ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at {reset}."
    );
    let error = json!({"type": "error", "message": message});
    let failed = json!({"type": "turn.failed", "error": {"message": message}});
    format!("cat > \"$0.stdin\"\ncat <<'STREAM'\n{error}\n{failed}\nSTREAM\nexit 1")
}

/// A fake `claude` body that keeps its prompt as `bin/claude.stdin` and prints `report` as
/// the result of `--output-format json`.
pub(crate) fn claude_writes(report: &str) -> String {
    let result =
        json!({"type": "result", "subtype": "success", "is_error": false, "result": report});
    format!("cat > \"$0.stdin\"\ncat <<'RESULT'\n{result}\nRESULT")
}

fn now_seconds() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    i64::try_from(since).unwrap_or(i64::MAX)
}

/// A minute about two hours from now, as Codex names it on a 12-hour UTC clock ("2:39 PM"),
/// and as the moment it is.
pub(crate) fn reset_in_two_hours() -> (String, Timestamp) {
    let at = (now_seconds() + 7200) / 60 * 60;
    let (hour, minute) = (at / 3600 % 24, at / 60 % 60);
    let half = if hour < 12 { "AM" } else { "PM" };
    let shown = format!("{}:{minute:02} {half}", (hour + 11) % 12 + 1);
    (shown, Timestamp::from_unix_millis(at * 1000))
}

fn event(sandbox: &Sandbox, index: usize) -> Value {
    let lines = sandbox.ledger_lines();
    serde_json::from_str(&lines[index]).unwrap_or(Value::Null)
}

#[test]
fn a_codex_usage_limit_cools_codex_down_to_its_reset_and_reruns_the_review_on_claude() {
    let sandbox = Sandbox::new().env("TZ", "UTC");
    let repo = sandbox.repo();
    let (reset, until) = reset_in_two_hours();
    sandbox.fake("codex", &codex_out_of_quota(&reset));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let shown: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(shown["verdict"], "clean", "{shown}");
    assert_eq!(shown["backend"], "claude", "{shown}");
    assert_eq!(
        shown["cooled"],
        json!([{"backend": "codex", "until": until.to_string()}]),
        "{shown}"
    );
    let reviews = sandbox.reviews();
    assert_eq!(
        fs::read_to_string(reviews.join("t1-r1.md")).unwrap(),
        CLEAN_REPORT
    );
    assert!(
        fs::read_to_string(reviews.join("t1-r1.jsonl"))
            .unwrap()
            .contains("usage limit"),
        "Codex's stream is kept"
    );
    let codex_prompt = fs::read_to_string(sandbox.path("bin/codex.stdin")).unwrap();
    assert!(
        codex_prompt.starts_with("$code-review since main.\n"),
        "{codex_prompt}"
    );
    let claude_prompt = fs::read_to_string(sandbox.path("bin/claude.stdin")).unwrap();
    assert!(
        claude_prompt.starts_with("Review the changes since main.\n"),
        "{claude_prompt}"
    );
    let logged = event(&sandbox, 0);
    assert_eq!(logged["backend"], "claude", "{logged}");
    assert_eq!(logged["model"], "opus", "{logged}");
    assert_eq!(logged["effort"], "high", "{logged}");
    assert_eq!(logged["fallback_from"], "codex", "{logged}");
    assert_eq!(logged["outcome"], "ok", "{logged}");

    let later = sandbox.factory_in(&repo, &["review", "main", "t2-r1"]);

    assert_eq!(later.code, 0, "{later:?}");
    assert_eq!(
        sandbox.calls("codex").len(),
        1,
        "a later review skips Codex"
    );
    assert_eq!(sandbox.calls("claude").len(), 2);
}

/// Writes the factory config, `$XDG_CONFIG_HOME/factory/config.toml`.
pub(crate) fn configure(sandbox: &Sandbox, toml: &str) {
    let dir = sandbox.path("config/factory");
    fs::create_dir_all(&dir).unwrap_or_default();
    fs::write(dir.join("config.toml"), toml).unwrap_or_default();
}

/// The value that follows `flag` in `argv`.
fn after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let at = argv.iter().position(|arg| arg == flag)?;
    argv.get(at + 1).map(String::as_str)
}

#[test]
fn claude_reviews_with_user_settings_only_and_no_plugins_or_mcp_servers() {
    let sandbox = Sandbox::new().env("CLAUDE_CODE_PLUGIN_DIRS", "/tmp/somewhere");
    let repo = sandbox.repo();
    configure(&sandbox, "[[review.backend]]\ncli = \"claude\"\n");
    fs::create_dir_all(sandbox.path("home/.claude")).unwrap();
    fs::write(
        sandbox.path("home/.claude/settings.json"),
        r#"{"enabledPlugins": {"helper@market": true, "idle@market": false}, "model": "opus"}"#,
    )
    .unwrap();
    sandbox.fake(
        "claude",
        &format!(
            "printf '%s' \"${{CLAUDE_CODE_PLUGIN_DIRS-unset}}\" > \"$0.plugin-dirs\"\n{}",
            claude_writes(CLEAN_REPORT)
        ),
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let calls = sandbox.calls("claude");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let argv = &calls[0];
    assert_eq!(after(argv, "--setting-sources"), Some("user"), "{argv:?}");
    assert!(
        argv.iter().any(|arg| arg == "--strict-mcp-config"),
        "{argv:?}"
    );
    for absent in ["--mcp-config", "--plugin-dir", "--bare"] {
        assert!(
            !argv.iter().any(|arg| arg == absent),
            "{absent} in {argv:?}"
        );
    }
    let settings: Value = serde_json::from_str(after(argv, "--settings").unwrap()).unwrap();
    assert_eq!(
        settings["enabledPlugins"]["helper@market"], false,
        "{settings}"
    );
    assert_eq!(
        settings["enabledPlugins"]["idle@market"], false,
        "{settings}"
    );
    assert_eq!(
        fs::read_to_string(sandbox.path("bin/claude.plugin-dirs")).unwrap(),
        "unset"
    );
}

#[test]
fn the_config_file_sets_the_backend_order_model_and_effort() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"claude\"\nmodel = \"sonnet\"\neffort = \"medium\"\n\n\
         [[review.backend]]\ncli = \"codex\"\n",
    );
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(sandbox.calls("codex").is_empty(), "claude comes first");
    let argv = &sandbox.calls("claude")[0];
    assert_eq!(after(argv, "--model"), Some("sonnet"), "{argv:?}");
    assert_eq!(after(argv, "--effort"), Some("medium"), "{argv:?}");
    let logged = event(&sandbox, 0);
    assert_eq!(logged["backend"], "claude", "{logged}");
    assert_eq!(logged["model"], "sonnet", "{logged}");
    assert_eq!(logged["effort"], "medium", "{logged}");
}

#[test]
fn the_config_file_sets_codexs_model_effort_and_timeout() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"codex\"\nmodel = \"gpt-test\"\neffort = \"low\"\ntimeout = 1\n",
    );
    sandbox.fake("codex", "exec sleep 30");

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 124, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: codex timed out after 1s"),
        "{out:?}"
    );
    let argv = &sandbox.calls("codex")[0];
    assert_eq!(after(argv, "-m"), Some("gpt-test"), "{argv:?}");
    assert_eq!(
        after(argv, "-c"),
        Some("model_reasoning_effort=\"low\""),
        "{argv:?}"
    );
}

#[test]
fn a_broken_config_is_refused_in_one_line_naming_the_file() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"claude\"\nconcurrency = 0\n",
    );

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert!(out.stderr.contains("config.toml"), "{out:?}");
    assert!(sandbox.calls("claude").is_empty());
}

/// A fake `codex` body that notes `start` in `bin/codex.log`, waits for a line on the fifo
/// `gate`, notes `end`, and writes `report`.
fn codex_waits_for(gate: &std::path::Path, report: &str) -> String {
    format!(
        "out=\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\n\
         cat > /dev/null\n\
         echo start >> \"$0.log\"\n\
         read go < '{gate}'\n\
         echo end >> \"$0.log\"\n\
         cat > \"$out\" <<'REPORT'\n{report}REPORT",
        gate = gate.display()
    )
}

fn gate(sandbox: &Sandbox) -> std::path::PathBuf {
    let gate = sandbox.path("gate");
    let made = std::process::Command::new("mkfifo").arg(&gate).status();
    assert!(made.is_ok_and(|status| status.success()), "mkfifo {gate:?}");
    gate
}

/// Lets one waiting fake through `gate`, blocking until one opens it. Open it again only
/// once the next fake waits, as the last one may still hold it open.
fn open(gate: &std::path::Path) {
    fs::write(gate, "go\n").unwrap_or_default();
}

fn codex_log(sandbox: &Sandbox) -> Vec<String> {
    support::lines_of(&sandbox.path("bin/codex.log"))
}

/// Whether process `pid` has a socket open: a review's CLI has one only while it talks to
/// the daemon, which is where it waits for a slot.
fn holds_a_socket(pid: u32) -> bool {
    fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
        fds.flatten().any(|fd| {
            fs::read_link(fd.path()).is_ok_and(|to| to.to_string_lossy().starts_with("socket:"))
        })
    })
}

/// How many connections on `socket` the daemon `pid` has accepted and still holds.
fn connections_taken(pid: u32, socket: &Path) -> usize {
    let held: Vec<String> = fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|fds| {
            fds.flatten()
                .filter_map(|fd| fs::read_link(fd.path()).ok())
                .filter_map(|to| {
                    let to = to.to_string_lossy().into_owned();
                    Some(to.strip_prefix("socket:[")?.strip_suffix(']')?.to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    let path = socket.to_string_lossy();
    fs::read_to_string("/proc/net/unix")
        .unwrap_or_default()
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        // Num RefCount Protocol Flags Type St Inode Path; St 03 is connected.
        .filter(|fields| fields.len() == 8 && fields[5] == "03" && fields[7] == path)
        .filter(|fields| held.iter().any(|inode| inode == fields[6]))
        .count()
}

const ONE_CODEX_THEN_CLAUDE: &str = "[[review.backend]]\ncli = \"codex\"\nconcurrency = 1\nwhen_busy = \"WHEN\"\n\n\
     [[review.backend]]\ncli = \"claude\"\n";

#[test]
fn a_review_finding_its_backend_full_takes_the_next_one_when_configured_to() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(&sandbox, &ONE_CODEX_THEN_CLAUDE.replace("WHEN", "next"));
    let gate = gate(&sandbox);
    sandbox.fake("codex", &codex_waits_for(&gate, CLEAN_REPORT));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));
    let mut first = sandbox.factory_command(&["review", "main", "t1", "--json"]);
    first.current_dir(&repo);
    let first = support::spawn(first);
    assert!(
        world::poll_until(|| codex_log(&sandbox).len() == 1),
        "codex never started"
    );

    let second = sandbox.factory_in(&repo, &["review", "main", "t2", "--json"]);

    assert_eq!(second.code, 0, "{second:?}");
    let shown: Value = serde_json::from_str(&second.stdout).unwrap();
    assert_eq!(shown["backend"], "claude", "{shown}");
    open(&gate);
    let first = support::finish(first);
    assert_eq!(first.code, 0, "{first:?}");
    let shown: Value = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(shown["backend"], "codex", "{shown}");
    assert_eq!(sandbox.calls("codex").len(), 1);
}

#[test]
fn a_review_finding_its_backend_full_queues_for_it_when_configured_to() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(&sandbox, &ONE_CODEX_THEN_CLAUDE.replace("WHEN", "queue"));
    let gate = gate(&sandbox);
    sandbox.fake("codex", &codex_waits_for(&gate, CLEAN_REPORT));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));
    let review = |label: &str| {
        let mut cmd = sandbox.factory_command(&["review", "main", label, "--json"]);
        cmd.current_dir(&repo);
        support::spawn(cmd)
    };
    let first = review("t1");
    assert!(
        world::poll_until(|| codex_log(&sandbox).len() == 1),
        "codex never started"
    );
    let second = review("t2");
    let queued = second.id();
    assert!(
        world::poll_until(|| holds_a_socket(queued)),
        "t2 never asked for a slot"
    );
    assert_eq!(
        codex_log(&sandbox),
        ["start"],
        "t2 waits while t1 holds codex"
    );

    open(&gate);
    assert!(
        world::poll_until(|| codex_log(&sandbox).len() == 3),
        "t2 never got codex: {:?}",
        codex_log(&sandbox)
    );
    open(&gate);
    let (first, second) = (support::finish(first), support::finish(second));

    for run in [&first, &second] {
        assert_eq!(run.code, 0, "{run:?}");
        let shown: Value = serde_json::from_str(&run.stdout).unwrap();
        assert_eq!(shown["backend"], "codex", "{shown}");
    }
    assert_eq!(codex_log(&sandbox), ["start", "end", "start", "end"]);
    assert!(sandbox.calls("claude").is_empty());
}

/// Kills the daemon the way a crash would, and waits until its socket stops answering.
fn kill_daemon(sandbox: &Sandbox) {
    let pid = fs::read_to_string(sandbox.path("run/factory/daemon.pid")).unwrap_or_default();
    let pid: u32 = pid.trim().parse().unwrap_or(0);
    assert!(pid > 0, "no daemon pid");
    support::kill_group(pid, rustix::process::Signal::KILL);
    let socket = sandbox.path("run/factory/factory.sock");
    assert!(
        world::poll_until(|| std::os::unix::net::UnixStream::connect(&socket).is_err()),
        "the daemon outlived SIGKILL"
    );
}

#[test]
fn a_cooldown_outlives_a_daemon_restart() {
    let sandbox = Sandbox::new().env("TZ", "UTC");
    let repo = sandbox.repo();
    let (reset, _) = reset_in_two_hours();
    sandbox.fake("codex", &codex_out_of_quota(&reset));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));
    let first = sandbox.factory_in(&repo, &["review", "main", "t1"]);
    assert_eq!(first.code, 0, "{first:?}");

    kill_daemon(&sandbox);
    let later = sandbox.factory_in(&repo, &["review", "main", "t2", "--json"]);

    assert_eq!(later.code, 0, "{later:?}");
    let shown: Value = serde_json::from_str(&later.stdout).unwrap();
    assert_eq!(shown["backend"], "claude", "{shown}");
    assert_eq!(
        sandbox.calls("codex").len(),
        1,
        "codex is still cooling down"
    );
}

#[test]
fn every_backend_out_of_quota_exits_75_naming_when_to_retry() {
    let sandbox = Sandbox::new().env("TZ", "UTC");
    let repo = sandbox.repo();
    configure(&sandbox, "[[review.backend]]\ncli = \"codex\"\n");
    let (reset, until) = reset_in_two_hours();
    sandbox.fake("codex", &codex_out_of_quota(&reset));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 75, "{out:?}");
    assert_eq!(out.stderr_lines().len(), 1, "{out:?}");
    assert_eq!(
        out.stderr,
        format!(
            "factory: every review backend is out of quota (codex until {until}); retry after {until}\n"
        )
    );
    let logged = event(&sandbox, 0);
    assert_eq!(logged["outcome"], "error", "{logged}");
    let again = sandbox.factory_in(&repo, &["review", "main", "t2"]);
    assert_eq!(again.code, 75, "{again:?}");
    assert_eq!(
        sandbox.calls("codex").len(),
        1,
        "a cooling backend is not run"
    );
}

#[test]
fn what_the_config_leaves_unset_is_left_to_codexs_own_config() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"codex\"\nmodel = \"gpt-test\"\n",
    );
    fs::create_dir_all(sandbox.path("home/.codex")).unwrap();
    fs::write(
        sandbox.path("home/.codex/config.toml"),
        "model = \"gpt-own\"\nmodel_reasoning_effort = \"xhigh\"\n",
    )
    .unwrap();
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let argv = &sandbox.calls("codex")[0];
    assert_eq!(after(argv, "-m"), Some("gpt-test"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg == "-c"), "{argv:?}");
    let logged = event(&sandbox, 0);
    assert_eq!(logged["model"], "gpt-test", "{logged}");
    assert_eq!(logged["effort"], "xhigh", "{logged}");
}

/// A fake `codex` body that writes `report` at once.
fn codex_writes(report: &str) -> String {
    format!(
        "out=\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\n\
         cat > /dev/null\n\
         cat > \"$out\" <<'REPORT'\n{report}REPORT"
    )
}

#[test]
fn a_review_queued_when_the_daemon_stops_says_to_run_it_again() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(&sandbox, &ONE_CODEX_THEN_CLAUDE.replace("WHEN", "queue"));
    let gate = gate(&sandbox);
    sandbox.fake("codex", &codex_waits_for(&gate, CLEAN_REPORT));
    let review = |label: &str| {
        let mut cmd = sandbox.factory_command(&["review", "main", label]);
        cmd.current_dir(&repo);
        support::spawn(cmd)
    };
    let first = review("t1");
    assert!(
        world::poll_until(|| codex_log(&sandbox).len() == 1),
        "codex never started"
    );
    let pid: u32 = fs::read_to_string(sandbox.path("run/factory/daemon.pid"))
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    let socket = sandbox.path("run/factory/factory.sock");
    let second = review("t2");
    // Once the daemon has taken t2's connection, t2's acquire is this daemon's to answer.
    assert!(
        world::poll_until(|| connections_taken(pid, &socket) == 2),
        "the daemon never took t2's connection"
    );

    // As systemd stops it for a restart or an upgrade.
    support::kill_group(pid, rustix::process::Signal::TERM);
    let second = support::finish(second);

    assert_eq!(second.code, 3, "{second:?}");
    assert_eq!(
        second.stderr,
        "factory: the daemon stopped (a restart or an upgrade); run the review again\n"
    );
    open(&gate);
    support::finish(first);
}
