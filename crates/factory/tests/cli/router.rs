//! `factory review` across backends: the configured order, quota cooldowns that outlive the
//! daemon, the fallback from Codex to Claude, and each backend's concurrency limit.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// A fake `codex` body that hits its usage limit, naming `reset` ("12:39 PM") as Codex does.
fn codex_out_of_quota(reset: &str) -> String {
    let message = format!(
        "You\u{2019}ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at {reset}."
    );
    let error = json!({"type": "error", "message": message});
    let failed = json!({"type": "turn.failed", "error": {"message": message}});
    format!("cat > \"$0.stdin\"\ncat <<'STREAM'\n{error}\n{failed}\nSTREAM\nexit 1")
}

/// A fake `claude` body that keeps its prompt as `bin/claude.stdin` and prints `report` as
/// the result of `--output-format json`.
fn claude_writes(report: &str) -> String {
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
fn reset_in_two_hours() -> (String, Timestamp) {
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
