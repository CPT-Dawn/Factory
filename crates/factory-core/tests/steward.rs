//! The context steward's rules: what a session-log line says about a session's context, and
//! what the steward does with a lead over the threshold.

use std::time::{Duration, Instant};

use factory_core::steward::{LeadNow, Policy, Reading, Step, reading};

const POLICY: Policy = Policy {
    ask_at: 300_000,
    grace: Duration::from_secs(120),
    subagent_at: 250_000,
};

#[test]
fn a_session_log_line_gives_the_context_its_request_carried() {
    let main = r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":509,"cache_read_input_tokens":18163,"output_tokens":141}}}"#;
    let side = r#"{"type":"assistant","isSidechain":true,"agentId":"a1","message":{"usage":{"input_tokens":7,"cache_read_input_tokens":300000}}}"#;
    let cases = [
        (main, Some((18674, false))),
        (side, Some((300_007, true))),
        // Only the input counts: output tokens are not context yet.
        (
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":5000}}}"#,
            Some((10, false)),
        ),
        // A line without usage, an empty usage and a line that isn't JSON say nothing.
        (r#"{"type":"user","message":{"content":"hi"}}"#, None),
        (
            r#"{"type":"assistant","message":{"usage":{"input_tokens":0}}}"#,
            None,
        ),
        (r#"{"type":"queue-operation","sessionId":"s1"}"#, None),
        ("not json", None),
        ("", None),
    ];
    for (line, expected) in cases {
        let expected = expected.map(|(tokens, sidechain)| Reading { tokens, sidechain });
        assert_eq!(reading(line), expected, "{line}");
    }
}

#[test]
fn an_idle_lead_over_the_threshold_is_asked_once_then_compacted_after_the_grace_period() {
    let start = Instant::now();
    let now = start + Duration::from_secs(600);
    let ago = |secs: u64| Some(now - Duration::from_secs(secs));
    let over = Some(300_000);
    let under = Some(299_999);
    let cases = [
        // tokens, idle since, waiting, asked at, step
        (None, ago(0), false, None, Step::Nothing),
        (under, ago(0), false, None, Step::Nothing),
        // Back under the threshold, an asked lead is asked again next time it passes.
        (under, ago(0), false, ago(60), Step::Unask),
        (under, None, true, ago(60), Step::Unask),
        (over, ago(0), false, None, Step::Ask),
        (over, ago(500), false, None, Step::Ask),
        // A working lead is left alone, asked or not.
        (over, None, false, None, Step::Nothing),
        (over, None, false, ago(300), Step::Nothing),
        // Within the grace period, the steward looks again when it ends.
        (
            over,
            ago(90),
            false,
            ago(60),
            Step::At(now + Duration::from_secs(60)),
        ),
        (
            over,
            ago(30),
            false,
            ago(300),
            Step::At(now + Duration::from_secs(90)),
        ),
        (over, ago(300), false, ago(180), Step::Compact),
        (over, ago(120), false, ago(120), Step::Compact),
        // A lead with a registered wait is never asked or compacted.
        (over, ago(0), true, None, Step::Nothing),
        (over, ago(500), true, ago(400), Step::Nothing),
    ];
    for (tokens, idle_since, waiting, asked_at, expected) in cases {
        let lead = LeadNow {
            tokens,
            idle_since,
            waiting,
            asked_at,
        };
        assert_eq!(POLICY.step(&lead, now), expected, "{lead:?}");
    }
}

#[test]
fn a_subagent_over_its_threshold_is_told_about_once() {
    let cases = [
        // tokens, told already, tell
        (249_999, false, false),
        (250_000, false, true),
        (400_000, true, false),
    ];
    for (tokens, told, expected) in cases {
        assert_eq!(POLICY.tells(tokens, told), expected, "{tokens} told={told}");
    }
}
