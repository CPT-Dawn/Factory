//! Reviewer quotas: the state machine each backend's quota follows, the router's pick of a
//! backend, and the readers that find a backend's out-of-quota signature and its reset time
//! (spec #1, "Reviewer router").

use serde_json::Value;

use crate::timestamp::Timestamp;
use crate::usage::{self, GroupWindow, QuotaGroup, WindowKind, WindowState};

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
    /// At this local time of this weekday (0 is Sunday), its next occurrence.
    Weekly {
        weekday: u32,
        hour: u32,
        minute: u32,
    },
    /// This long after the message.
    In { millis: i64 },
    /// At this moment.
    On(Timestamp),
    /// The message names no time it can be read by.
    Unknown,
}

impl Reset {
    /// The moment of the reset, seen at `now` in a zone `utc_offset` seconds east of UTC. A
    /// time of day that has passed today is tomorrow's; an unknown reset is an hour on.
    pub fn until(self, now: Timestamp, utc_offset: i32) -> Timestamp {
        let now = now.unix_millis();
        match self {
            Reset::At { hour, minute } => next_local(now, utc_offset, None, hour, minute),
            Reset::Weekly {
                weekday,
                hour,
                minute,
            } => next_local(now, utc_offset, Some(weekday), hour, minute),
            Reset::In { millis } => Timestamp::from_unix_millis(now.saturating_add(millis)),
            Reset::On(moment) => moment,
            Reset::Unknown => Timestamp::from_unix_millis(now + UNKNOWN_RESET_MILLIS),
        }
    }
}

/// The next local `hour`:`minute` after `now` (unix millis) in a zone `utc_offset` seconds east
/// of UTC: today's or tomorrow's, or with a `weekday` (0 is Sunday) that weekday's this week or
/// next.
fn next_local(
    now: i64,
    utc_offset: i32,
    weekday: Option<u32>,
    hour: u32,
    minute: u32,
) -> Timestamp {
    let offset = i64::from(utc_offset) * 1000;
    let day = (now + offset).div_euclid(MILLIS_PER_DAY);
    let (ahead, period) = match weekday {
        None => (0, 1),
        // 1970-01-01 was a Thursday.
        Some(weekday) => ((i64::from(weekday) - (day + 4)).rem_euclid(7), 7),
    };
    let minutes = i64::from(hour) * 60 + i64::from(minute);
    let mut until = (day + ahead) * MILLIS_PER_DAY + minutes * MILLIS_PER_MINUTE - offset;
    if until <= now {
        until += period * MILLIS_PER_DAY;
    }
    Timestamp::from_unix_millis(until)
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

/// The reset agy reports for a window nobody has used: the Unix epoch.
const AGY_EPOCH_MILLIS: i64 = 0;

/// What agy's free usage query (`agy -p /usage --output-format json`) says of one model
/// group's quota.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgyUsage {
    /// Every bucket of the group has some left.
    Left,
    /// A bucket is empty; the group has quota again once the last empty one resets.
    Empty { until: Timestamp },
    /// The answer names no buckets for the group, or none it can read: why.
    Unreadable(String),
}

/// Reads agy's usage for the model `group` (`Gemini Models`): each of its buckets (five-hour,
/// weekly) has a `remaining_fraction` and a `reset_time`.
pub fn agy_usage(usage: &str, group: &str) -> AgyUsage {
    let Ok(usage) = serde_json::from_str::<Value>(usage) else {
        return AgyUsage::Unreadable("its answer is not JSON".to_owned());
    };
    let buckets = agy_buckets(&usage, group).collect::<Vec<_>>();
    if buckets.is_empty() {
        return AgyUsage::Unreadable(format!("it shows no {group} quota"));
    }
    let mut until = None;
    for bucket in buckets {
        let left = bucket_left(bucket);
        let reset = bucket_reset(bucket);
        match (left, reset) {
            // The epoch is no reset: the bucket has been used, or it has not. Only a whole
            // quota left says it has not.
            (Some(left), Some(reset))
                if reset.unix_millis() == AGY_EPOCH_MILLIS && !epoch_means_unused(left) =>
            {
                return AgyUsage::Unreadable(format!(
                    "a {group} bucket has used some quota but reports no reset"
                ));
            }
            (Some(_), Some(reset)) if reset.unix_millis() == AGY_EPOCH_MILLIS => {}
            (Some(left), _) if left > 0.0 => {}
            (Some(_), Some(reset)) => until = until.max(Some(reset)),
            _ => {
                return AgyUsage::Unreadable(format!(
                    "a {group} bucket has no quota or reset it can read"
                ));
            }
        }
    }
    until.map_or(AgyUsage::Left, |until| AgyUsage::Empty { until })
}

/// Every quota group agy's usage answer lists, each with its 5-hour and weekly windows as the
/// answer reads them, or why the answer can't be read at all.
///
/// A bucket that agy reports with the Unix epoch as its reset has no clock running: it is an
/// unused window when its remaining fraction is 1, and unknown otherwise. It is never read as
/// a reset that has passed.
pub fn agy_groups(usage: &str) -> Result<Vec<QuotaGroup>, String> {
    let Ok(usage) = serde_json::from_str::<Value>(usage) else {
        return Err("its answer is not JSON".to_owned());
    };
    let listed = usage
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
        .filter(|groups| !groups.is_empty())
        .ok_or_else(|| "its answer lists no quota groups".to_owned())?;
    let (mut placed, mut read) = (false, false);
    let mut groups = Vec::new();
    for group in listed {
        let (Some(name), Some(buckets)) = (
            group.get("name").and_then(Value::as_str),
            group.get("buckets").and_then(Value::as_array),
        ) else {
            return Err("a quota group of its answer has no name or buckets".to_owned());
        };
        // Both windows of every group, so a bucket the answer leaves out is an unknown window
        // on the screen and not a window that is not there.
        let mut windows = [WindowKind::FiveHour, WindowKind::Weekly].map(|window| GroupWindow {
            window,
            state: WindowState::Unknown,
        });
        let mut seen = [false; 2];
        for bucket in buckets {
            let Some(kind) = bucket_window(bucket) else {
                continue;
            };
            let at = usize::from(kind == WindowKind::Weekly);
            if !std::mem::replace(&mut seen[at], true) {
                placed = true;
                let reading = read_bucket(bucket);
                read |= reading.is_some();
                windows[at].state = bucket_state(reading);
            }
        }
        groups.push(QuotaGroup {
            name: name.to_owned(),
            windows: windows.to_vec(),
        });
    }
    if !placed {
        return Err("its answer lists no 5-hour or weekly window".to_owned());
    }
    if !read {
        return Err("its answer has no bucket it can read".to_owned());
    }
    Ok(groups)
}

/// Whether a bucket with the epoch as its reset, which has no clock running, is untouched: only
/// when the whole quota is left.
#[allow(clippy::float_cmp)]
fn epoch_means_unused(left: f64) -> bool {
    left == 1.0
}

/// The share of a bucket left, 0 to 1, as agy printed it.
fn bucket_left(bucket: &Value) -> Option<f64> {
    bucket
        .get("remaining_fraction")
        .and_then(Value::as_f64)
        .filter(|left| (0.0..=1.0).contains(left))
}

/// When a bucket resets, as agy printed it.
fn bucket_reset(bucket: &Value) -> Option<Timestamp> {
    bucket
        .get("reset_time")
        .and_then(Value::as_str)
        .and_then(Timestamp::parse)
}

/// A bucket as agy printed it, for the display: the share left and when it resets, the reset
/// a time that can stand (or the epoch, which says none).
fn read_bucket(bucket: &Value) -> Option<(f64, i64)> {
    let left = bucket_left(bucket)?;
    let resets = bucket_reset(bucket)?.unix_millis();
    (resets == AGY_EPOCH_MILLIS || usage::representable(resets)).then_some((left, resets))
}

/// Which window a bucket is: its `window` (`5h`, `weekly`), else what its id or name says.
fn bucket_window(bucket: &Value) -> Option<WindowKind> {
    ["window", "id", "name"]
        .into_iter()
        .filter_map(|key| bucket.get(key)?.as_str())
        .find_map(|text| {
            let text = text.to_lowercase();
            if text.contains("5h") || text.contains("five hour") || text.contains("5-hour") {
                Some(WindowKind::FiveHour)
            } else if text.contains("week") {
                Some(WindowKind::Weekly)
            } else {
                None
            }
        })
}

/// What a bucket says of its window.
fn bucket_state(reading: Option<(f64, i64)>) -> WindowState {
    match reading {
        Some((left, resets)) if resets == AGY_EPOCH_MILLIS => {
            if epoch_means_unused(left) {
                WindowState::Unused
            } else {
                WindowState::Unknown
            }
        }
        Some((left, resets)) if usage::representable(resets) => WindowState::Used {
            // Rounded to a millionth first: 1 - 0.9 is 0.0999... in floats, and 10% used.
            used_percent: usage::whole_percent(((1.0 - left) * 100.0 * 1e6).round() / 1e6),
            resets,
        },
        _ => WindowState::Unknown,
    }
}

/// Whether a bucket of agy's model `group` over `window` (`5h`, `weekly`) has less than
/// `fraction` of its quota left, by its usage query: a reserve kept for the reviews that may
/// spend it. An answer that can't be read keeps nothing back.
pub fn agy_below(usage: &str, group: &str, window: &str, fraction: f64) -> bool {
    let Ok(usage) = serde_json::from_str::<Value>(usage) else {
        return false;
    };
    agy_buckets(&usage, group)
        .filter(|bucket| bucket.get("window").and_then(Value::as_str) == Some(window))
        .filter_map(|bucket| bucket.get("remaining_fraction")?.as_f64())
        .any(|left| left < fraction)
}

/// The buckets agy's usage answer lists for the model `group`, in its order.
fn agy_buckets<'a>(usage: &'a Value, group: &'a str) -> impl Iterator<Item = &'a Value> {
    usage
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |candidate| candidate.get("name").and_then(Value::as_str) == Some(group))
        .filter_map(|group| group.get("buckets")?.as_array())
        .flatten()
}

/// The model agy runs when none is chosen, in the factory's config or in agy: its log names it
/// `Gemini 4 Neon (High)` (agy 1.5).
pub const AGY_DEFAULT_MODEL: &str = "gemini-4-neon";

/// What a group's description says before the labels of the models that share its quota.
const AGY_MODELS_LISTED: &str = "Models within this group:";

/// The group of agy's models whose quota `model` draws on, by agy's `usage` answer: the group
/// whose "Models within this group" list names the model's family (`Gemini 4 Argon`,
/// `Claude Opus 5.5`), else a group named for it (the old answer's `Gemini 4 Argon`), else
/// the static map: Claude and GPT models share one group and every Gemini model the other.
/// `model` is an ID such as `claude-opus-5-5-high` or a label as agy's `/model` saves it,
/// such as `Claude Opus 5.5 (Thinking)`; the longest family that begins it wins.
pub fn agy_group(usage: &str, model: &str) -> String {
    let model = words(model);
    let groups: Vec<Value> = serde_json::from_str::<Value>(usage)
        .ok()
        .and_then(|usage| usage.pointer("/command/data/groups")?.as_array().cloned())
        .unwrap_or_default();
    let named = |group: &Value| group.get("name").and_then(Value::as_str).map(str::to_owned);
    // The longest of `families` that begins the model, with its group.
    let longest = |families: Vec<(String, String)>| {
        families
            .into_iter()
            .filter_map(|(family, group)| {
                let family = words(&family);
                (!family.is_empty() && model.starts_with(&family)).then_some((family.len(), group))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, group)| group)
    };
    let listed = groups
        .iter()
        .filter_map(|group| {
            let name = named(group)?;
            let description = group.get("description")?.as_str()?;
            let (_, models) = description.split_once(AGY_MODELS_LISTED)?;
            Some(
                models
                    .split(',')
                    .map(|family| (family.trim().trim_end_matches('.').to_owned(), name.clone()))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();
    let by_name = groups
        .iter()
        .filter_map(|group| named(group).map(|name| (name.clone(), name)))
        .collect();
    longest(listed)
        .or_else(|| longest(by_name))
        .unwrap_or_else(|| {
            match model.first().map(String::as_str) {
                Some("claude" | "gpt") => "Claude and GPT models",
                _ => "Gemini Models",
            }
            .to_owned()
        })
}

/// The lowercase words of a model's ID or label, split at anything not a letter or digit:
/// `claude-opus-5-5-high` and `Claude Opus 5.5` both begin `claude opus 5 5`.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The model a review draws on when the factory's config names none: the one chosen with
/// agy's `/model`, which its `settings` (`~/.gemini/antigravity-cli/settings.json`) keep by
/// its label under `model`, else agy's default. Settings that can't be read choose none.
pub fn agy_chosen_model(settings: Option<&str>) -> String {
    settings
        .and_then(|settings| serde_json::from_str::<Value>(settings).ok())
        .and_then(|settings| Some(settings.get("model")?.as_str()?.trim().to_owned()))
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| AGY_DEFAULT_MODEL.to_owned())
}

/// agy's out-of-quota signature: an error of its own that says "Individual quota reached" and
/// "Resets in 2h13m5s". Its errors are the `error` of its JSON `result` and the `AGY_ERROR:`
/// lines of its `stderr`, not what it read while reviewing. None when no error says so.
pub fn agy_limit(result: &str, stderr: &str) -> Option<Reset> {
    let result_error = serde_json::from_str::<Value>(result)
        .ok()
        .and_then(|result| match result.get("error")? {
            Value::String(error) => Some(error.clone()),
            Value::Null => None,
            error => Some(error.to_string()),
        });
    let errors = stderr
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("AGY_ERROR:"))
        .map(str::to_owned)
        .chain(result_error);
    errors
        .filter(|error| error.contains("Individual quota reached"))
        .filter_map(|error| {
            let (_, after) = error.split_once("Resets in ")?;
            Some(span(after).map_or(Reset::Unknown, |millis| Reset::In { millis }))
        })
        .next_back()
}

/// A span such as `2h13m5s` or `1d2h` at the start of `text`, in milliseconds.
fn span(text: &str) -> Option<i64> {
    let mut total = 0_i64;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            'd' => MILLIS_PER_DAY,
            'h' => 3_600_000,
            'm' => MILLIS_PER_MINUTE,
            's' => 1000,
            _ => break,
        };
        total = total.checked_add(number.parse::<i64>().ok()?.checked_mul(unit)?)?;
        number.clear();
    }
    (total > 0 && number.is_empty()).then_some(total)
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
