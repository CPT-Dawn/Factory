//! The bar feed: the daemon's snapshot in, Zenith's cockpit v1 document out, checked against
//! the v1 fixture copied from Zenith's `tests/fixtures/cockpit-feed.json`.

use std::collections::BTreeSet;

use factory_core::bar::lab::{Emulator, Lab, Phone, Stack};
use factory_core::bar::{Feed, Snapshot, daemon_down, feed, glyph, line};
use factory_core::claude::ClaudeQuota;
use factory_core::lab::ready::Readiness;
use factory_core::status::{Claude, ClaudeState, Lead, Leads, Wait};
use factory_core::timestamp::Timestamp;
use factory_core::usage::{UsageReading, UsageWindow, WindowKind};
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
    assert_eq!(line(&all[0]), "claude 12/40 · 1 lead · lab 0/0");
    // Phones ready out of plugged in: a stale ready one isn't counted ready.
    assert_eq!(line(&all[1]), "claude 80/40 · 0 leads · lab 1/6");
    // A hold and the leads that need the owner are the chips' to show, not the line's.
    assert_eq!(line(&all[2]), "claude 41/91 · 2 leads · lab 0/0");
    assert_eq!(line(&all[3]), "claude ?/12 · 0 leads · lab 0/0");
    assert_eq!(line(&all[4]), "claude ?/? · ? leads · lab 0/0");
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
