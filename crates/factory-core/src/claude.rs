//! The Claude plan's quota: its 5-hour window as Claude Code's statusline reports it, the limit a
//! 429 says was hit, and whether a lead may dispatch another agent now.
//!
//! The statusline's `rate_limits.five_hour` (`used_percentage`, `resets_at`) is the documented
//! signal; a limit message (`You've hit your session limit · resets 3:45pm`) is the fallback,
//! read when a turn ends on a rate limit.

use serde_json::Value;

use crate::quota::Reset;
use crate::status;
use crate::timestamp::Timestamp;

const MILLIS_PER_HOUR: i64 = 3_600_000;

/// How much of the window may be spent before a lead holds new dispatches, by default.
pub const HOLD_AT_PERCENT: f64 = 90.0;

/// The 5-hour window as one statusline saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// 0 to 100.
    pub used_percent: f64,
    pub resets: Timestamp,
}

/// A window and when it was seen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seen {
    pub window: Window,
    pub at: Timestamp,
}

/// A limit a turn hit: dispatches hold until `until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub until: Timestamp,
    /// When it was hit.
    pub at: Timestamp,
}

/// What is known of the Claude plan's quota.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ClaudeQuota {
    pub window: Option<Seen>,
    pub limit: Option<Limit>,
}

/// Whether a lead may dispatch now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    /// No window seen, or the last one has reset: dispatch.
    Unknown,
    /// The window has room.
    Go {
        used_percent: f64,
        resets: Timestamp,
    },
    /// Hold new dispatches until `until`.
    Hold {
        until: Timestamp,
        used_percent: Option<f64>,
    },
}

impl ClaudeQuota {
    /// The window as it stands at `now`: none once its reset has come.
    pub fn window_at(&self, now: Timestamp) -> Option<Window> {
        self.window
            .map(|seen| seen.window)
            .filter(|window| window.resets > now)
    }

    /// The limit's reset while it holds at `now`.
    pub fn held_until(&self, now: Timestamp) -> Option<Timestamp> {
        self.limit
            .filter(|limit| limit.until > now)
            .map(|limit| limit.until)
    }

    /// A statusline saw `window` at `now`. A window with room, seen after a limit was hit,
    /// lifts the limit: the 429 was not the plan's.
    pub fn see_window(&mut self, window: Window, now: Timestamp) {
        if self
            .limit
            .is_some_and(|limit| now > limit.at && window.used_percent < 100.0)
        {
            self.limit = None;
        }
        self.window = Some(Seen { window, at: now });
    }

    /// A turn hit a limit at `now` that resets at `reset`. Without one, it holds until the
    /// window's reset, else for an hour. A limit already holding keeps its reset unless this one
    /// names a later one.
    pub fn hit_limit(&mut self, reset: Option<Timestamp>, now: Timestamp) {
        let named = reset.or_else(|| self.window_at(now).map(|window| window.resets));
        let until = match (self.held_until(now), reset) {
            (Some(held), None) => held,
            (Some(held), Some(reset)) => held.max(reset),
            (None, _) => named.unwrap_or_else(|| {
                Timestamp::from_unix_millis(now.unix_millis() + MILLIS_PER_HOUR)
            }),
        };
        self.limit = Some(Limit { until, at: now });
    }

    /// Whether a lead may dispatch at `now`, holding once `hold_at` percent of the window is
    /// spent or while a limit holds.
    pub fn verdict(&self, now: Timestamp, hold_at: f64) -> Verdict {
        let window = self.window_at(now);
        if let Some(until) = self.held_until(now) {
            return Verdict::Hold {
                until,
                used_percent: window.map(|window| window.used_percent),
            };
        }
        match window {
            Some(window) if window.used_percent >= hold_at => Verdict::Hold {
                until: window.resets,
                used_percent: Some(window.used_percent),
            },
            Some(window) => Verdict::Go {
                used_percent: window.used_percent,
                resets: window.resets,
            },
            None => Verdict::Unknown,
        }
    }
}

impl Verdict {
    pub fn state(self) -> status::ClaudeState {
        match self {
            Verdict::Unknown => status::ClaudeState::Unknown,
            Verdict::Go { .. } => status::ClaudeState::Go,
            Verdict::Hold { .. } => status::ClaudeState::Hold,
        }
    }

    /// The verdict as `factory status` records it.
    pub fn record(self) -> status::Claude {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let round = |used: f64| used.round().clamp(0.0, 100.0) as u32;
        let (used_percent, resets, until) = match self {
            Verdict::Unknown => (None, None, None),
            Verdict::Go {
                used_percent,
                resets,
            } => (Some(round(used_percent)), Some(resets.unix_millis()), None),
            Verdict::Hold {
                until,
                used_percent,
            } => (used_percent.map(round), None, Some(until.unix_millis())),
        };
        status::Claude {
            verdict: self.state(),
            used_percent,
            resets,
            until,
        }
    }
}

/// The 5-hour window in Claude Code's statusline input: `rate_limits.five_hour`, present for
/// Pro and Max plans once a session has had its first answer.
pub fn statusline_window(input: &Value) -> Option<Window> {
    let five = input.pointer("/rate_limits/five_hour")?;
    let used_percent = five.get("used_percentage")?.as_f64()?;
    let resets = five.get("resets_at")?.as_f64()?;
    #[allow(clippy::cast_possible_truncation)]
    let resets = Timestamp::from_unix_millis((resets * 1000.0) as i64);
    Some(Window {
        used_percent,
        resets,
    })
}

/// A plan limit a message says was hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitHit {
    /// It resets at this moment.
    Resets(Timestamp),
    /// It names no reset that can be read.
    Unnamed,
}

impl LimitHit {
    pub fn reset(self) -> Option<Timestamp> {
        match self {
            LimitHit::Resets(at) => Some(at),
            LimitHit::Unnamed => None,
        }
    }
}

/// Whether `text` says a plan limit was hit (`You've hit your session limit · resets 3:45pm`,
/// `Usage limit reached · limit resets 3:45pm`, `Claude AI usage limit reached|<epoch>`), and
/// the reset it names, read in a zone `utc_offset` seconds east of UTC: a time of day that has
/// passed is the next day's, a weekday the next one's. None when it is no plan limit.
pub fn limit_reset(text: &str, now: Timestamp, utc_offset: i32) -> Option<LimitHit> {
    Some(match plan_limit(text)? {
        Reset::Unknown => LimitHit::Unnamed,
        reset => LimitHit::Resets(reset.until(now, utc_offset)),
    })
}

/// Whether the text of a failed `claude -p` says it ran out of quota: a plan limit
/// ([`limit_reset`]'s), a rate limit (`rate_limit_error`, HTTP 429), or a reset it names. The
/// reset is the one it names, else [`Reset::Unknown`]. None when the failure is no quota's.
pub fn failure_limit(text: &str) -> Option<Reset> {
    if let Some(reset) = plan_limit(text) {
        return Some(reset);
    }
    let lower = text.to_lowercase();
    let named = named_reset(&lower);
    let rate = lower.contains("rate limit")
        || names_rate_limit(&lower)
        || lower.contains("too many requests")
        || is_status(&lower, "429");
    (rate || named.is_some()).then(|| named.unwrap_or(Reset::Unknown))
}

/// Whether `lower` carries the API's `rate_limit` or `rate_limit_error` type as a whole word,
/// not an identifier that only starts with it (`rate_limit_config`).
fn names_rate_limit(lower: &str) -> bool {
    lower.match_indices("rate_limit").any(|(at, word)| {
        let rest = &lower[at + word.len()..];
        let rest = rest.strip_prefix("_error").unwrap_or(rest);
        !lower[..at].ends_with(is_word_char) && !rest.starts_with(is_word_char)
    })
}

/// Whether `text` has the HTTP status `code`: the number alone, after an HTTP marker
/// (`API Error: 429`, `HTTP/1.1 429`, `HTTP/2 429`, `"status":429`, `status code 429`), not
/// a number that only follows other words (`request 429 failed`, `request code 429`).
fn is_status(text: &str, code: &str) -> bool {
    text.match_indices(code).any(|(at, _)| {
        let alone = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric());
        if !alone(text[..at].chars().next_back()) || !alone(text[at + code.len()..].chars().next())
        {
            return false;
        }
        let before = text[..at].trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
        let http_version = before
            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
            .strip_suffix('/')
            .is_some_and(|head| ends_with_word(head, "http"));
        http_version
            || [
                "http",
                "http error",
                "status",
                "status code",
                "status_code",
                "statuscode",
                "api error",
            ]
            .iter()
            .any(|marker| ends_with_word(before, marker))
    })
}

/// Whether `text` ends with `word` as a whole word: no letter, digit or `_` just before it.
fn ends_with_word(text: &str, word: &str) -> bool {
    text.strip_suffix(word)
        .is_some_and(|head| !head.ends_with(is_word_char))
}

/// A letter, digit or `_`: a character that carries a word or identifier on.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Where `lower` says `resets` as a whole word (not inside `presets` or `resets_hourly`), the
/// last first.
fn resets_at(lower: &str) -> impl Iterator<Item = usize> + '_ {
    lower
        .rmatch_indices("resets")
        .map(|(at, _)| at)
        .filter(|at| {
            let end = at + "resets".len();
            ends_with_word(&lower[..end], "resets") && !lower[end..].starts_with(is_word_char)
        })
}

/// The reset `lower` names after its last `resets `: a time of day or a weekday and time.
fn named_reset(lower: &str) -> Option<Reset> {
    let at = resets_at(lower).find(|at| lower[at + "resets".len()..].starts_with(' '))?;
    reset_clock(&lower[at + "resets ".len()..])
}

/// The plan limit `text` says was hit, and the reset it names: [`Reset::Unknown`] when none
/// can be read.
fn plan_limit(text: &str) -> Option<Reset> {
    let lower = text.to_lowercase();
    if let Some((_, after)) = lower.split_once("limit reached|") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(secs) = digits.parse::<i64>() {
            return Some(Reset::On(Timestamp::from_unix_millis(
                secs.saturating_mul(1000),
            )));
        }
    }
    let plan = lower.contains("limit")
        && (resets_at(&lower).next().is_some()
            || lower.contains("hit your")
            || lower.contains("usage limit"));
    plan.then(|| named_reset(&lower).unwrap_or(Reset::Unknown))
}

/// A `StopFailure` hook's input: the limit a turn ended on, with the reset its message names.
/// A rate-limit error counts even when its message names no plan limit; another error counts
/// only when its message does. None when the turn ended on no limit.
pub fn stop_failure(input: &Value, now: Timestamp, utc_offset: i32) -> Option<LimitHit> {
    let rate_limit = input.get("error").and_then(Value::as_str) == Some("rate_limit");
    let mut plan = false;
    for key in ["last_assistant_message", "error_details"] {
        let Some(text) = input.get(key).and_then(Value::as_str) else {
            continue;
        };
        match limit_reset(text, now, utc_offset) {
            Some(LimitHit::Resets(reset)) => return Some(LimitHit::Resets(reset)),
            Some(LimitHit::Unnamed) => plan = true,
            None => {}
        }
    }
    (rate_limit || plan).then_some(LimitHit::Unnamed)
}

const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// `3:45pm`, `11am`, `12:15 PM` or `Mon 12:00am` at the start of `text`, as a reset.
fn reset_clock(text: &str) -> Option<Reset> {
    let (weekday, clock) = match WEEKDAYS.iter().position(|day| text.starts_with(day)) {
        Some(day) => (Some(day), text.get(3..)?.trim_start()),
        None => (None, text),
    };
    let digits: String = clock.chars().take_while(char::is_ascii_digit).collect();
    let hour: u32 = digits.parse().ok()?;
    let mut rest = &clock[digits.len()..];
    let mut minute = 0;
    if let Some(after) = rest.strip_prefix(':') {
        minute = after.get(..2)?.parse().ok()?;
        rest = &after[2..];
    }
    let pm = match rest.trim_start().get(..2)? {
        "am" => false,
        "pm" => true,
        _ => return None,
    };
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour = hour % 12 + if pm { 12 } else { 0 };
    Some(match weekday {
        None => Reset::At { hour, minute },
        Some(weekday) => Reset::Weekly {
            weekday: u32::try_from(weekday).ok()?,
            hour,
            minute,
        },
    })
}
