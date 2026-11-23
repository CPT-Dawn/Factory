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
fn values_that_look_numeric_become_numbers_and_the_rest_stay_strings() {
    let cases = [
        ("round=1", r#""round":1"#),
        ("n=007", r#""n":7"#),
        ("neg=-3", r#""neg":-3"#),
        ("dur=1.50", r#""dur":1.5"#),
        ("two=2.0", r#""two":2"#),
        ("head=612ee246", r#""head":"612ee246""#),
        ("plus=+5", r#""plus":"+5""#),
        ("exp=1e3", r#""exp":"1e3""#),
        ("half=.5", r#""half":".5""#),
        ("trail=5.", r#""trail":"5.""#),
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
        r#"{"ts":"2026-11-20T02:47:18.365Z","type":"lead-end","a":2,"c":"x"}"#
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
