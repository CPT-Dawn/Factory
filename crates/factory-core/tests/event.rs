//! Events follow factory-stats' `log` rules, so old and new writers land the same lines.

use factory_core::event::{Event, EventError};
use factory_core::timestamp::Timestamp;
use serde_json::Value;

const TS: Timestamp = Timestamp::from_unix_millis(1_795_142_838_365);

fn line(kind: &str, pairs: &[&str], session: Option<&str>) -> String {
    let event = match Event::from_args(kind, pairs, session) {
        Ok(event) => event,
        Err(e) => panic!("{kind} {pairs:?}: {e}"),
    };
    Value::Object(event.stamped(TS)).to_string()
}

#[test]
fn a_count_or_a_measure_that_looks_numeric_becomes_a_number() {
    let cases = [
        ("round=1", r#""round":1"#),
        ("round=007", r#""round":7"#),
        ("fixed=-3", r#""fixed":-3"#),
        ("duration_s=1.50", r#""duration_s":1.5"#),
        ("tokens=2.0", r#""tokens":2"#),
        ("standards_hard=2", r#""standards_hard":2"#),
        ("spec_findings=0", r#""spec_findings":0"#),
        ("question=1", r#""question":1"#),
        ("bundle_bytes=1334262", r#""bundle_bytes":1334262"#),
        ("round=+5", r#""round":"+5""#),
        ("round=1e3", r#""round":"1e3""#),
        ("round=.5", r#""round":".5""#),
        ("round=5.", r#""round":"5.""#),
    ];
    for (pair, field) in cases {
        let expected = format!(r#"{{"ts":"2026-11-20T02:47:18.365Z","type":"review",{field}}}"#);
        assert_eq!(line("review", &[pair], None), expected, "{pair}");
    }
}

#[test]
fn any_other_value_stays_the_string_it_was_given() {
    let cases = [
        ("head=4750048", r#""head":"4750048""#),
        ("head=0475004", r#""head":"0475004""#),
        ("from=57334235", r#""from":"57334235""#),
        ("label=3", r#""label":"3""#),
        ("slug=42", r#""slug":"42""#),
        ("n=007", r#""n":"007""#),
        ("dur=1.50", r#""dur":"1.50""#),
        ("head=612ee246", r#""head":"612ee246""#),
        ("tags=a,b", r#""tags":"a,b""#),
        ("eq=a=b", r#""eq":"a=b""#),
    ];
    for (pair, field) in cases {
        let expected = format!(r#"{{"ts":"2026-11-20T02:47:18.365Z","type":"review",{field}}}"#);
        assert_eq!(line("review", &[pair], None), expected, "{pair}");
    }
}

#[test]
fn an_empty_value_is_dropped_and_a_later_duplicate_wins_in_place() {
    assert_eq!(
        line("lead-end", &["a=1", "b=", "c=x", "a=2", "c="], None),
        r#"{"ts":"2026-11-20T02:47:18.365Z","type":"lead-end","a":"2","c":"x"}"#
    );
}

#[test]
fn the_session_goes_last_unless_a_pair_names_it() {
    assert_eq!(
        line("standin", &["repo=X"], Some("s-1")),
        r#"{"ts":"2026-11-20T02:47:18.365Z","type":"standin","repo":"X","session":"s-1"}"#
    );
    assert_eq!(
        line("standin", &["session=mine", "repo=X"], Some("s-1")),
        r#"{"ts":"2026-11-20T02:47:18.365Z","type":"standin","session":"mine","repo":"X"}"#
    );
}

#[test]
fn a_pair_named_ts_replaces_the_stamp_in_place() {
    assert_eq!(
        line("review", &["repo=X", "ts=then"], None),
        r#"{"ts":"then","type":"review","repo":"X"}"#
    );
}

#[test]
fn an_argument_that_is_not_key_value_is_refused() {
    for arg in ["repo", "=x", ""] {
        assert_eq!(
            Event::from_args("review", &["ok=1", arg], None),
            Err(EventError::Pair(arg.to_owned())),
            "{arg:?}"
        );
    }
    assert_eq!(
        EventError::Pair("repo".to_owned()).to_string(),
        "expected key=value, got: repo"
    );
}

#[test]
fn a_type_must_be_lowercase_letters_and_dashes() {
    for kind in ["review", "lead-end", "a-"] {
        assert!(
            Event::from_args(kind, &[] as &[&str], None).is_ok(),
            "{kind}"
        );
    }
    for kind in ["", "Review", "lead_end", "-a", "r2"] {
        assert_eq!(
            Event::from_args(kind, &[] as &[&str], None),
            Err(EventError::Type(kind.to_owned())),
            "{kind:?}"
        );
    }
}
