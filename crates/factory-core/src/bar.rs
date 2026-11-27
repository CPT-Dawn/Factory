//! The bar feed: what `factory bar` prints. The daemon's snapshot comes in; Zenith's cockpit v1
//! document (`version`, `generated`, `glance`, `sections`) or the one-line summary goes out.
//! The factory measures, the bar displays (ADR 0001): nothing here reads a file or asks a tool.

use serde::{Deserialize, Serialize};

pub mod lab;

use crate::fleet::{Ci, Mark, PullState};
use crate::status::{self, ClaudeState};
use crate::timestamp::Timestamp;
use crate::usage::{UsageReading, WindowKind, WindowState};

/// The cockpit format's version.
pub const VERSION: u32 = 1;

/// A share from which a window's tone is `warn`.
const WARN_AT: u32 = 75;
/// A share from which a window's tone is `critical`.
const CRITICAL_AT: u32 = 90;

/// What the daemon answers the bar's one request with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// When it was taken, in milliseconds since the epoch.
    pub now: i64,
    pub claude: status::Claude,
    #[serde(default)]
    pub codex: status::Codex,
    pub agy: status::Agy,
    /// The config's mark for holding a quota group of agy's, in percent of either window.
    #[serde(default = "agy_hold_default")]
    pub agy_hold_at: f64,
    pub waits: Vec<status::Wait>,
    pub leads: status::Leads,
    /// The watched repos' CI; none configured shows no CI chip or section.
    #[serde(default)]
    pub ci: crate::fleet::Ci,
    /// The lab as the daemon last checked it.
    #[serde(default)]
    pub lab: lab::Lab,
}

fn agy_hold_default() -> f64 {
    crate::quota::AGY_HOLD_AT_PERCENT
}

/// A cockpit v1 document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub version: u32,
    /// RFC 3339.
    pub generated: String,
    pub glance: Vec<Chip>,
    pub sections: Vec<Section>,
}

/// One chip of the bar's glance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chip {
    pub id: String,
    /// A Nerd Font glyph from [`glyph::TABLE`].
    pub label: String,
    pub text: String,
    pub tone: Tone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// One card of the bar's popup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Why its source failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Since when its numbers are old, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_since: Option<String>,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Row {
    Meter {
        label: String,
        /// 0 to 1.
        value: f64,
        text: String,
        tone: Tone,
        /// When the meter resets, RFC 3339.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<String>,
    },
    Text {
        label: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
}

/// The five tones Zenith knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Ok,
    Busy,
    Warn,
    Critical,
    Muted,
}

/// The Nerd Font Material Design glyphs the bar labels its chips with, from the Nerd Fonts
/// cheat sheet. Names go in the tooltips.
pub mod glyph {
    /// A glyph and its cheat-sheet name.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Glyph {
        pub name: &'static str,
        pub char: char,
    }

    pub const CLAUDE: Glyph = Glyph {
        name: "md-robot",
        char: '\u{f06a9}',
    };
    pub const CODEX: Glyph = Glyph {
        name: "md-code_braces",
        char: '\u{f0169}',
    };
    pub const AGY: Glyph = Glyph {
        name: "md-google",
        char: '\u{f02ad}',
    };
    pub const FACTORY: Glyph = Glyph {
        name: "md-factory",
        char: '\u{f020f}',
    };
    pub const CI_PASSING: Glyph = Glyph {
        name: "md-check_circle",
        char: '\u{f05e0}',
    };
    pub const CI_FAILING: Glyph = Glyph {
        name: "md-close_circle",
        char: '\u{f0159}',
    };
    pub const PHONE: Glyph = Glyph {
        name: "md-cellphone",
        char: '\u{f011c}',
    };
    pub const EMULATOR: Glyph = Glyph {
        name: "md-monitor_cellphone",
        char: '\u{f0989}',
    };
    pub const STACK: Glyph = Glyph {
        name: "md-database",
        char: '\u{f01bc}',
    };

    /// Every glyph the bar uses.
    pub const TABLE: [Glyph; 9] = [
        CLAUDE, CODEX, AGY, FACTORY, CI_PASSING, CI_FAILING, PHONE, EMULATOR, STACK,
    ];
}

/// The cockpit v1 document for `snapshot`.
pub fn feed(snapshot: &Snapshot) -> Feed {
    Feed {
        version: VERSION,
        generated: stamp(snapshot.now),
        glance: [
            Some(claude_chip(snapshot)),
            Some(codex_chip(snapshot)),
            Some(agy_chip(snapshot)),
            Some(factory_chip(snapshot)),
            Some(lab::chip(&snapshot.lab)),
            ci_chip(snapshot),
        ]
        .into_iter()
        .flatten()
        .collect(),
        sections: [
            Some(claude_section(snapshot)),
            Some(codex_section(snapshot)),
            Some(agy_section(snapshot)),
            Some(factory_section(snapshot)),
            Some(lab::section(&snapshot.lab)),
            ci_section(snapshot),
        ]
        .into_iter()
        .flatten()
        .collect(),
    }
}

/// `✓`, `✗`, `busy`, or `?` before GitHub answered.
fn ci_mark_text(ci: &Ci) -> &'static str {
    match ci.mark() {
        Mark::Unknown => "?",
        Mark::Failing => "✗",
        Mark::Busy => "busy",
        Mark::Passing => "✓",
    }
}

fn ci_chip(snapshot: &Snapshot) -> Option<Chip> {
    let ci = &snapshot.ci;
    if !ci.shown() {
        return None;
    }
    let mark = ci.mark();
    let (glyph, tone) = match mark {
        Mark::Unknown => (glyph::CI_PASSING, Tone::Muted),
        Mark::Failing => (glyph::CI_FAILING, Tone::Critical),
        Mark::Busy => (glyph::CI_PASSING, Tone::Busy),
        Mark::Passing => (glyph::CI_PASSING, Tone::Ok),
    };
    Some(Chip {
        id: "ci".to_owned(),
        label: glyph.char.to_string(),
        text: ci_mark_text(ci).to_owned(),
        tone,
        tooltip: Some(format!("CI: {}", ci.summary(snapshot.now))),
    })
}

/// Runners busy out of total, the queue and its oldest wait, and each open factory pull
/// request with its link. A failed read keeps the last good numbers, marked stale.
fn ci_section(snapshot: &Snapshot) -> Option<Section> {
    let ci = &snapshot.ci;
    if !ci.shown() {
        return None;
    }
    let mut rows = Vec::new();
    if ci.answered() {
        let tally = ci.tally();
        #[allow(clippy::cast_precision_loss)]
        let value = if tally.total == 0 {
            0.0
        } else {
            tally.busy as f64 / tally.total as f64
        };
        rows.push(Row::Meter {
            label: "runners".to_owned(),
            value,
            text: tally.runners_text(),
            tone: if tally.offline + tally.missing > 0 {
                Tone::Warn
            } else if tally.busy > 0 {
                Tone::Busy
            } else {
                Tone::Ok
            },
            until: None,
        });
        let queue = tally.queue_text(snapshot.now);
        rows.push(if tally.queued == 0 {
            text("queue", queue, Tone::Muted)
        } else {
            text("queue", queue, Tone::Busy)
        });
        let pulls: Vec<Row> = ci
            .repos
            .iter()
            .flat_map(|repo| {
                let name = repo.repo.rsplit('/').next().unwrap_or(&repo.repo);
                repo.pulls.iter().map(move |pull| Row::Text {
                    label: format!("{name}#{}", pull.number),
                    text: pull.state.word().to_owned(),
                    tone: Some(match pull.state {
                        PullState::Passing => Tone::Ok,
                        PullState::Failing => Tone::Critical,
                        PullState::Pending => Tone::Busy,
                    }),
                    url: Some(pull.url.clone()),
                })
            })
            .collect();
        if pulls.is_empty() {
            rows.push(text("PRs", "no open factory PRs".to_owned(), Tone::Muted));
        }
        rows.extend(pulls);
    }
    Some(Section {
        id: "ci".to_owned(),
        title: "CI".to_owned(),
        note: None,
        error: ci.error(),
        stale_since: ci.stale_since().map(stamp),
        rows,
    })
}

/// What `factory bar --json` prints when the daemon can't be asked: one critical chip.
pub fn daemon_down(reason: &str, now: i64) -> Feed {
    Feed {
        version: VERSION,
        generated: stamp(now),
        glance: vec![Chip {
            id: "factory".to_owned(),
            label: glyph::FACTORY.char.to_string(),
            text: "factory daemon down".to_owned(),
            tone: Tone::Critical,
            tooltip: Some(format!("Factory: daemon down: {reason}")),
        }],
        sections: vec![Section {
            id: "factory".to_owned(),
            title: "Factory".to_owned(),
            note: None,
            error: Some(format!("factory daemon down: {reason}")),
            stale_since: None,
            rows: Vec::new(),
        }],
    }
}

/// `claude 12/40 · codex 8/30 · agy 30/63 · 2 leads · lab 2/2 · CI ✓`: each plan's windows'
/// shares (5-hour/weekly, `?` while unknown; agy's the worst of its quota groups, or
/// `unreadable` while its usage is), the live leads (`? leads` while herdr can't tell), the
/// phones ready out of plugged in, then CI's mark when repos are watched.
pub fn line(snapshot: &Snapshot) -> String {
    let shares = |windows: &[UsageReading]| {
        let share = |kind| {
            windows
                .iter()
                .filter(|reading| reading.current)
                .find(|reading| reading.window == kind)
                .map_or_else(
                    || "?".to_owned(),
                    |reading| reading.used_percent.to_string(),
                )
        };
        format!(
            "{}/{}",
            share(WindowKind::FiveHour),
            share(WindowKind::Weekly)
        )
    };
    // A plan that can't be read shows no shares, however old its last reading.
    let codex = match snapshot.codex.unreadable {
        Some(_) => "?/?".to_owned(),
        None => shares(&snapshot.codex.windows),
    };
    let leads = if snapshot.leads.error.is_some() {
        "? leads".to_owned()
    } else {
        plural(live(snapshot).count(), "lead")
    };
    let ci = if snapshot.ci.shown() {
        format!(" · CI {}", ci_mark_text(&snapshot.ci))
    } else {
        String::new()
    };
    let agy_share = |kind| {
        snapshot
            .agy
            .worst_of(kind, snapshot.now)
            .map_or_else(|| "?".to_owned(), |used| used.to_string())
    };
    // An answer that can't be read is said so, apart from a reading not yet taken (`?/?`).
    let agy = if snapshot.agy.guard.is_some() {
        "no guard".to_owned()
    } else if snapshot.agy.error.is_some() {
        "unreadable".to_owned()
    } else {
        format!(
            "{}/{}",
            agy_share(WindowKind::FiveHour),
            agy_share(WindowKind::Weekly)
        )
    };
    format!(
        "claude {} · codex {codex} · agy {agy} · {leads} · {}{ci}",
        shares(&snapshot.claude.windows),
        lab::line(&snapshot.lab),
    )
}

fn stamp(millis: i64) -> String {
    Timestamp::from_unix_millis(millis).to_string()
}

fn current(claude: &status::Claude) -> impl Iterator<Item = &UsageReading> {
    claude.windows.iter().filter(|reading| reading.current)
}

/// The tone of a share in whole percent. Whole percents are rounded down, so a share reaches a
/// whole-percent mark here exactly when it reaches it unrounded: 74.6% is still `ok`.
fn share_tone(used: u32) -> Tone {
    if used >= CRITICAL_AT {
        Tone::Critical
    } else if used >= WARN_AT {
        Tone::Warn
    } else {
        Tone::Ok
    }
}

/// `hold: weekly 91% until 11-30 02:00Z`.
fn hold_note(snapshot: &Snapshot) -> Option<String> {
    let claude = &snapshot.claude;
    if claude.verdict != ClaudeState::Hold {
        return None;
    }
    let what = claude.hold.as_deref().unwrap_or("limit hit");
    Some(match claude.until {
        Some(until) => format!("hold: {what} until {}", status::moment(until, snapshot.now)),
        None => format!("hold: {what}"),
    })
}

fn claude_chip(snapshot: &Snapshot) -> Chip {
    let claude = &snapshot.claude;
    let worst = current(claude).map(|reading| reading.used_percent).max();
    let held = claude.verdict == ClaudeState::Hold;
    let (text, tone) = match (worst, held) {
        (Some(used), true) => (format!("{used}%"), Tone::Critical),
        (Some(used), false) => (format!("{used}%"), share_tone(used)),
        (None, true) => ("hold".to_owned(), Tone::Critical),
        (None, false) => ("?".to_owned(), Tone::Muted),
    };
    let mut tooltip = if claude.windows.is_empty() {
        "Claude: no reading yet".to_owned()
    } else {
        let windows: Vec<String> = claude
            .windows
            .iter()
            .map(|reading| reading.describe(snapshot.now))
            .collect();
        format!("Claude: {}", windows.join(", "))
    };
    if let Some(note) = hold_note(snapshot) {
        tooltip.push_str(" · ");
        tooltip.push_str(&note);
    }
    Chip {
        id: "claude".to_owned(),
        label: glyph::CLAUDE.char.to_string(),
        text,
        tone,
        tooltip: Some(tooltip),
    }
}

/// A meter per window, 5-hour first, whether or not a reading of it was seen.
fn meter_rows(windows: &[UsageReading], now: i64) -> Vec<Row> {
    [WindowKind::FiveHour, WindowKind::Weekly]
        .into_iter()
        .map(|kind| {
            let label = kind.label().to_owned();
            match windows.iter().find(|reading| reading.window == kind) {
                Some(reading) if reading.current => Row::Meter {
                    label,
                    value: f64::from(reading.used_percent) / 100.0,
                    text: format!("{}%", reading.used_percent),
                    tone: share_tone(reading.used_percent),
                    until: Some(stamp(reading.resets)),
                },
                Some(reading) => Row::Meter {
                    label,
                    value: 0.0,
                    text: format!(
                        "unknown, {} old",
                        crate::lab::ago(now / 1000, reading.seen / 1000)
                    ),
                    tone: Tone::Muted,
                    until: Some(stamp(reading.resets)),
                },
                None => Row::Meter {
                    label,
                    value: 0.0,
                    text: "no reading yet".to_owned(),
                    tone: Tone::Muted,
                    until: None,
                },
            }
        })
        .collect()
}

/// Since when the readings that no longer stand have been old.
fn stale_since(windows: &[UsageReading]) -> Option<String> {
    windows
        .iter()
        .filter(|reading| !reading.current)
        .map(|reading| reading.seen)
        .min()
        .map(stamp)
}

fn claude_section(snapshot: &Snapshot) -> Section {
    let claude = &snapshot.claude;
    Section {
        id: "claude".to_owned(),
        title: "Claude".to_owned(),
        note: hold_note(snapshot),
        error: None,
        stale_since: stale_since(&claude.windows),
        rows: meter_rows(&claude.windows, snapshot.now),
    }
}

/// `hold: weekly 91% until 11-30 02:00Z`, while Codex reviews are held.
fn codex_hold_note(snapshot: &Snapshot) -> Option<String> {
    let codex = &snapshot.codex;
    if codex.verdict != ClaudeState::Hold {
        return None;
    }
    let what = codex.hold.as_deref().unwrap_or("limit hit");
    Some(match codex.until {
        Some(until) => format!("hold: {what} until {}", status::moment(until, snapshot.now)),
        None => format!("hold: {what}"),
    })
}

/// The worst window's share, as Claude's chip does; muted `?` while Codex can't be read, so an
/// unreadable source is never a calm 0%, unless a reading that still stands holds dispatch:
/// then the hold is shown, critical, with why the plan could not be read again beside it.
fn codex_chip(snapshot: &Snapshot) -> Chip {
    let codex = &snapshot.codex;
    let note = codex_hold_note(snapshot);
    let held = codex.verdict == ClaudeState::Hold;
    if let Some(line) = codex.unreadable_line().filter(|_| !held) {
        let tooltip = match &note {
            Some(note) => format!("Codex: {line} · {note}"),
            None => format!("Codex: {line}"),
        };
        return Chip {
            id: "codex".to_owned(),
            label: glyph::CODEX.char.to_string(),
            text: "?".to_owned(),
            tone: Tone::Muted,
            tooltip: Some(tooltip),
        };
    }
    let worst = codex
        .windows
        .iter()
        .filter(|reading| reading.current)
        .map(|reading| reading.used_percent)
        .max();
    let (text, tone) = match (worst, held) {
        (Some(used), true) => (format!("{used}%"), Tone::Critical),
        (Some(used), false) => (format!("{used}%"), share_tone(used)),
        (None, true) => ("hold".to_owned(), Tone::Critical),
        (None, false) => ("?".to_owned(), Tone::Muted),
    };
    let mut tooltip = if codex.windows.is_empty() {
        "Codex: no reading yet".to_owned()
    } else {
        let windows: Vec<String> = codex
            .windows
            .iter()
            .map(|reading| reading.describe(snapshot.now))
            .collect();
        format!("Codex: {}", windows.join(", "))
    };
    if let Some(line) = codex.unreadable_line() {
        tooltip.push_str(" · ");
        tooltip.push_str(&line);
    }
    if let Some(note) = note {
        tooltip.push_str(" · ");
        tooltip.push_str(&note);
    }
    Chip {
        id: "codex".to_owned(),
        label: glyph::CODEX.char.to_string(),
        text,
        tone,
        tooltip: Some(tooltip),
    }
}

/// A meter per window, then the plan and the reset credits available. A Codex that can't be
/// read puts the reason in `error` and shows no meters: a reading it could not take is not 0%.
fn codex_section(snapshot: &Snapshot) -> Section {
    let codex = &snapshot.codex;
    let mut rows = Vec::new();
    if codex.unreadable.is_none() {
        rows = meter_rows(&codex.windows, snapshot.now);
        if let Some(plan) = &codex.plan {
            rows.push(text("plan", plan.clone(), Tone::Muted));
        }
        let credits = match codex.reset_credits {
            0 => "none".to_owned(),
            1 => "1 available".to_owned(),
            n => format!("{n} available"),
        };
        rows.push(text("reset credits", credits, Tone::Muted));
    }
    Section {
        id: "codex".to_owned(),
        title: "Codex".to_owned(),
        note: codex_hold_note(snapshot),
        error: codex.unreadable_line(),
        stale_since: stale_since(&codex.windows),
        rows,
    }
}

/// The worst window across agy's quota groups, toned by its share, and critical once it is at
/// the config's hold mark, where routing holds its pool; muted while agy's usage is unreadable
/// or no window stands. Whole percents are rounded down, so a fractional mark counts as reached
/// at the next whole percent.
fn agy_chip(snapshot: &Snapshot) -> Chip {
    let agy = &snapshot.agy;
    let label = glyph::AGY.char.to_string();
    if let Some(guard) = &agy.guard {
        return Chip {
            id: "agy".to_owned(),
            label,
            text: "!".to_owned(),
            tone: Tone::Critical,
            tooltip: Some(guard.clone()),
        };
    }
    if let Some(problem) = agy.problem() {
        return Chip {
            id: "agy".to_owned(),
            label,
            text: "?".to_owned(),
            tone: Tone::Muted,
            tooltip: Some(problem),
        };
    }
    let worst = agy.worst(snapshot.now);
    let held = worst.is_some_and(|used| f64::from(used) >= snapshot.agy_hold_at);
    let (text, tone) = match worst {
        Some(used) if held => (format!("{used}%"), Tone::Critical),
        Some(used) => (format!("{used}%"), share_tone(used)),
        None => ("?".to_owned(), Tone::Muted),
    };
    let mut tooltip = if agy.groups.is_empty() {
        "agy: no reading yet".to_owned()
    } else {
        let groups: Vec<String> = agy
            .groups
            .iter()
            .map(|group| format!("{} {}", group.name, group.describe(snapshot.now, ", ")))
            .collect();
        format!("agy: {}", groups.join("; "))
    };
    if held {
        tooltip.push_str(&format!(" · held from {}%", snapshot.agy_hold_at));
    }
    Chip {
        id: "agy".to_owned(),
        label,
        text,
        tone,
        tooltip: Some(tooltip),
    }
}

/// One block per quota group: its name and windows, then a meter per window.
fn agy_section(snapshot: &Snapshot) -> Section {
    let agy = &snapshot.agy;
    let now = snapshot.now;
    let mut rows = Vec::new();
    for group in &agy.groups {
        rows.push(text(&group.name, group.describe(now, " · "), Tone::Muted));
        for window in &group.windows {
            let label = window.window.label().to_owned();
            rows.push(match window.state {
                WindowState::Used {
                    used_percent,
                    resets,
                } if resets > now => Row::Meter {
                    label,
                    value: f64::from(used_percent) / 100.0,
                    text: format!("{used_percent}%"),
                    tone: share_tone(used_percent),
                    until: Some(stamp(resets)),
                },
                WindowState::Used { resets, .. } => Row::Meter {
                    label,
                    value: 0.0,
                    text: format!(
                        "unknown, {} old",
                        crate::lab::ago(now / 1000, agy.seen.unwrap_or(now) / 1000)
                    ),
                    tone: Tone::Muted,
                    until: Some(stamp(resets)),
                },
                WindowState::Unused => Row::Meter {
                    label,
                    value: 0.0,
                    text: "0%".to_owned(),
                    tone: Tone::Ok,
                    until: None,
                },
                WindowState::Unknown => Row::Meter {
                    label,
                    value: 0.0,
                    text: "unknown".to_owned(),
                    tone: Tone::Muted,
                    until: None,
                },
            });
        }
    }
    let stale =
        agy.groups.iter().flat_map(|group| &group.windows).any(
            |window| matches!(window.state, WindowState::Used { resets, .. } if resets <= now),
        );
    Section {
        id: "agy".to_owned(),
        title: "agy".to_owned(),
        note: None,
        error: agy.problem(),
        stale_since: if stale { agy.seen.map(stamp) } else { None },
        rows,
    }
}

fn live(snapshot: &Snapshot) -> impl Iterator<Item = &status::Lead> {
    snapshot
        .leads
        .leads
        .iter()
        .filter(|lead| !matches!(lead.state.as_str(), "finished" | "dead"))
}

fn needs_owner(lead: &status::Lead) -> bool {
    matches!(lead.state.as_str(), "asking" | "blocked")
}

fn plural(count: usize, one: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {one}s")
    }
}

/// `2 leads`, or `1 need you` while any live lead asks or is blocked.
fn leads_text(snapshot: &Snapshot) -> String {
    let need = live(snapshot).filter(|lead| needs_owner(lead)).count();
    if need > 0 {
        format!("{need} need you")
    } else {
        plural(live(snapshot).count(), "lead")
    }
}

fn factory_chip(snapshot: &Snapshot) -> Chip {
    let label = glyph::FACTORY.char.to_string();
    if let Some(error) = &snapshot.leads.error {
        return Chip {
            id: "factory".to_owned(),
            label,
            text: "? leads".to_owned(),
            tone: Tone::Muted,
            tooltip: Some(format!("Factory: leads unknown: {error}")),
        };
    }
    let count = live(snapshot).count();
    let need = live(snapshot).any(needs_owner);
    let tone = match (need, count) {
        (true, _) => Tone::Warn,
        (false, 0) => Tone::Muted,
        (false, _) => Tone::Busy,
    };
    Chip {
        id: "factory".to_owned(),
        label,
        text: leads_text(snapshot),
        tone,
        tooltip: Some(format!(
            "Factory: {}, {}",
            plural(count, "lead"),
            plural(snapshot.waits.len(), "wait")
        )),
    }
}

fn text(label: &str, text: String, tone: Tone) -> Row {
    Row::Text {
        label: label.to_owned(),
        text,
        tone: Some(tone),
        url: None,
    }
}

fn lead_tone(state: &str) -> Tone {
    match state {
        "working" => Tone::Busy,
        "asking" => Tone::Warn,
        "blocked" | "dead" => Tone::Critical,
        _ => Tone::Muted,
    }
}

fn factory_section(snapshot: &Snapshot) -> Section {
    let mut rows = Vec::new();
    if snapshot.leads.error.is_some() {
        rows.push(text("leads", "unknown".to_owned(), Tone::Muted));
    } else if live(snapshot).next().is_none() {
        rows.push(text("leads", "none".to_owned(), Tone::Muted));
    } else {
        for lead in live(snapshot) {
            let slug = lead.name.strip_prefix("lead-").unwrap_or(&lead.name);
            rows.push(text(slug, lead.status.clone(), lead_tone(&lead.state)));
        }
    }
    if snapshot.waits.is_empty() {
        rows.push(text("waits", "none".to_owned(), Tone::Muted));
    }
    for wait in &snapshot.waits {
        let waiter = wait.lead.as_ref().or(wait.pane.as_ref());
        let what = match waiter {
            Some(waiter) => format!("{waiter} on {} {}", wait.kind, wait.on),
            None => format!("{} {}", wait.kind, wait.on),
        };
        rows.push(text("wait", what, Tone::Muted));
    }
    rows.push(if snapshot.leads.monitor {
        text("steward", "running".to_owned(), Tone::Ok)
    } else {
        text("steward", "stopped".to_owned(), Tone::Muted)
    });
    Section {
        id: "factory".to_owned(),
        title: "Factory".to_owned(),
        note: None,
        error: snapshot.leads.error.clone(),
        stale_since: None,
        rows,
    }
}
