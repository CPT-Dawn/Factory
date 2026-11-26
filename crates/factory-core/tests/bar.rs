//! The bar feed: the daemon's snapshot in, Zenith's cockpit v1 document out, checked against
//! the v1 fixture copied from Zenith's `tests/fixtures/cockpit-feed.json`.

use std::collections::BTreeSet;

use factory_core::bar::lab::{Emulator, Lab, Phone, Stack};
use factory_core::bar::{Feed, Snapshot, daemon_down, feed, glyph, line};
use factory_core::claude::ClaudeQuota;
use factory_core::lab::ready::Readiness;
use factory_core::status::{Agy, Claude, ClaudeState, Codex, Lead, Leads, Wait};
use factory_core::timestamp::Timestamp;
use factory_core::usage::{
    GroupWindow, QuotaGroup, UsageReading, UsageWindow, WindowKind, WindowState,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/cockpit-feed.json");

fn at(text: &str) -> i64 {
    Timestamp::parse(text).map_or(0, Timestamp::unix_millis)
}

fn now() -> i64 {
    at("2026-11-23T04:13:00Z")
}

fn reading(window: WindowKind, used: u32, resets: &str, seen: &str) -> UsageReading {
    UsageReading {
        window,
        used_percent: used,
        resets: at(resets),
        seen: at(seen),
        current: at(resets) > now(),
    }
}

fn five(used: u32) -> UsageReading {
    reading(
        WindowKind::FiveHour,
        used,
        "2026-11-23T07:00:00Z",
        "2026-11-23T04:10:00Z",
    )
}

fn weekly(used: u32) -> UsageReading {
    reading(
        WindowKind::Weekly,
        used,
        "2026-11-30T02:00:00Z",
        "2026-11-23T04:10:00Z",
    )
}

fn going(windows: Vec<UsageReading>) -> Claude {
    Claude {
        verdict: ClaudeState::Go,
        windows,
        ..Claude::default()
    }
}

fn codex(windows: Vec<UsageReading>) -> Codex {
    Codex {
        verdict: ClaudeState::Go,
        windows,
        plan: Some("plus".to_owned()),
        reset_credits: 2,
        ..Codex::default()
    }
}

fn lead(name: &str, state: &str) -> Lead {
    Lead {
        name: name.to_owned(),
        state: state.to_owned(),
        status: state.to_owned(),
        pane: "w1:p1".to_owned(),
        report: None,
        session: None,
        tokens: None,
        asked: false,
        compacting: false,
        subagents: Vec::new(),
    }
}

fn snapshot(claude: Claude, leads: Vec<Lead>) -> Snapshot {
    Snapshot {
        now: now(),
        claude,
        codex: Codex::default(),
        agy: Agy::default(),
        waits: vec![Wait {
            pane: Some("w1:p1".to_owned()),
            lead: Some("lead-a".to_owned()),
            kind: "ci".to_owned(),
            on: "CPT-Dawn/Factory#3".to_owned(),
        }],
        leads: Leads {
            error: None,
            monitor: true,
            leads,
        },
        ci: factory_core::fleet::Ci::default(),
        lab: Lab::default(),
    }
}

/// A lab with a phone of each readiness, a leased one, a stale one, an emulator and stacks.
fn busy_lab() -> Lab {
    let phone = |serial: &str, readiness, probed: i64, holder: Option<&str>| Phone {
        serial: serial.to_owned(),
        model: format!("Pixel {serial}"),
        readiness,
        probed: Some(probed),
        holder: holder.map(str::to_owned),
    };
    Lab {
        now: now(),
        phones: vec![
            phone("1", Some(Readiness::Ready), now(), None),
            phone("2", Some(Readiness::Locked), now(), None),
            phone("3", Some(Readiness::Unauthorized), now(), None),
            phone("4", Some(Readiness::Ready), now() - 10 * 60_000, None),
            phone("5", Some(Readiness::Asleep), now(), Some("app-t1")),
            phone("6", None, now(), None),
        ],
        emulators: vec![Emulator {
            serial: "emulator-5580".to_owned(),
            ready: true,
            holder: Some("app-t2".to_owned()),
        }],
        stacks: vec![Stack {
            project: "app".to_owned(),
            slot: 0,
            up: Some(true),
            ours: false,
            holder: None,
        }],
        queue: 1,
        max_emulators: 2,
        mem_available_mb: 8192,
        error: None,
        usb_error: None,
    }
}

fn with_agy(agy: Agy) -> Snapshot {
    Snapshot {
        agy,
        ..snapshot(going(vec![five(12), weekly(40)]), Vec::new())
    }
}

fn agy_window(kind: WindowKind, state: WindowState) -> GroupWindow {
    GroupWindow {
        window: kind,
        state,
    }
}

fn agy_used(percent: u32, resets: &str) -> WindowState {
    WindowState::Used {
        used_percent: percent,
        resets: at(resets),
    }
}

/// agy's two quota groups: Gemini at 30% and 7%, Claude and GPT at 24% and 63%.
fn agy_read() -> Agy {
    Agy {
        groups: vec![
            QuotaGroup {
                name: "Gemini Models".to_owned(),
                windows: vec![
                    agy_window(WindowKind::FiveHour, agy_used(30, "2026-11-23T07:00:00Z")),
                    agy_window(WindowKind::Weekly, agy_used(7, "2026-11-30T02:00:00Z")),
                ],
            },
            QuotaGroup {
                name: "Claude and GPT models".to_owned(),
                windows: vec![
                    agy_window(WindowKind::FiveHour, agy_used(24, "2026-11-23T06:30:00Z")),
                    agy_window(WindowKind::Weekly, agy_used(63, "2026-11-30T01:00:00Z")),
                ],
            },
        ],
        error: None,
        seen: Some(at("2026-11-23T04:10:00Z")),
    }
}

fn agy_unreadable() -> Agy {
    Agy {
        groups: Vec::new(),
        error: Some("its answer is not JSON".to_owned()),
        seen: Some(at("2026-11-23T04:10:00Z")),
    }
}

fn json(feed: &Feed) -> Value {
    serde_json::to_value(feed).unwrap_or(Value::Null)
}

fn chip<'a>(feed: &'a Value, id: &str) -> &'a Value {
    feed["glance"]
        .as_array()
        .and_then(|chips| chips.iter().find(|chip| chip["id"] == id))
        .unwrap_or(&Value::Null)
}

fn section<'a>(feed: &'a Value, id: &str) -> &'a Value {
    feed["sections"]
        .as_array()
        .and_then(|sections| sections.iter().find(|section| section["id"] == id))
        .unwrap_or(&Value::Null)
}

/// Every snapshot shape the tests know: a go, a warn, a hold, a stale window, nothing seen,
/// leads that need the owner, and herdr unreachable.
fn snapshots() -> Vec<Snapshot> {
    let mut unreachable = snapshot(Claude::default(), Vec::new());
    unreachable.leads.error = Some("herdr is not running".to_owned());
    unreachable.leads.monitor = false;
    let mut readable = snapshot(going(vec![five(12), weekly(40)]), Vec::new());
    readable.codex = codex(vec![five(18), weekly(62)]);
    let mut unreadable = snapshot(Claude::default(), Vec::new());
    unreadable.codex = Codex {
        unreadable: Some("it did not answer within 20s".to_owned()),
        ..Codex::default()
    };
    vec![
        snapshot(
            going(vec![five(12), weekly(40)]),
            vec![lead("lead-a", "working")],
        ),
        Snapshot {
            lab: busy_lab(),
            ..snapshot(going(vec![five(80), weekly(40)]), Vec::new())
        },
        snapshot(
            Claude {
                verdict: ClaudeState::Hold,
                until: Some(at("2026-11-30T02:00:00Z")),
                hold: Some("weekly 91%".to_owned()),
                ..going(vec![five(41), weekly(91)])
            },
            vec![lead("lead-a", "asking"), lead("lead-b", "blocked")],
        ),
        snapshot(
            going(vec![
                reading(
                    WindowKind::FiveHour,
                    80,
                    "2026-11-23T03:00:00Z",
                    "2026-11-23T01:13:00Z",
                ),
                weekly(12),
            ]),
            vec![lead("lead-c", "finished"), lead("lead-d", "dead")],
        ),
        unreachable,
        readable,
        unreadable,
        with_agy(Agy::default()),
        with_agy(agy_read()),
        with_agy(agy_unreadable()),
    ]
}

#[test]
fn the_fixture_reads_as_a_feed() {
    let fixture: Result<Feed, _> = serde_json::from_str(FIXTURE);
    assert!(fixture.is_ok(), "{fixture:?}");
}

/// The keys each kind of object carries in the fixture.
fn keys(fixture: &Value) -> [(BTreeSet<String>, BTreeSet<String>); 4] {
    let names = |object: &Value| -> BTreeSet<String> {
        object
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    };
    let chips: Vec<BTreeSet<String>> = fixture["glance"]
        .as_array()
        .map(|chips| chips.iter().map(names).collect())
        .unwrap_or_default();
    let sections: Vec<&Value> = fixture["sections"]
        .as_array()
        .map(|sections| sections.iter().collect())
        .unwrap_or_default();
    let rows: Vec<&Value> = sections
        .iter()
        .flat_map(|section| section["rows"].as_array().into_iter().flatten())
        .collect();
    let of = |kind: &str| -> Vec<BTreeSet<String>> {
        rows.iter()
            .filter(|row| row["kind"] == kind)
            .map(|row| names(row))
            .collect()
    };
    let span = |sets: Vec<BTreeSet<String>>| {
        let all: BTreeSet<String> = sets.iter().flatten().cloned().collect();
        let each = sets.iter().fold(all.clone(), |common, set| &common & set);
        (all, each)
    };
    [
        span(chips),
        span(sections.into_iter().map(names).collect()),
        span(of("meter")),
        span(of("text")),
    ]
}

#[test]
fn the_builder_writes_the_fixtures_shape() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap_or(Value::Null);
    let [chips, sections, meters, texts] = keys(&fixture);
    let documented: BTreeSet<String> = ["error", "stale_since", "url", "tooltip", "note", "until"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let check = |object: &Value, (all, each): &(BTreeSet<String>, BTreeSet<String>)| {
        let names: BTreeSet<String> = object
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        let known = all | &documented;
        assert!(names.is_subset(&known), "{object} has a key v1 lacks");
        let needed = each - &documented;
        assert!(needed.is_subset(&names), "{object} lacks a key v1 needs");
    };
    let mut feeds: Vec<Value> = snapshots().iter().map(|s| json(&feed(s))).collect();
    feeds.push(json(&daemon_down("connection refused", now())));
    for document in &feeds {
        assert_eq!(document["version"], 1);
        let generated = document["generated"].as_str().unwrap_or_default();
        assert!(Timestamp::parse(generated).is_some(), "{generated}");
        for chip in document["glance"].as_array().into_iter().flatten() {
            check(chip, &chips);
        }
        for section in document["sections"].as_array().into_iter().flatten() {
            check(section, &sections);
            for row in section["rows"].as_array().into_iter().flatten() {
                match row["kind"].as_str() {
                    Some("meter") => {
                        check(row, &meters);
                        let value = row["value"].as_f64().unwrap_or(-1.0);
                        assert!((0.0..=1.0).contains(&value), "{row}");
                    }
                    Some("text") => check(row, &texts),
                    _ => panic!("{row} is no v1 row"),
                }
                if let Some(until) = row.get("until") {
                    assert!(Timestamp::parse(until.as_str().unwrap_or_default()).is_some());
                }
            }
            if let Some(since) = section.get("stale_since") {
                assert!(Timestamp::parse(since.as_str().unwrap_or_default()).is_some());
            }
        }
    }
}

fn tones(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "tone" {
                    found.push(value.as_str().unwrap_or("not a string").to_owned());
                }
                tones(value, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| tones(item, found)),
        _ => {}
    }
}

#[test]
fn every_tone_is_one_zenith_knows() {
    let mut found = Vec::new();
    for snapshot in snapshots() {
        tones(&json(&feed(&snapshot)), &mut found);
    }
    tones(&json(&daemon_down("connection refused", now())), &mut found);
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap_or(Value::Null);
    tones(&fixture, &mut found);
    let known = ["ok", "busy", "warn", "critical", "muted"];
    assert!(found.len() > 20);
    for tone in &found {
        assert!(known.contains(&tone.as_str()), "{tone}");
    }
}

#[test]
fn the_claude_chip_shows_the_worst_window_toned_by_its_share() {
    let tone = |claude: Claude| {
        let document = json(&feed(&snapshot(claude, Vec::new())));
        let chip = chip(&document, "claude");
        (
            chip["text"].as_str().unwrap_or_default().to_owned(),
            chip["tone"].as_str().unwrap_or_default().to_owned(),
        )
    };
    assert_eq!(
        tone(going(vec![five(12), weekly(40)])),
        ("40%".into(), "ok".into())
    );
    assert_eq!(
        tone(going(vec![five(74), weekly(40)])),
        ("74%".into(), "ok".into())
    );
    assert_eq!(
        tone(going(vec![five(75), weekly(40)])),
        ("75%".into(), "warn".into())
    );
    assert_eq!(
        tone(going(vec![five(12), weekly(90)])),
        ("90%".into(), "critical".into())
    );
    assert_eq!(tone(Claude::default()), ("?".into(), "muted".into()));
    let held = Claude {
        verdict: ClaudeState::Hold,
        until: Some(at("2026-11-23T07:00:00Z")),
        hold: Some("limit hit".to_owned()),
        ..going(vec![five(60)])
    };
    assert_eq!(tone(held), ("60%".into(), "critical".into()));

    let document = json(&feed(&snapshot(
        going(vec![five(12), weekly(40)]),
        Vec::new(),
    )));
    let chip = chip(&document, "claude");
    assert_eq!(chip["label"], glyph::CLAUDE.char.to_string());
    assert_eq!(chip["tooltip"], "Claude: 5h 12%, weekly 40%");
}

#[test]
fn the_claude_card_has_a_meter_per_window_until_its_reset() {
    let document = json(&feed(&snapshot(
        going(vec![five(12), weekly(40)]),
        Vec::new(),
    )));
    let card = section(&document, "claude");
    assert_eq!(card["title"], "Claude");
    assert_eq!(
        card["rows"],
        serde_json::json!([
            {"kind": "meter", "label": "5h", "value": 0.12, "text": "12%", "tone": "ok",
             "until": "2026-11-23T07:00:00.000Z"},
            {"kind": "meter", "label": "weekly", "value": 0.4, "text": "40%", "tone": "ok",
             "until": "2026-11-30T02:00:00.000Z"},
        ])
    );
    assert!(card.get("stale_since").is_none());
}

#[test]
fn a_window_past_its_reset_is_unknown_and_marks_the_card_stale() {
    let document = json(&feed(&snapshots()[3]));
    let card = section(&document, "claude");
    assert_eq!(card["stale_since"], "2026-11-23T01:13:00.000Z");
    assert_eq!(
        card["rows"][0],
        serde_json::json!({"kind": "meter", "label": "5h", "value": 0.0,
            "text": "unknown, 3h old", "tone": "muted", "until": "2026-11-23T03:00:00.000Z"})
    );
    assert_eq!(chip(&document, "claude")["text"], "12%");
}

#[test]
fn a_window_never_seen_still_has_its_meter() {
    let rows = |claude: Claude| {
        section(&json(&feed(&snapshot(claude, Vec::new()))), "claude")["rows"].clone()
    };
    let unseen = |label: &str| {
        serde_json::json!({"kind": "meter", "label": label, "value": 0.0,
            "text": "no reading yet", "tone": "muted"})
    };
    let partial = rows(going(vec![five(12)]));
    assert_eq!(partial.as_array().map(Vec::len), Some(2), "{partial}");
    assert_eq!(partial[0]["until"], "2026-11-23T07:00:00.000Z");
    assert_eq!(partial[1], unseen("weekly"));
    let weekly_only = rows(going(vec![weekly(40)]));
    assert_eq!(weekly_only[0], unseen("5h"));
    assert_eq!(weekly_only[1]["text"], "40%");
    assert_eq!(
        rows(Claude::default()),
        serde_json::json!([unseen("5h"), unseen("weekly")])
    );
}

#[test]
fn the_chip_tones_a_share_by_what_was_spent_not_its_rounding() {
    // From the quota keeper's own record, as the daemon's snapshot carries it.
    let chip_of = |five_hour: f64| {
        let mut quota = ClaudeQuota::default();
        let now = Timestamp::from_unix_millis(now());
        quota.see(
            &[UsageWindow {
                kind: WindowKind::FiveHour,
                used_percent: five_hour,
                resets: Timestamp::from_unix_millis(at("2026-11-23T07:00:00Z")),
                seen: now,
            }],
            now,
        );
        let document = json(&feed(&snapshot(quota.record(now, 90.0), Vec::new())));
        let chip = chip(&document, "claude").clone();
        (
            chip["text"].as_str().unwrap_or_default().to_owned(),
            chip["tone"].as_str().unwrap_or_default().to_owned(),
        )
    };
    assert_eq!(chip_of(74.6), ("74%".into(), "ok".into()));
    assert_eq!(chip_of(75.0), ("75%".into(), "warn".into()));
    assert_eq!(chip_of(89.6), ("89%".into(), "warn".into()));
    assert_eq!(chip_of(90.0), ("90%".into(), "critical".into()));
    assert_eq!(chip_of(99.9), ("99%".into(), "critical".into()));
}

#[test]
fn a_hold_is_noted_on_the_card() {
    let document = json(&feed(&snapshots()[2]));
    assert_eq!(
        section(&document, "claude")["note"],
        "hold: weekly 91% until 11-30 02:00Z"
    );
    assert_eq!(chip(&document, "claude")["tone"], "critical");
}

#[test]
fn the_factory_chip_counts_live_leads_or_those_that_need_the_owner() {
    let chip_of = |snapshot: &Snapshot| {
        let document = json(&feed(snapshot));
        let chip = chip(&document, "factory").clone();
        (
            chip["text"].as_str().unwrap_or_default().to_owned(),
            chip["tone"].as_str().unwrap_or_default().to_owned(),
        )
    };
    let all = snapshots();
    assert_eq!(chip_of(&all[0]), ("1 lead".into(), "busy".into()));
    assert_eq!(chip_of(&all[1]), ("0 leads".into(), "muted".into()));
    assert_eq!(chip_of(&all[2]), ("2 need you".into(), "warn".into()));
    assert_eq!(chip_of(&all[3]), ("0 leads".into(), "muted".into()));
    let document = json(&feed(&all[0]));
    let chip = chip(&document, "factory");
    assert_eq!(chip["label"], glyph::FACTORY.char.to_string());
    assert_eq!(chip["tooltip"], "Factory: 1 lead, 1 wait");
}

#[test]
fn the_factory_card_lists_leads_waits_and_the_steward() {
    let document = json(&feed(&snapshots()[0]));
    assert_eq!(
        section(&document, "factory")["rows"],
        serde_json::json!([
            {"kind": "text", "label": "a", "text": "working", "tone": "busy"},
            {"kind": "text", "label": "wait", "text": "lead-a on ci CPT-Dawn/Factory#3",
             "tone": "muted"},
            {"kind": "text", "label": "steward", "text": "running", "tone": "ok"},
        ])
    );
    let unreachable = json(&feed(&snapshots()[4]));
    let card = section(&unreachable, "factory");
    assert_eq!(card["error"], "herdr is not running");
    assert_eq!(
        card["rows"][0],
        serde_json::json!(
        {"kind": "text", "label": "leads", "text": "unknown", "tone": "muted"})
    );
    assert_eq!(
        card["rows"][2],
        serde_json::json!(
        {"kind": "text", "label": "steward", "text": "stopped", "tone": "muted"})
    );
}

#[test]
fn the_line_names_both_windows_and_the_leads() {
    let all = snapshots();
    assert_eq!(
        line(&all[0]),
        "claude 12/40 · codex ?/? · agy ?/? · 1 lead · lab 0/0"
    );
    // Phones ready out of plugged in: a stale ready one isn't counted ready.
    assert_eq!(
        line(&all[1]),
        "claude 80/40 · codex ?/? · agy ?/? · 0 leads · lab 1/6"
    );
    // A hold and the leads that need the owner are the chips' to show, not the line's.
    assert_eq!(
        line(&all[2]),
        "claude 41/91 · codex ?/? · agy ?/? · 2 leads · lab 0/0"
    );
    assert_eq!(
        line(&all[3]),
        "claude ?/12 · codex ?/? · agy ?/? · 0 leads · lab 0/0"
    );
    assert_eq!(
        line(&all[4]),
        "claude ?/? · codex ?/? · agy ?/? · ? leads · lab 0/0"
    );
}

#[test]
fn a_daemon_down_feed_is_one_critical_chip() {
    let document = json(&daemon_down("connection refused", now()));
    assert_eq!(document["generated"], "2026-11-23T04:13:00.000Z");
    assert_eq!(
        document["glance"],
        serde_json::json!([{"id": "factory", "label": glyph::FACTORY.char.to_string(),
            "text": "factory daemon down", "tone": "critical",
            "tooltip": "Factory: daemon down: connection refused"}])
    );
    assert_eq!(
        section(&document, "factory")["error"],
        "factory daemon down: connection refused"
    );
}

#[test]
fn the_glyph_table_holds_one_codepoint_per_name() {
    let names: BTreeSet<&str> = glyph::TABLE.iter().map(|glyph| glyph.name).collect();
    let chars: BTreeSet<char> = glyph::TABLE.iter().map(|glyph| glyph.char).collect();
    assert_eq!(names.len(), glyph::TABLE.len());
    assert_eq!(chars.len(), glyph::TABLE.len());
    assert_eq!(glyph::CLAUDE.name, "md-robot");
    assert_eq!(glyph::CLAUDE.char, '\u{f06a9}');
    assert_eq!(glyph::FACTORY.char, '\u{f020f}');
}

fn codex_chip(codex: Codex) -> (String, String) {
    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = codex;
    chip_of(&json(&feed(&shot)), "codex")
}

#[test]
fn the_agy_chip_shows_the_worst_window_across_its_quota_groups() {
    let document = json(&feed(&with_agy(agy_read())));
    let chip = chip(&document, "agy");
    assert_eq!(chip["text"], "63%", "{chip}");
    assert_eq!(chip["tone"], "ok", "{chip}");
    assert_eq!(chip["label"], glyph::AGY.char.to_string());
    assert_eq!(
        chip["tooltip"],
        "agy: Gemini Models 5h 30%, weekly 7%; Claude and GPT models 5h 24%, weekly 63%"
    );

    let mut hot = agy_read();
    hot.groups[0].windows[0].state = agy_used(91, "2026-11-23T07:00:00Z");
    let document = json(&feed(&with_agy(hot)));
    assert_eq!(chip_of(&document, "agy"), ("91%".into(), "critical".into()));
}

fn chip_of(document: &Value, id: &str) -> (String, String) {
    let chip = chip(document, id);
    (
        chip["text"].as_str().unwrap_or_default().to_owned(),
        chip["tone"].as_str().unwrap_or_default().to_owned(),
    )
}

#[test]
fn the_codex_chip_shows_the_worst_window_toned_by_its_share() {
    assert_eq!(
        codex_chip(codex(vec![five(18), weekly(62)])),
        ("62%".into(), "ok".into())
    );
    assert_eq!(
        codex_chip(codex(vec![five(80), weekly(62)])),
        ("80%".into(), "warn".into())
    );
    assert_eq!(
        codex_chip(codex(vec![five(18), weekly(90)])),
        ("90%".into(), "critical".into())
    );
    // A window past its reset is not a reading.
    let past = reading(
        WindowKind::FiveHour,
        95,
        "2026-11-23T03:00:00Z",
        "2026-11-23T01:13:00Z",
    );
    assert_eq!(
        codex_chip(codex(vec![past, weekly(12)])),
        ("12%".into(), "ok".into())
    );
    assert_eq!(codex_chip(Codex::default()), ("?".into(), "muted".into()));
    let held = Codex {
        verdict: ClaudeState::Hold,
        until: Some(at("2026-11-23T07:00:00Z")),
        hold: Some("5h 93%".to_owned()),
        ..codex(vec![five(93)])
    };
    assert_eq!(codex_chip(held), ("93%".into(), "critical".into()));

    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = codex(vec![five(18), weekly(62)]);
    let document = json(&feed(&shot));
    let chip = chip(&document, "codex");
    assert_eq!(chip["label"], glyph::CODEX.char.to_string());
    assert_eq!(chip["tooltip"], "Codex: 5h 18%, weekly 62%");
}

#[test]
fn an_unreadable_codex_whose_standing_reading_still_holds_shows_the_hold_critical() {
    let held = Codex {
        verdict: ClaudeState::Hold,
        until: Some(at("2026-11-23T07:00:00Z")),
        hold: Some("5h 93%".to_owned()),
        unreadable: Some("it did not answer within 20s".to_owned()),
        ..codex(vec![five(93)])
    };
    assert_eq!(codex_chip(held.clone()), ("93%".into(), "critical".into()));

    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = held;
    let document = json(&feed(&shot));
    let tooltip = chip(&document, "codex")["tooltip"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        tooltip.contains("codex usage unreadable: it did not answer within 20s"),
        "{tooltip}"
    );
    assert!(tooltip.contains("hold: 5h 93% until"), "{tooltip}");

    // Unreadable with nothing holding stays a muted ?.
    let open = Codex {
        unreadable: Some("it did not answer within 20s".to_owned()),
        ..codex(vec![five(18)])
    };
    assert_eq!(codex_chip(open), ("?".into(), "muted".into()));
}

#[test]
fn an_unreadable_agy_answer_is_a_muted_chip_and_a_card_with_an_error_never_a_share() {
    let document = json(&feed(&with_agy(agy_unreadable())));

    assert_eq!(chip_of(&document, "agy"), ("?".into(), "muted".into()));
    assert_eq!(
        chip(&document, "agy")["tooltip"],
        "agy usage unreadable: its answer is not JSON"
    );
    let card = section(&document, "agy");
    assert_eq!(
        card["error"], "agy usage unreadable: its answer is not JSON",
        "{card}"
    );
    assert_eq!(card["rows"], serde_json::json!([]), "{card}");
    assert!(!document.to_string().contains("100%"), "{document}");
}

#[test]
fn the_agy_card_has_a_block_per_quota_group_with_a_meter_per_window() {
    let document = json(&feed(&with_agy(agy_read())));
    let card = section(&document, "agy");

    assert_eq!(card["title"], "agy");
    assert_eq!(
        card["rows"],
        serde_json::json!([
            {"kind": "text", "label": "Gemini Models", "text": "5h 30% · weekly 7%", "tone": "muted"},
            {"kind": "meter", "label": "5h", "value": 0.3, "text": "30%", "tone": "ok",
             "until": "2026-11-23T07:00:00.000Z"},
            {"kind": "meter", "label": "weekly", "value": 0.07, "text": "7%", "tone": "ok",
             "until": "2026-11-30T02:00:00.000Z"},
            {"kind": "text", "label": "Claude and GPT models", "text": "5h 24% · weekly 63%", "tone": "muted"},
            {"kind": "meter", "label": "5h", "value": 0.24, "text": "24%", "tone": "ok",
             "until": "2026-11-23T06:30:00.000Z"},
            {"kind": "meter", "label": "weekly", "value": 0.63, "text": "63%", "tone": "ok",
             "until": "2026-11-30T01:00:00.000Z"},
        ])
    );
}

#[test]
fn an_unused_agy_window_is_zero_and_an_unknown_one_is_muted_never_zero() {
    let mut agy = agy_read();
    agy.groups[1].windows = vec![
        agy_window(WindowKind::FiveHour, WindowState::Unused),
        agy_window(WindowKind::Weekly, WindowState::Unknown),
    ];
    let document = json(&feed(&with_agy(agy)));
    let card = section(&document, "agy");
    let rows = card["rows"].as_array().cloned().unwrap_or_default();

    assert_eq!(
        rows[4],
        serde_json::json!({"kind": "meter", "label": "5h", "value": 0.0, "text": "0%", "tone": "ok"})
    );
    assert_eq!(
        rows[5],
        serde_json::json!({"kind": "meter", "label": "weekly", "value": 0.0, "text": "unknown", "tone": "muted"})
    );
    assert_eq!(
        rows[3]["text"], "5h 0% · weekly unknown",
        "the block says what its windows say"
    );
    // The worst window is of those it can place: Gemini's 30%.
    assert_eq!(chip_of(&document, "agy"), ("30%".into(), "ok".into()));
}

#[test]
fn an_agy_window_past_its_reset_is_unknown_with_the_age_of_the_reading() {
    let mut agy = agy_read();
    agy.groups[0].windows[0].state = agy_used(80, "2026-11-23T03:00:00Z");
    agy.seen = Some(at("2026-11-23T01:13:00Z"));
    let document = json(&feed(&with_agy(agy)));
    let card = section(&document, "agy");

    assert_eq!(card["stale_since"], "2026-11-23T01:13:00.000Z", "{card}");
    assert_eq!(
        card["rows"][1],
        serde_json::json!({"kind": "meter", "label": "5h", "value": 0.0,
            "text": "unknown, 3h old", "tone": "muted", "until": "2026-11-23T03:00:00.000Z"}),
    );
    assert_eq!(chip_of(&document, "agy"), ("63%".into(), "ok".into()));
}

#[test]
fn the_bar_line_gives_agys_worst_share_of_each_window_after_claudes() {
    let line_of = |agy: Agy| line(&with_agy(agy));
    assert_eq!(
        line_of(agy_read()),
        "claude 12/40 · codex ?/? · agy 30/63 · 0 leads · lab 0/0"
    );
    assert_eq!(
        line_of(agy_unreadable()),
        "claude 12/40 · codex ?/? · agy unreadable · 0 leads · lab 0/0"
    );
    assert_eq!(
        line_of(Agy::default()),
        "claude 12/40 · codex ?/? · agy ?/? · 0 leads · lab 0/0"
    );
}

#[test]
fn the_agy_chip_comes_after_claudes_and_before_the_factorys() {
    let document = json(&feed(&with_agy(agy_read())));
    let ids: Vec<&str> = document["glance"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|chip| chip["id"].as_str())
        .collect();
    assert_eq!(ids, ["claude", "codex", "agy", "factory", "lab"]);
    let ids: Vec<&str> = document["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|section| section["id"].as_str())
        .collect();
    assert_eq!(ids, ["claude", "codex", "agy", "factory", "lab"]);
}

#[test]
fn the_codex_card_has_a_meter_per_window_the_plan_and_the_reset_credits() {
    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = codex(vec![five(18), weekly(62)]);
    let document = json(&feed(&shot));
    let card = section(&document, "codex");
    assert_eq!(card["title"], "Codex");
    assert_eq!(
        card["rows"],
        serde_json::json!([
            {"kind": "meter", "label": "5h", "value": 0.18, "text": "18%", "tone": "ok",
             "until": "2026-11-23T07:00:00.000Z"},
            {"kind": "meter", "label": "weekly", "value": 0.62, "text": "62%", "tone": "ok",
             "until": "2026-11-30T02:00:00.000Z"},
            {"kind": "text", "label": "plan", "text": "plus", "tone": "muted"},
            {"kind": "text", "label": "reset credits", "text": "2 available", "tone": "muted"},
        ])
    );
    assert!(card.get("error").is_none());

    shot.codex.reset_credits = 0;
    let document = json(&feed(&shot));
    let rows = section(&document, "codex")["rows"].clone();
    assert_eq!(
        rows[3],
        serde_json::json!({"kind": "text", "label": "reset credits", "text": "none", "tone": "muted"})
    );
}

#[test]
fn a_codex_hold_is_noted_on_its_card() {
    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = Codex {
        verdict: ClaudeState::Hold,
        until: Some(at("2026-11-30T02:00:00Z")),
        hold: Some("weekly 92%".to_owned()),
        ..codex(vec![five(18), weekly(92)])
    };
    let document = json(&feed(&shot));
    assert_eq!(
        section(&document, "codex")["note"],
        "hold: weekly 92% until 11-30 02:00Z"
    );
    assert_eq!(
        chip(&document, "codex")["tooltip"],
        "Codex: 5h 18%, weekly 92% · hold: weekly 92% until 11-30 02:00Z"
    );
}

#[test]
fn an_unreadable_codex_is_a_muted_chip_and_an_error_card_never_zero() {
    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = Codex {
        unreadable: Some("it did not answer within 20s".to_owned()),
        ..codex(vec![five(18), weekly(62)])
    };
    let document = json(&feed(&shot));

    let chip = chip(&document, "codex");
    assert_eq!(chip["text"], "?");
    assert_eq!(chip["tone"], "muted");
    assert_eq!(
        chip["tooltip"],
        "Codex: codex usage unreadable: it did not answer within 20s"
    );
    let card = section(&document, "codex");
    assert_eq!(
        card["error"],
        "codex usage unreadable: it did not answer within 20s"
    );
    assert_eq!(card["rows"], serde_json::json!([]));
}

#[test]
fn the_line_names_codexs_windows_and_hides_them_while_unreadable() {
    let mut shot = snapshot(Claude::default(), Vec::new());
    shot.codex = codex(vec![five(8), weekly(30)]);
    assert_eq!(
        line(&shot),
        "claude ?/? · codex 8/30 · agy ?/? · 0 leads · lab 0/0"
    );

    shot.codex.unreadable = Some("it is not there".to_owned());
    assert_eq!(
        line(&shot),
        "claude ?/? · codex ?/? · agy ?/? · 0 leads · lab 0/0"
    );
}
