//! Whether a phone can run unattended (#55): its **readiness**, judged from what adb's probes
//! print, and the one desktop notification each change of it earns.

/// The shell line that prints the phone's `stay_on_while_plugged_in` setting: a bit per power
/// source the screen stays on for (1 AC, 2 USB, 4 wireless, 8 dock).
pub const STAY_ON_PROBE: &str = "settings get global stay_on_while_plugged_in";
/// The shell line that prints which power sources the phone is on.
/// It ends in `head` so that naming no source is an answer, not a failure.
pub const BATTERY_PROBE: &str = "dumpsys battery | grep -E 'powered: ' | head -8";
/// The shell line that prints whether the screen is awake.
pub const POWER_PROBE: &str = "dumpsys power | grep -E 'mWakefulness=' | head -1";
/// The shell line that prints adb's authorization timeout; `0` when it is off.
pub const AUTH_TIMEOUT_PROBE: &str = "settings get global adb_allowed_connection_time";

/// How old, in ms, a probe of a ready phone may be for a grant to rest on it.
pub const GRANT_FRESH_MS: i64 = 10_000;

/// Whether a grant probes a phone again: its last probe, which found it `was`, is `age_ms` old.
/// One that is ready is believed for [GRANT_FRESH_MS]; one that is not is always probed, so a
/// request sees the phone its owner has just unlocked.
pub fn probe_again(was: Readiness, age_ms: i64) -> bool {
    was != Readiness::Ready || age_ms > GRANT_FRESH_MS
}

/// How long before its [GRANT_FRESH_MS] are up a grant's refresh probes a ready phone again,
/// so that the probe is still fresh when the grant reads it.
pub const GRANT_MARGIN_MS: i64 = 2_000;

/// Whether a grant's refresh probes a phone again: as [probe_again], and a ready one already
/// once its probe, `age_ms` old, is due within [GRANT_MARGIN_MS].
pub fn refresh_again(was: Readiness, age_ms: i64) -> bool {
    probe_again(was, age_ms.saturating_add(GRANT_MARGIN_MS))
}

/// What a grant takes a phone to be: `state` is how adb shows it now, `probed` what the last
/// probe of it found and that probe's age in ms, when one was made since adb has shown it so.
/// A phone adb can't use is what adb says, whatever was probed before; one it can use is what
/// its probe found, or offline before any. None: a ready probe more than [GRANT_FRESH_MS] old,
/// which the grant has made again before it believes the phone ready.
pub fn for_grant(state: &str, probed: Option<(Readiness, i64)>) -> Option<Readiness> {
    if state != "device" {
        return Some(judge(&Probe {
            state: state.to_owned(),
            stay_on: None,
            battery: None,
            power: None,
            lock: None,
        }));
    }
    match probed {
        None => Some(Readiness::Offline),
        Some((Readiness::Ready, age_ms)) if probe_again(Readiness::Ready, age_ms) => None,
        Some((readiness, _)) => Some(readiness),
    }
}

/// What a phone is, as far as running unattended goes. Serialized as its [`Readiness::word`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Readiness {
    /// Authorized, kept awake on its power source, awake and unlocked.
    Ready,
    /// The keyguard shows.
    Locked,
    /// The screen is not awake.
    Asleep,
    /// `stay_on_while_plugged_in` doesn't cover the power source it is on.
    #[serde(rename = "stay-awake off")]
    StayAwakeOff,
    /// adb lists it, but the phone hasn't allowed this computer's USB debugging.
    Unauthorized,
    /// adb lists it but can't reach it.
    Offline,
}

impl Readiness {
    /// The word the store and `--json` keep.
    pub fn word(self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::Locked => "locked",
            Readiness::Asleep => "asleep",
            Readiness::StayAwakeOff => "stay-awake off",
            Readiness::Unauthorized => "unauthorized",
            Readiness::Offline => "offline",
        }
    }

    pub fn from_word(word: &str) -> Option<Readiness> {
        [
            Readiness::Ready,
            Readiness::Locked,
            Readiness::Asleep,
            Readiness::StayAwakeOff,
            Readiness::Unauthorized,
            Readiness::Offline,
        ]
        .into_iter()
        .find(|r| r.word() == word)
    }
}

/// What adb said of one phone: its state in adb's list, and what each probe printed (none
/// when adb failed to run it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub state: String,
    /// [STAY_ON_PROBE]'s output.
    pub stay_on: Option<String>,
    /// [BATTERY_PROBE]'s output.
    pub battery: Option<String>,
    /// [POWER_PROBE]'s output.
    pub power: Option<String>,
    /// [crate::lab::devices::LOCK_PROBE]'s output: non-empty while the keyguard shows.
    pub lock: Option<String>,
}

/// The phone's readiness. adb's own word comes first, then stay-awake (without it the screen
/// sleeps and locks again), then the keyguard, then the screen.
pub fn judge(p: &Probe) -> Readiness {
    match p.state.as_str() {
        "device" => {}
        "unauthorized" => return Readiness::Unauthorized,
        _ => return Readiness::Offline,
    }
    // A probe adb got no answer to leaves the phone unknown, not fine.
    let (Some(stay_on), Some(battery), Some(power), Some(lock)) = (
        p.stay_on.as_deref(),
        p.battery.as_deref(),
        p.power.as_deref(),
        p.lock.as_deref(),
    ) else {
        return Readiness::Offline;
    };
    let stay_on: u32 = stay_on.trim().parse().unwrap_or(0);
    // A phone on no power at all sleeps whatever stay-awake says.
    let sources = power_sources(battery);
    if sources == 0 || stay_on & sources != sources {
        return Readiness::StayAwakeOff;
    }
    if !lock.trim().is_empty() {
        return Readiness::Locked;
    }
    let wakefulness = power
        .split_whitespace()
        .find_map(|w| w.strip_prefix("mWakefulness="));
    if wakefulness.is_some_and(|w| w != "Awake") {
        return Readiness::Asleep;
    }
    Readiness::Ready
}

/// The stay-awake bits of the power sources `dumpsys battery` says the phone is on, none when
/// it says each is off; USB when it names no source, as a phone adb reaches over USB is on it.
fn power_sources(battery: &str) -> u32 {
    let mut named = false;
    let on: u32 = battery
        .lines()
        .filter_map(|line| {
            let (source, powered) = line.trim().split_once(" powered:")?;
            named = true;
            (powered.trim() == "true").then_some(match source {
                "AC" => 1,
                "USB" => 2,
                "Wireless" => 4,
                "Dock" => 8,
                _ => 0,
            })
        })
        .fold(0, |bits, bit| bits | bit);
    if named { on } else { 2 }
}

/// How loud a notification is: `notify-send -u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn word(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

/// One desktop notification: its summary line, a body that says what to do, and its urgency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
}

/// What is kept of a phone from its plug-in to its unplug: its last readiness, and whether it
/// was ready at any point since it was plugged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    pub readiness: Readiness,
    pub ever_ready: bool,
}

/// What is kept of a phone once a probe found it `now`; `before` is none for a phone just
/// plugged in.
pub fn after(before: Option<Known>, now: Readiness) -> Known {
    Known {
        readiness: now,
        ever_ready: before.is_some_and(|k| k.ever_ready) || now == Readiness::Ready,
    }
}

const UNLOCK_ONCE: &str = "Unlock it once: it stays awake and unlocked while plugged in.";

/// The notification a phone named `name` earns when a probe finds it `now`; none when its
/// readiness did not change, so no state is said twice.
pub fn notice(name: &str, before: Option<Known>, now: Readiness) -> Option<Notice> {
    if before.is_some_and(|k| k.readiness == now) {
        return None;
    }
    let was_ready = before.is_some_and(|k| k.ever_ready);
    let (summary, body, urgency) = match now {
        Readiness::Ready if was_ready => (
            format!("{name} ready again"),
            "It stays awake and unlocked while plugged in.",
            Urgency::Normal,
        ),
        Readiness::Ready => (
            format!("{name} ready"),
            "It stays awake and unlocked while plugged in.",
            Urgency::Low,
        ),
        Readiness::Locked if was_ready => (
            format!("{name} locked again"),
            "Unlock it once: it stays awake and unlocked while plugged in. If it locks again, check it is on power and run devlab check.",
            Urgency::Critical,
        ),
        Readiness::Locked => (
            format!("{name} needs unlocking"),
            UNLOCK_ONCE,
            Urgency::Critical,
        ),
        Readiness::Asleep => (
            format!("{name} asleep"),
            "Its screen is off: wake it and unlock it once; it stays awake while plugged in.",
            Urgency::Normal,
        ),
        Readiness::StayAwakeOff => (
            format!("{name} won't stay awake"),
            "Stay-awake is off for the power it is on: replug it, or run devlab prep.",
            Urgency::Normal,
        ),
        Readiness::Unauthorized => (
            format!("{name}: allow USB debugging"),
            "Unlock the phone and tap Allow (tick Always allow from this computer).",
            Urgency::Critical,
        ),
        Readiness::Offline => (
            format!("{name} offline"),
            "adb can't reach it: replug it, or run adb kill-server.",
            Urgency::Normal,
        ),
    };
    Some(Notice {
        summary,
        body: body.to_owned(),
        urgency,
    })
}

/// The notification for a phone named `name` that was unplugged; `holder` names the lease
/// that held it (`lease 3 (demo-a)`), if one did.
pub fn unplugged(name: &str, holder: Option<&str>) -> Notice {
    Notice {
        summary: format!("{name} unplugged"),
        body: holder.map_or_else(String::new, |h| {
            format!("{h} was using it; its runs fail until it is back.")
        }),
        urgency: if holder.is_some() {
            Urgency::Critical
        } else {
            Urgency::Low
        },
    }
}

/// What `devlab check` says of phone `serial`: ready, or what is wrong and what to do.
/// `readiness` is none for a phone adb doesn't list; `warning` is said after a ready.
pub fn check_line(
    serial: &str,
    model: &str,
    readiness: Option<Readiness>,
    warning: Option<&str>,
) -> String {
    let name = if model.is_empty() {
        serial.to_owned()
    } else {
        format!("{serial} ({model})")
    };
    let what = match readiness {
        None => "not attached: plug it in and allow USB debugging".to_owned(),
        Some(Readiness::Ready) => match warning {
            Some(w) => format!("ready; {w}"),
            None => "ready".to_owned(),
        },
        Some(Readiness::Locked) => {
            "locked: unlock it once; it then stays awake and unlocked while plugged in".to_owned()
        }
        Some(Readiness::Asleep) => {
            "asleep: wake it and unlock it once; it then stays awake while plugged in".to_owned()
        }
        Some(Readiness::StayAwakeOff) => {
            format!("stay-awake off: replug it, or run devlab prep {serial}")
        }
        Some(Readiness::Unauthorized) => {
            "unauthorized: unlock it and tap Allow on the USB debugging prompt (tick Always allow)"
                .to_owned()
        }
        Some(Readiness::Offline) => "offline: replug it, or run adb kill-server".to_owned(),
    };
    format!("{name}: {what}")
}

/// The warning for a phone whose [AUTH_TIMEOUT_PROBE] printed `out`: adb's authorization
/// still times out (the prep turns it off where the phone lets it), so the phone will ask to
/// allow USB debugging again.
pub fn auth_timeout_warning(out: Option<&str>) -> Option<&'static str> {
    (out.map(str::trim) != Some("0")).then_some(
        "adb authorization may time out: turn on Developer options > Disable adb authorization timeout",
    )
}
