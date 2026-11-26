//! The bar's Lab chip and card (#72), from the daemon's stored checks: the wording table, the
//! chip's counts and tone, the card's rows and its staleness.

use factory_core::bar::Tone;
use factory_core::bar::lab::{
    Emulator, Lab, Phone, STALE_MS, Shown, Stack, WORDS, chip, line, phone_words, section,
};
use factory_core::lab::ready::Readiness;
use serde_json::{Value, json};

const NOW: i64 = 1_800_000_000_000;

fn phone(serial: &str, model: &str, readiness: Readiness) -> Phone {
    Phone {
        serial: serial.to_owned(),
        model: model.to_owned(),
        readiness: Some(readiness),
        probed: Some(NOW),
        holder: None,
    }
}

fn lab(phones: Vec<Phone>) -> Lab {
    Lab {
        now: NOW,
        phones,
        max_emulators: 2,
        mem_available_mb: 12 * 1024,
        ..Lab::default()
    }
}

fn rows(lab: &Lab) -> Vec<Value> {
    serde_json::to_value(section(lab))
        .ok()
        .and_then(|s| s["rows"].as_array().cloned())
        .unwrap_or_default()
}

fn row(lab: &Lab, label: &str) -> Value {
    rows(lab)
        .into_iter()
        .find(|row| row["label"] == label)
        .unwrap_or(Value::Null)
}

fn tone(tone: Tone) -> Value {
    serde_json::to_value(tone).unwrap_or(Value::Null)
}

#[test]
fn the_wording_and_tones_come_from_one_table_as_the_spec_words_them() {
    let spec = [
        (Shown::Free(Readiness::Ready), "ready", "ok"),
        (Shown::Leased, "leased to", "busy"),
        (Shown::Free(Readiness::Locked), "unlock it", "warn"),
        (Shown::Free(Readiness::Asleep), "wake it", "warn"),
        (
            Shown::Free(Readiness::StayAwakeOff),
            "turn on Stay awake",
            "warn",
        ),
        (
            Shown::Free(Readiness::Unauthorized),
            "allow USB debugging",
            "critical",
        ),
        (Shown::Free(Readiness::Offline), "replug it", "critical"),
    ];
    assert_eq!(WORDS.len(), spec.len());
    for (shown, words, tone_name) in spec {
        let entry = WORDS.iter().find(|(s, _, _)| *s == shown);
        assert_eq!(
            entry.map(|(_, w, t)| (*w, tone(*t))),
            Some((words, json!(tone_name))),
            "{shown:?}"
        );
    }
    for (shown, words, tone_name) in spec {
        let Shown::Free(readiness) = shown else {
            continue;
        };
        assert_eq!(
            phone_words(Some(readiness), None),
            Some((
                words.to_owned(),
                WORDS.iter().find(|(s, _, _)| *s == shown).unwrap().2
            ))
        );
        let card = lab(vec![phone("S1", "Pixel 8", readiness)]);
        assert_eq!(
            row(&card, "Pixel 8"),
            json!({"kind": "text", "label": "Pixel 8", "text": words, "tone": tone_name})
        );
    }
    // A leased phone is its holder's, whatever it was last found, and before its first check.
    for readiness in [None, Some(Readiness::Ready), Some(Readiness::Locked)] {
        assert_eq!(
            phone_words(readiness, Some("app-t1")).map(|(w, t)| (w, tone(t))),
            Some(("leased to app-t1".to_owned(), json!("busy")))
        );
        let mut held = phone("S1", "Pixel 8", Readiness::Ready);
        held.readiness = readiness;
        held.holder = Some("app-t1".to_owned());
        assert_eq!(
            row(&lab(vec![held]), "Pixel 8"),
            json!({"kind": "text", "label": "Pixel 8", "text": "leased to app-t1", "tone": "busy"})
        );
    }
    // A free phone not checked yet has no words of the table's.
    assert_eq!(phone_words(None, None), None);
}

#[test]
fn the_chip_counts_ready_phones_emulators_and_stacks_up_toned_by_the_worst() {
    let mut card = lab(vec![
        phone("S1", "Pixel 8", Readiness::Ready),
        phone("S2", "Pixel 7", Readiness::Ready),
    ]);
    card.emulators = vec![Emulator {
        serial: "emulator-5580".to_owned(),
        ready: true,
        holder: Some("app-t1".to_owned()),
    }];
    card.stacks = vec![Stack {
        project: "app".to_owned(),
        slot: 0,
        up: Some(true),
        ours: true,
        holder: None,
    }];
    let shown = chip(&card);
    assert_eq!(shown.id, "lab");
    assert!(
        shown.text.starts_with("2/2 ") && shown.text.ends_with(" 1 \u{f01bc} 1"),
        "{shown:?}"
    );
    assert_eq!(tone(shown.tone), json!("busy"));
    assert_eq!(
        shown.tooltip.as_deref(),
        Some("Lab: 2/2 phones ready, 1 emulators, 1 stacks up")
    );
    assert_eq!(line(&card), "lab 2/2");

    card.phones[1].readiness = Some(Readiness::Locked);
    card.phones
        .push(phone("S3", "Pixel 6", Readiness::Unauthorized));
    let shown = chip(&card);
    assert!(shown.text.starts_with("1/3 "), "{shown:?}");
    assert_eq!(tone(shown.tone), json!("critical"));
    assert_eq!(
        shown.tooltip.as_deref(),
        Some("Lab: Pixel 7: unlock it; Pixel 6: allow USB debugging")
    );
    assert_eq!(line(&card), "lab 1/3");
}

#[test]
fn a_free_phones_readiness_older_than_five_minutes_shows_its_age_and_stale_since() {
    let mut card = lab(vec![
        phone("S1", "Pixel 8", Readiness::Locked),
        phone("S2", "Pixel 7", Readiness::Ready),
        phone("S3", "Pixel 6", Readiness::Ready),
    ]);
    card.phones[0].probed = Some(NOW - STALE_MS - 60_000);
    // Exactly five minutes is not yet stale.
    card.phones[1].probed = Some(NOW - STALE_MS);
    // A leased phone isn't re-checked: its age says nothing.
    card.phones[2].probed = Some(NOW - 60 * 60_000);
    card.phones[2].holder = Some("app-t1".to_owned());

    assert_eq!(
        row(&card, "Pixel 8"),
        json!({"kind": "text", "label": "Pixel 8", "text": "unlock it, 6m old", "tone": "muted"})
    );
    assert_eq!(row(&card, "Pixel 7")["text"], "ready");
    assert_eq!(row(&card, "Pixel 6")["text"], "leased to app-t1");
    let stale = serde_json::to_value(section(&card)).unwrap_or(Value::Null);
    assert_eq!(stale["stale_since"], "2027-01-15T07:54:00.000Z", "{stale}");
    // A stale warning isn't shown as one; a leased phone counts as its last check found it.
    assert_eq!(line(&card), "lab 2/3");
    assert_eq!(tone(chip(&card).tone), json!("busy"));

    card.phones[0].probed = Some(NOW);
    let fresh = serde_json::to_value(section(&card)).unwrap_or(Value::Null);
    assert!(fresh.get("stale_since").is_none(), "{fresh}");
}

#[test]
fn a_phone_not_yet_checked_says_so() {
    let mut card = lab(vec![phone("S1", "Pixel 8", Readiness::Ready)]);
    card.phones[0].readiness = None;
    card.phones[0].probed = None;
    assert_eq!(row(&card, "Pixel 8")["text"], "checking");
    assert_eq!(row(&card, "Pixel 8")["tone"], "muted");
    card.phones[0].holder = Some("app-t1".to_owned());
    assert_eq!(row(&card, "Pixel 8")["text"], "leased to app-t1");
    assert_eq!(row(&card, "Pixel 8")["tone"], "busy");
}

#[test]
fn two_phones_of_one_model_are_told_apart_by_the_end_of_their_serials() {
    let card = lab(vec![
        phone("FAKEPHONE01", "Pixel 8", Readiness::Ready),
        phone("FAKEPHONE02", "Pixel 8", Readiness::Locked),
    ]);
    assert_eq!(row(&card, "Pixel 8 (NE01)")["text"], "ready");
    assert_eq!(row(&card, "Pixel 8 (NE02)")["text"], "unlock it");
}

#[test]
fn the_card_lists_emulators_stacks_the_queue_and_the_room_left() {
    let mut card = lab(Vec::new());
    card.emulators = vec![
        Emulator {
            serial: "emulator-5580".to_owned(),
            ready: true,
            holder: Some("app-t1".to_owned()),
        },
        Emulator {
            serial: "emulator-5582".to_owned(),
            ready: false,
            holder: None,
        },
    ];
    let stack = |slot, up, ours, holder: Option<&str>| Stack {
        project: "app".to_owned(),
        slot,
        up,
        ours,
        holder: holder.map(str::to_owned),
    };
    card.stacks = vec![
        stack(0, Some(true), false, None),
        stack(1, Some(true), true, None),
        stack(2, Some(false), true, Some("app-t2")),
        stack(3, None, false, None),
    ];
    card.queue = 2;
    let text = |label: &str, text: &str, tone: &str| json!({"kind": "text", "label": label, "text": text, "tone": tone});
    assert_eq!(
        rows(&card),
        vec![
            text("phones", "none plugged in", "muted"),
            text("emulator-5580", "ready, app-t1", "busy"),
            text("emulator-5582", "booting, free", "busy"),
            text("app slot 0", "up, yours", "ok"),
            text("app slot 1", "up, free", "ok"),
            text("app slot 2", "down, app-t2", "busy"),
            text("app slot 3", "unchecked, free", "muted"),
            text("queue", "2 waiting", "busy"),
            text("room", "emulators 2 of 2 · 12.0 GB free", "muted"),
        ]
    );
}

#[test]
fn a_lab_that_cannot_be_read_says_so_on_the_chip_the_card_and_the_line() {
    let card = Lab {
        error: Some("database is locked".to_owned()),
        ..lab(vec![phone("S1", "Pixel 8", Readiness::Ready)])
    };
    let shown = chip(&card);
    assert_eq!(
        (shown.text.as_str(), tone(shown.tone)),
        ("?", json!("muted"))
    );
    assert_eq!(
        shown.tooltip.as_deref(),
        Some("Lab: unknown: database is locked")
    );
    let card_json = serde_json::to_value(section(&card)).unwrap_or(Value::Null);
    assert_eq!(card_json["error"], "database is locked");
    assert_eq!(card_json["rows"], json!([]));
    assert_eq!(line(&card), "lab ?");
}
