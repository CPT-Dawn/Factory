//! Ledger events: what `factory log <type> key=value…` appends.
//!
//! The format is the one factory-stats has always written and reads: one compact JSON object
//! per line, `ts` first, then `type`, then the fields in the order given. In an event a writer
//! logs ([`Event::from_args`], [`Event::from_fields`]) only the fields in [`NUMERIC_FIELDS`] hold
//! numbers; every other value is a string. The daemon's own events, built with [`Event::new`],
//! type their fields themselves.

use std::fmt;

use serde_json::{Map, Number, Value};

use crate::timestamp::Timestamp;

/// An event's fields, in the order given.
pub type Fields = Map<String, Value>;

/// An event before the daemon stamps it with the moment it lands in the ledger.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    kind: String,
    fields: Fields,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventError {
    /// The type isn't lowercase letters and dashes starting with a letter.
    Type(String),
    /// An argument that isn't `key=value` with a non-empty key.
    Pair(String),
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventError::Type(kind) => write!(
                f,
                "event type must be lowercase letters and dashes, starting with a letter, got: {kind:?}"
            ),
            EventError::Pair(arg) => write!(f, "expected key=value, got: {arg}"),
        }
    }
}

impl std::error::Error for EventError {}

impl Event {
    /// An event of type `kind` holding `fields`.
    pub fn new(kind: &str, fields: Fields) -> Result<Event, EventError> {
        if !is_event_type(kind) {
            return Err(EventError::Type(kind.to_owned()));
        }
        if let Some(value) = fields.get("") {
            return Err(EventError::Pair(format!("={value}")));
        }
        Ok(Event {
            kind: kind.to_owned(),
            fields,
        })
    }

    /// The event a writer that types its own values sends the daemon (factory-stats sends every
    /// value of digits as a number), with each field as the ledger keeps it: a number outside
    /// [`NUMERIC_FIELDS`] goes back to its text, and a string of digits in one becomes the number.
    pub fn from_fields(kind: &str, fields: Fields) -> Result<Event, EventError> {
        let fields = fields
            .into_iter()
            .map(|(key, value)| {
                let value = typed(&key, value);
                (key, value)
            })
            .collect();
        Event::new(kind, fields)
    }

    /// The event `factory log <kind> <pairs>…` describes, with `session` recorded last
    /// unless a pair sets it.
    pub fn from_args<S: AsRef<str>>(
        kind: &str,
        pairs: &[S],
        session: Option<&str>,
    ) -> Result<Event, EventError> {
        let mut fields = parse_pairs(pairs)?;
        if let Some(session) = session {
            fields
                .entry("session")
                .or_insert_with(|| Value::String(session.to_owned()));
        }
        Event::new(kind, fields)
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn fields(&self) -> &Fields {
        &self.fields
    }

    /// The ledger object for this event landing at `ts`: `ts` and `type`, then the fields.
    /// A field named `ts` or `type` replaces that value in place, as in factory-stats.
    pub fn stamped(&self, ts: Timestamp) -> Fields {
        let mut out = Fields::new();
        out.insert("ts".to_owned(), Value::String(ts.to_string()));
        out.insert("type".to_owned(), Value::String(self.kind.clone()));
        out.extend(self.fields.clone());
        out
    }
}

/// `^[a-z][a-z-]*$`, the types factory-stats accepts.
fn is_event_type(kind: &str) -> bool {
    let mut chars = kind.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Parses `key=value` arguments as factory-stats does: split at the first `=`.
fn parse_pairs<S: AsRef<str>>(pairs: &[S]) -> Result<Fields, EventError> {
    let mut fields = Fields::new();
    for pair in pairs {
        let pair = pair.as_ref();
        let Some((key, value)) = pair.split_once('=').filter(|(key, _)| !key.is_empty()) else {
            return Err(EventError::Pair(pair.to_owned()));
        };
        if !value.is_empty() {
            fields.insert(key.to_owned(), field_value(key, value));
        }
    }
    Ok(fields)
}

/// The fields the ledger keeps as numbers: the counts and measures factory-stats adds up
/// (`round`, `duration_s`, `tokens` and the finding counts) and the other counts its lines
/// have always held as numbers. Every other field keeps the text it was given, so a sha of
/// digits like `0475004` stays a sha.
pub const NUMERIC_FIELDS: [&str; 32] = [
    "round",
    "duration_s",
    "tokens",
    "standards_hard",
    "standards_judgement",
    "spec_findings",
    "spec_advisory",
    "questions",
    "fixed",
    "still_present",
    "bundle_bytes",
    "stalls",
    "blocking",
    "advisory",
    "question",
    "dropped",
    // The lab's events (#62): its ids, durations and counts.
    "lease",
    "slot",
    "reservation",
    "waiter",
    "seconds",
    "idle_seconds",
    "idle_minutes",
    "count",
    "cap",
    "busy",
    "locked",
    "asleep",
    "stay_awake_off",
    "battery_faked",
    "offline",
    "unauthorized",
];

/// `value` as the ledger keeps field `key`.
fn typed(key: &str, value: Value) -> Value {
    match value {
        Value::Number(n) if !is_numeric_field(key) => Value::String(n.to_string()),
        Value::String(text) => field_value(key, &text),
        other => other,
    }
}

/// Whether the ledger keeps the field `key` as a number.
pub fn is_numeric_field(key: &str) -> bool {
    NUMERIC_FIELDS.contains(&key)
}

/// The value of field `key`: in a numeric field, a value matching `^-?\d+(\.\d+)?$` becomes a
/// number, as JavaScript's `Number` reads it (`007` is 7, `2.0` is 2); anything else stays a
/// string.
fn field_value(key: &str, value: &str) -> Value {
    let number = (is_numeric_field(key) && is_decimal(value))
        .then(|| value.parse::<f64>().ok())
        .flatten()
        .and_then(js_number);
    number.map_or_else(|| Value::String(value.to_owned()), Value::Number)
}

fn is_decimal(value: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    match unsigned.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(unsigned),
    }
}

/// The number JavaScript's `JSON.stringify` would show: integral values without a fraction.
fn js_number(n: f64) -> Option<Number> {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53: every integer up to it is exact.
    if n.fract() == 0.0 && n.abs() <= EXACT {
        return Some(Number::from(n as i64));
    }
    Number::from_f64(n)
}
