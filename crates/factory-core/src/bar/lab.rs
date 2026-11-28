//! The Lab chip and card: whether the lab needs the owner's hands. The daemon's stored checks
//! come in (each phone's last readiness, the emulators and stack slots it keeps); the chip, the
//! card and the line's `lab 2/2` go out. Nothing here asks adb or docker.

use serde::{Deserialize, Serialize};

use super::{Chip, Row, Section, Tone, glyph, stamp};
use crate::lab::ago;
use crate::lab::ready::{Readiness, USB_DEBUGGING_OFF_WORDS, phone_name};

/// How old, in ms, a free phone's readiness may be before the card shows its age.
pub const STALE_MS: i64 = 5 * 60_000;

/// What a phone's row says: whose it is, and for a free phone what its readiness is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// No lease holds it: what its last check found.
    Free(Readiness),
    /// A lease holds it, whatever it was last found: its words are followed by the holder.
    Leased,
}

/// The one table of what each phone's row asks of the owner, and its tone.
pub const WORDS: [(Shown, &str, Tone); 8] = [
    (Shown::Free(Readiness::Ready), "ready", Tone::Ok),
    (Shown::Leased, "leased to", Tone::Busy),
    (Shown::Free(Readiness::Locked), "unlock it", Tone::Warn),
    (Shown::Free(Readiness::Asleep), "wake it", Tone::Warn),
    (
        Shown::Free(Readiness::StayAwakeOff),
        "turn on Stay awake",
        Tone::Warn,
    ),
    (
        Shown::Free(Readiness::Unauthorized),
        "allow USB debugging",
        Tone::Critical,
    ),
    (Shown::Free(Readiness::Offline), "replug it", Tone::Critical),
    (
        Shown::Free(Readiness::UsbDebuggingOff),
        USB_DEBUGGING_OFF_WORDS,
        Tone::Warn,
    ),
];

/// The words and tone of a phone, from [`WORDS`]: `leased to <holder>` while a lease holds it,
/// else what its readiness asks of the owner; a leased phone adb lost for its USB debugging
/// being off says both. None: a free phone not checked yet.
pub fn phone_words(readiness: Option<Readiness>, holder: Option<&str>) -> Option<(String, Tone)> {
    // A leased phone that adb has lost still asks for its USB debugging: the holder follows.
    if let (Some(holder), Some(Readiness::UsbDebuggingOff)) = (holder, readiness) {
        return Some((
            format!("{USB_DEBUGGING_OFF_WORDS}, leased to {holder}"),
            Tone::Warn,
        ));
    }
    let shown = match (holder, readiness) {
        (Some(_), _) => Shown::Leased,
        (None, Some(readiness)) => Shown::Free(readiness),
        (None, None) => return None,
    };
    let (_, words, tone) = WORDS.iter().find(|(s, _, _)| *s == shown)?;
    Some(match holder {
        Some(holder) => (format!("{words} {holder}"), *tone),
        None => ((*words).to_owned(), *tone),
    })
}

/// The lab as the daemon last checked it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lab {
    /// The lab's clock as the daemon read the lab, in ms since the epoch.
    pub now: i64,
    /// The phones plugged in.
    pub phones: Vec<Phone>,
    /// The emulators the lab runs.
    pub emulators: Vec<Emulator>,
    /// Every stack slot the lab knows.
    pub stacks: Vec<Stack>,
    /// How many requests wait for a device or a slot.
    pub queue: usize,
    /// How many emulators may run at once (`maxEmulators`).
    pub max_emulators: u32,
    /// `MemAvailable`, in MB.
    pub mem_available_mb: i64,
    /// What is free after the CI slice's headroom, in MB; none without a CI slice (#84).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_after_ci_mb: Option<i64>,
    /// Why the lab couldn't be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why the USB bus couldn't be read at the last sweep: the phones with USB debugging off
    /// are then the last ones found, and may be out of date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phone {
    pub serial: String,
    pub model: String,
    /// What its last probe found; none before its first.
    pub readiness: Option<Readiness>,
    /// When that probe began, in ms since the epoch on the lab's clock.
    pub probed: Option<i64>,
    /// The worktree of the lease that holds it.
    pub holder: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emulator {
    pub serial: String,
    /// Finished booting.
    pub ready: bool,
    pub holder: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub project: String,
    pub slot: u32,
    /// Whether its stack ran at the daemon's last look; none before its first.
    pub up: Option<bool>,
    /// devlab started it: one up that devlab didn't start is the owner's.
    pub ours: bool,
    /// The lease or reservation that holds the slot.
    pub holder: Option<String>,
}

/// One phone as the card says it.
struct PhoneLine {
    name: String,
    text: String,
    tone: Tone,
    ready: bool,
    /// Since when its readiness is old, for a free phone's past [`STALE_MS`].
    stale_since: Option<i64>,
}

fn phone_lines(lab: &Lab) -> Vec<PhoneLine> {
    lab.phones
        .iter()
        .map(|phone| {
            let shared = !phone.model.is_empty()
                && lab
                    .phones
                    .iter()
                    .any(|other| other.serial != phone.serial && other.model == phone.model);
            let name = phone_name(&phone.model, &phone.serial, shared);
            let Some((words, tone)) = phone_words(phone.readiness, phone.holder.as_deref()) else {
                return PhoneLine {
                    name,
                    text: "checking".to_owned(),
                    tone: Tone::Muted,
                    ready: false,
                    stale_since: None,
                };
            };
            let stale_since = phone
                .probed
                .filter(|probed| phone.holder.is_none() && lab.now - probed > STALE_MS);
            match stale_since {
                Some(probed) => PhoneLine {
                    name,
                    text: format!("{words}, {} old", ago(lab.now / 1000, probed / 1000)),
                    tone: Tone::Muted,
                    ready: false,
                    stale_since,
                },
                None => PhoneLine {
                    name,
                    text: words,
                    tone,
                    ready: phone.readiness == Some(Readiness::Ready),
                    stale_since,
                },
            }
        })
        .collect()
}

/// How much a tone asks of the owner: the chip takes the most demanding.
fn weight(tone: Tone) -> u8 {
    match tone {
        Tone::Muted => 0,
        Tone::Ok => 1,
        Tone::Busy => 2,
        Tone::Warn => 3,
        Tone::Critical => 4,
    }
}

fn emulator_text(emulator: &Emulator) -> (String, Tone) {
    let state = if emulator.ready { "ready" } else { "booting" };
    match (&emulator.holder, emulator.ready) {
        (Some(holder), _) => (format!("{state}, {holder}"), Tone::Busy),
        (None, true) => (format!("{state}, free"), Tone::Ok),
        (None, false) => (format!("{state}, free"), Tone::Busy),
    }
}

fn stack_text(stack: &Stack) -> (String, Tone) {
    let state = match stack.up {
        Some(true) => "up",
        Some(false) => "down",
        None => "unchecked",
    };
    match (&stack.holder, stack.up) {
        (Some(holder), _) => (format!("{state}, {holder}"), Tone::Busy),
        (None, Some(true)) if !stack.ours => (format!("{state}, yours"), Tone::Ok),
        (None, Some(true)) => (format!("{state}, free"), Tone::Ok),
        (None, _) => (format!("{state}, free"), Tone::Muted),
    }
}

fn stacks_up(lab: &Lab) -> usize {
    lab.stacks.iter().filter(|s| s.up == Some(true)).count()
}

/// `2/2`: phones ready out of plugged in.
fn phones_ready(lines: &[PhoneLine]) -> String {
    let ready = lines.iter().filter(|line| line.ready).count();
    format!("{ready}/{}", lines.len())
}

/// The line's `lab 2/2`.
pub fn line(lab: &Lab) -> String {
    if lab.error.is_some() {
        return "lab ?".to_owned();
    }
    format!("lab {}", phones_ready(&phone_lines(lab)))
}

/// `󰄜 2/2 󰍹 0 󰆼 1`, toned by what asks most of the owner; its tooltip names each phone that
/// needs them and what to do.
pub fn chip(lab: &Lab) -> Chip {
    let label = glyph::PHONE.char.to_string();
    if let Some(error) = &lab.error {
        return Chip {
            id: "lab".to_owned(),
            label,
            text: "?".to_owned(),
            tone: Tone::Muted,
            tooltip: Some(format!("Lab: unknown: {error}")),
        };
    }
    let phones = phone_lines(lab);
    let tones = phones
        .iter()
        .map(|line| line.tone)
        .chain(lab.emulators.iter().map(|e| emulator_text(e).1))
        .chain(lab.stacks.iter().map(|s| stack_text(s).1));
    let tone = tones
        .max_by_key(|tone| weight(*tone))
        .unwrap_or(Tone::Muted);
    let needs: Vec<String> = phones
        .iter()
        .filter(|line| matches!(line.tone, Tone::Warn | Tone::Critical))
        .map(|line| format!("{}: {}", line.name, line.text))
        .collect();
    let mut tooltip = if needs.is_empty() {
        format!(
            "Lab: {} phones ready, {} emulators, {} stacks up",
            phones_ready(&phones),
            lab.emulators.len(),
            stacks_up(lab)
        )
    } else {
        format!("Lab: {}", needs.join("; "))
    };
    if let Some(why) = usb_unread(lab) {
        tooltip.push_str(" · ");
        tooltip.push_str(&why);
    }
    Chip {
        id: "lab".to_owned(),
        label,
        text: format!(
            "{} {} {} {} {}",
            phones_ready(&phones),
            glyph::EMULATOR.char,
            lab.emulators.len(),
            glyph::STACK.char,
            stacks_up(lab)
        ),
        tone,
        tooltip: Some(tooltip),
    }
}

/// What the card and the chip say while the USB bus can't be read.
fn usb_unread(lab: &Lab) -> Option<String> {
    lab.usb_error.as_ref().map(|why| {
        format!(
            "the USB bus could not be read ({why}): a phone with USB debugging off may be missing"
        )
    })
}

fn text(label: &str, text: String, tone: Tone) -> Row {
    Row::Text {
        label: label.to_owned(),
        text,
        tone: Some(tone),
        url: None,
    }
}

/// A row per phone, emulator and stack slot, then the queue and the room left.
pub fn section(lab: &Lab) -> Section {
    let phones = phone_lines(lab);
    let mut rows = Vec::new();
    if lab.error.is_none() {
        // A bus that can't be read says nothing of what is plugged in.
        if phones.is_empty() && lab.usb_error.is_none() {
            rows.push(text("phones", "none plugged in".to_owned(), Tone::Muted));
        } else if phones.is_empty() {
            rows.push(text("phones", "unknown".to_owned(), Tone::Muted));
        }
        for line in &phones {
            rows.push(text(&line.name, line.text.clone(), line.tone));
        }
        for emulator in &lab.emulators {
            let (said, tone) = emulator_text(emulator);
            rows.push(text(&emulator.serial, said, tone));
        }
        for stack in &lab.stacks {
            let (said, tone) = stack_text(stack);
            rows.push(text(
                &format!("{} slot {}", stack.project, stack.slot),
                said,
                tone,
            ));
        }
        rows.push(if lab.queue == 0 {
            text("queue", "empty".to_owned(), Tone::Muted)
        } else {
            text("queue", format!("{} waiting", lab.queue), Tone::Busy)
        });
        let after_ci = lab.mem_after_ci_mb.map_or_else(String::new, |mb| {
            format!(" · {} GB free after CI", crate::lab::memory::gb(mb))
        });
        rows.push(text(
            "room",
            format!(
                "emulators {} of {} · {} GB free{after_ci}",
                lab.emulators.len(),
                lab.max_emulators,
                crate::lab::memory::gb(lab.mem_available_mb)
            ),
            Tone::Muted,
        ));
    }
    Section {
        id: "lab".to_owned(),
        title: "Lab".to_owned(),
        note: None,
        error: lab.error.clone().or_else(|| usb_unread(lab)),
        stale_since: phones
            .iter()
            .filter_map(|line| line.stale_since)
            .min()
            .map(stamp),
        rows,
    }
}
