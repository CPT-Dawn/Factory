//! The Claude plan's quota: its 5-hour and weekly windows as Claude Code's status line reports
//! them, the limit a 429 says was hit, and whether a lead may dispatch another agent now.
//!
//! The status line's `rate_limits.five_hour` and `rate_limits.seven_day` (`used_percentage`,
//! `resets_at`) are the documented signal; a limit message (`You've hit your session limit ·
//! resets 3:45pm`) is the fallback, read when a turn ends on a rate limit.

use serde_json::Value;

use crate::quota::Reset;
use crate::status;
use crate::timestamp::Timestamp;
use crate::usage::{self, UsageWindow, WindowKind};

const MILLIS_PER_HOUR: i64 = 3_600_000;

/// How much of either window may be spent before a lead holds new dispatches, by default.
pub const HOLD_AT_PERCENT: f64 = 90.0;

/// A limit a turn hit: dispatches hold until `until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub until: Timestamp,
    /// When it was hit.
    pub at: Timestamp,
}

/// What is known of the Claude plan's quota: the last reading of each window, and a limit hit.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ClaudeQuota {
    pub five_hour: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    pub limit: Option<Limit>,
}

/// Whether a lead may dispatch now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    /// No window's reading stands, and no limit holds: dispatch.
    Unknown,
    /// Every window that stands has room.
    Go,
    /// Hold new dispatches until `until`.
    Hold { until: Timestamp, by: Holding },
}

/// What holds dispatches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Holding {
    /// A turn hit the plan's limit.
    Limit,
    /// A window over the mark.
    Window(UsageWindow),
}

impl Holding {
    /// As the screen and the quota line name it: `weekly 91%`, or `limit hit`.
    pub fn describe(self) -> String {
        match self {
            Holding::Limit => "limit hit".to_owned(),
            Holding::Window(window) => format!(
                "{} {}%",
                window.kind.label(),
                usage::whole_percent(window.used_percent)
            ),
        }
    }
}

impl ClaudeQuota {
    /// The windows a reading was seen of, the 5-hour one first.
    pub fn windows(&self) -> Vec<UsageWindow> {
        self.five_hour.into_iter().chain(self.weekly).collect()
    }

    /// The limit's reset while it holds at `now`.
    pub fn held_until(&self, now: Timestamp) -> Option<Timestamp> {
        self.limit
            .filter(|limit| limit.until > now)
            .map(|limit| limit.until)
    }

    /// A status line read `windows` at `now`; a window it doesn't mention keeps its last
    /// reading. A reading in which every window has room, seen after a limit was hit, lifts the
    /// limit: the 429 was not the plan's.
    pub fn see(&mut self, windows: &[UsageWindow], now: Timestamp) {
        if windows.is_empty() {
            return;
        }
        if self.limit.is_some_and(|limit| now > limit.at)
            && windows.iter().all(|window| window.used_percent < 100.0)
        {
            self.limit = None;
        }
        for window in windows {
            match window.kind {
                WindowKind::FiveHour => self.five_hour = Some(*window),
                WindowKind::Weekly => self.weekly = Some(*window),
            }
        }
    }

    /// A turn hit a limit at `now` that resets at `reset`. Without one, it holds until the
    /// 5-hour window's reset, else for an hour. A limit already holding keeps its reset unless
    /// this one names a later one.
    pub fn hit_limit(&mut self, reset: Option<Timestamp>, now: Timestamp) {
        let five_hour = self
            .five_hour
            .filter(|window| window.current(now))
            .map(|window| window.resets);
        let named = reset.or(five_hour);
        let until = match (self.held_until(now), reset) {
            (Some(held), None) => held,
            (Some(held), Some(reset)) => held.max(reset),
            (None, _) => named.unwrap_or_else(|| {
                Timestamp::from_unix_millis(now.unix_millis() + MILLIS_PER_HOUR)
            }),
        };
        self.limit = Some(Limit { until, at: now });
    }

    /// Whether a lead may dispatch at `now`, holding once `hold_at` percent of either window
    /// is spent or while a limit holds; of both, whichever ends last.
    pub fn verdict(&self, now: Timestamp, hold_at: f64) -> Verdict {
        let windows = usage::verdict(&self.windows(), now, hold_at);
        match (self.held_until(now), windows) {
            (Some(until), usage::Verdict::Hold { by }) if by.resets > until => Verdict::Hold {
                until: by.resets,
                by: Holding::Window(by),
            },
            (Some(until), _) => Verdict::Hold {
                until,
                by: Holding::Limit,
            },
            (None, usage::Verdict::Hold { by }) => Verdict::Hold {
                until: by.resets,
                by: Holding::Window(by),
            },
            (None, usage::Verdict::Go) => Verdict::Go,
            (None, usage::Verdict::Unknown) => Verdict::Unknown,
        }
    }

    /// The quota at `now` as `factory status` and the bar record it.
    pub fn record(&self, now: Timestamp, hold_at: f64) -> status::Claude {
        let verdict = self.verdict(now, hold_at);
        let five_hour = self.five_hour.filter(|window| window.current(now));
        let (state, until, hold) = match verdict {
            Verdict::Unknown => (status::ClaudeState::Unknown, None, None),
            Verdict::Go => (status::ClaudeState::Go, None, None),
            Verdict::Hold { until, by } => (
                status::ClaudeState::Hold,
                Some(until.unix_millis()),
                Some(by.describe()),
            ),
        };
        status::Claude {
            verdict: state,
            used_percent: five_hour.map(|window| usage::whole_percent(window.used_percent)),
            resets: five_hour.map(|window| window.resets.unix_millis()),
            until,
            hold,
            windows: self
                .windows()
                .iter()
                .map(|window| window.reading(now))
                .collect(),
        }
    }
}

/// The windows in Claude Code's status-line input, read at `now`: `rate_limits.five_hour` and
/// `rate_limits.seven_day`, present for Pro and Max plans once a session has had its first
/// answer. A window that is missing or malformed is left out; any input at all is read.
pub fn statusline_windows(input: &Value, now: Timestamp) -> Vec<UsageWindow> {
    [
        ("/rate_limits/five_hour", WindowKind::FiveHour),
        ("/rate_limits/seven_day", WindowKind::Weekly),
    ]
    .into_iter()
    .filter_map(|(pointer, kind)| {
        let window = input.pointer(pointer)?;
        let used_percent = window.get("used_percentage")?.as_f64()?;
        let resets = window.get("resets_at")?.as_f64()?;
        if !used_percent.is_finite() || !resets.is_finite() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        let resets = Timestamp::from_unix_millis((resets * 1000.0) as i64);
        Some(UsageWindow {
            kind,
            used_percent,
            resets,
            seen: now,
        })
    })
    .collect()
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
