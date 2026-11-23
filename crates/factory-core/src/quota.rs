//! Reviewer quotas: the state machine each backend's quota follows, the router's pick of a
//! backend, and the readers that find a backend's out-of-quota signature and its reset time
//! (spec #1, "Reviewer router").

use serde_json::Value;

use crate::timestamp::Timestamp;

const MILLIS_PER_MINUTE: i64 = 60_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// How long a backend cools down when its out-of-quota message names no reset time.
const UNKNOWN_RESET_MILLIS: i64 = 3_600_000;

/// What is known of a backend's quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quota {
    /// Nothing yet, or its cooldown is over: worth a try.
    Unknown,
    /// Its last review finished.
    Available,
    /// It ran out and resets at `until`; the router skips it until then.
    CoolingDown { until: Timestamp },
}

impl Quota {
    /// The quota as it stands at `now`: a cooldown whose reset has come is unknown again.
    pub fn at(self, now: Timestamp) -> Quota {
        match self {
            Quota::CoolingDown { until } if until <= now => Quota::Unknown,
            quota => quota,
        }
    }

    /// What this quota becomes when a review reports `learned` at `now`. A review that finished
    /// says nothing against a cooldown another review hit meanwhile, while that cooldown holds.
    pub fn learn(self, learned: Quota, now: Timestamp) -> Quota {
        match (self.at(now), learned) {
            (Quota::CoolingDown { .. }, Quota::Available | Quota::Unknown) => self,
            _ => learned,
        }
    }
}

/// What a review does when a backend it could use has every slot taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhenBusy {
    /// Wait for a slot on it.
    Queue,
    /// Try the next backend.
    Next,
}

/// One configured backend, as the router sees it when a review asks for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub quota: Quota,
    /// Reviews running on it now.
    pub running: u32,
    /// The most reviews it may run at once.
    pub concurrency: u32,
    pub when_busy: WhenBusy,
}

/// The router's answer to a review asking for a backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Run on the candidate at this index.
    Take(usize),
    /// Every usable backend is busy: ask again when a slot frees.
    Wait,
    /// Every backend is cooling down; the first comes back at `until` (none when there are
    /// no backends).
    OutOfQuota { until: Option<Timestamp> },
}

/// Picks a backend for a review, trying `candidates` in their configured order: the first
/// with quota and a free slot, passing over a busy one only when it says [`WhenBusy::Next`].
pub fn pick(candidates: &[Candidate], now: Timestamp) -> Pick {
    let mut busy = false;
    for (index, candidate) in candidates.iter().enumerate() {
        if let Quota::CoolingDown { .. } = candidate.quota.at(now) {
            continue;
        }
        if candidate.running < candidate.concurrency {
            return Pick::Take(index);
        }
        match candidate.when_busy {
            WhenBusy::Queue => return Pick::Wait,
            WhenBusy::Next => busy = true,
        }
    }
    if busy {
        return Pick::Wait;
    }
    let until = candidates
        .iter()
        .filter_map(|candidate| match candidate.quota {
            Quota::CoolingDown { until } => Some(until),
            _ => None,
        })
        .min();
    Pick::OutOfQuota { until }
}

/// When a backend that ran out of quota gets it back, as its message says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reset {
    /// At this local time of day (24-hour), its next occurrence.
    At { hour: u32, minute: u32 },
    /// The message names no time it can be read by.
    Unknown,
}

impl Reset {
    /// The moment of the reset, seen at `now` in a zone `utc_offset` seconds east of UTC. A
    /// time of day that has passed today is tomorrow's; an unknown reset is an hour on.
    pub fn until(self, now: Timestamp, utc_offset: i32) -> Timestamp {
        let now = now.unix_millis();
        match self {
            Reset::At { hour, minute } => {
                let offset = i64::from(utc_offset) * 1000;
                let local_midnight = (now + offset).div_euclid(MILLIS_PER_DAY) * MILLIS_PER_DAY;
                let minutes = i64::from(hour) * 60 + i64::from(minute);
                let mut until = local_midnight + minutes * MILLIS_PER_MINUTE - offset;
                if until <= now {
                    until += MILLIS_PER_DAY;
                }
                Timestamp::from_unix_millis(until)
            }
            Reset::Unknown => Timestamp::from_unix_millis(now + UNKNOWN_RESET_MILLIS),
        }
    }
}

/// Codex's out-of-quota signature in its event stream: an error that says "usage limit", with
/// the reset in its "try again at 12:39 PM" (the last such time its errors name). Only Codex's
/// own errors count, not what it read while reviewing. None when no error says so.
pub fn codex_limit(stream: &str) -> Option<Reset> {
    let errors: Vec<String> = stream.lines().filter_map(codex_error).collect();
    if !errors.iter().any(|error| error.contains("usage limit")) {
        return None;
    }
    let reset = errors
        .iter()
        .flat_map(|error| {
            error
                .match_indices("try again at ")
                .filter_map(|(at, phrase)| clock_time(&error[at + phrase.len()..]))
        })
        .last()
        .unwrap_or(Reset::Unknown);
    Some(reset)
}

/// The error a line of Codex's stream reports: an `error` or `turn.failed` event's message, or
/// a line that isn't JSON (its stderr). None for its other events.
fn codex_error(line: &str) -> Option<String> {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Some(line.to_owned());
    };
    let message = match event.get("type").and_then(Value::as_str)? {
        "error" => event.get("message"),
        "turn.failed" => event.get("error").and_then(|error| error.get("message")),
        _ => None,
    };
    message.and_then(Value::as_str).map(str::to_owned)
}

/// A 12-hour clock time at the start of `text`, such as `12:39 PM`.
fn clock_time(text: &str) -> Option<Reset> {
    let (hour, rest) = text.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = rest.get(..2)?.parse().ok()?;
    let half = rest.get(2..)?.trim_start();
    let pm = match half.get(..2)?.to_ascii_uppercase().as_str() {
        "AM" => false,
        "PM" => true,
        _ => return None,
    };
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour = hour % 12 + if pm { 12 } else { 0 };
    Some(Reset::At { hour, minute })
}
