//! The bar's Lab chip and card (#72): phones ready out of plugged in, emulators and stacks,
//! from the daemon's stored checks. The phone watcher re-checks each free phone's readiness
//! and leaves leased ones alone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::lab::{Agent, for_owner, lab, owned, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Sandbox, SetOnDrop, run};

const PIXEL: &str = "plug FAKEPHONE01 device usb:1-1 model:Pixel_8";

/// The golden world with phones re-checked ten times a second, its daemon running.
fn watching() -> (Sandbox, World) {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""readinessCheckSec": 0.1"#);
    let wt = world.worktree("plain-bar");
    let status = run(crate::lab::devlab(&sandbox, &wt, &["status"]));
    assert_eq!(status.code, 0, "{status:?}");
    (sandbox, world)
}

fn feed(sandbox: &Sandbox) -> Value {
    let out = sandbox.factory(&["bar", "--json"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

fn by_id<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == id))
        .unwrap_or(&Value::Null)
}

/// The Lab card's row labelled `label`.
fn row(feed: &Value, label: &str) -> Value {
    by_id(&feed["sections"], "lab")["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["label"] == label))
        .cloned()
        .unwrap_or(Value::Null)
}

/// How many adb calls were made to phone `serial`.
fn calls_to(sandbox: &Sandbox, serial: &str) -> usize {
    sandbox
        .calls("adb")
        .iter()
        .filter(|c| c.first().is_some_and(|a| a == "-s") && c.get(1).is_some_and(|s| s == serial))
        .count()
}

/// How many times phone `serial`'s screen was probed.
fn probes(sandbox: &Sandbox, serial: &str) -> usize {
    sandbox
        .calls("adb")
        .iter()
        .filter(|c| c.get(1).is_some_and(|s| s == serial))
        .filter(|c| c.last().is_some_and(|cmd| cmd.contains("dumpsys power")))
        .count()
}

/// Polls the feed until `done` accepts it; the last feed read either way.
fn feed_until(sandbox: &Sandbox, done: impl Fn(&Value) -> bool) -> (bool, Value) {
    let mut last = Value::Null;
    let ok = poll_until(|| {
        last = feed(sandbox);
        done(&last)
    });
    (ok, last)
}

#[test]
fn a_free_phone_that_locks_shows_unlock_it_until_it_is_ready_again() {
    let (sandbox, world) = watching();
    world.apply(PIXEL).unwrap();
    let (ready, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(ready, "{last}");
    assert_eq!(row(&last, "Pixel 8")["tone"], "ok", "{last}");

    world.apply("lock FAKEPHONE01").unwrap();
    let (locked, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "unlock it");
    assert!(locked, "{last}");
    assert_eq!(row(&last, "Pixel 8")["tone"], "warn", "{last}");
    let chip = by_id(&last["glance"], "lab");
    assert_eq!(chip["tone"], "warn", "{last}");
    assert!(
        chip["tooltip"]
            .as_str()
            .is_some_and(|t| t.contains("Pixel 8: unlock it")),
        "{last}"
    );

    world.apply("unlock FAKEPHONE01").unwrap();
    let (cleared, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(cleared, "{last}");
}

#[test]
fn a_leased_phone_gets_no_recheck_and_shows_its_holder() {
    let (sandbox, world) = watching();
    world.apply(PIXEL).unwrap();
    world
        .apply("plug FAKEPHONE02 device usb:1-2 model:Pixel_7")
        .unwrap();
    let (ready, last) = feed_until(&sandbox, |f| {
        row(f, "Pixel 8")["text"] == "ready" && row(f, "Pixel 7")["text"] == "ready"
    });
    assert!(ready, "{last}");
    let owner = Agent::start();
    let held = run(for_owner(
        &sandbox,
        &world.worktree("plain-held"),
        owner.pid(),
        &["acquire", "--phone"],
    ));
    assert_eq!(held.code, 0, "{held:?}");
    let (leased, free) = if held.stdout.contains("ANDROID_SERIAL=FAKEPHONE01") {
        (("FAKEPHONE01", "Pixel 8"), ("FAKEPHONE02", "Pixel 7"))
    } else {
        (("FAKEPHONE02", "Pixel 7"), ("FAKEPHONE01", "Pixel 8"))
    };
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, leased.1)["text"] == "leased to plain-held"
    });
    assert!(shown, "{last}");
    assert_eq!(row(&last, leased.1)["tone"], "busy", "{last}");

    world.apply(&format!("lock {}", leased.0)).unwrap();
    let before = calls_to(&sandbox, leased.0);
    let free_before = probes(&sandbox, free.0);
    assert!(
        poll_until(|| probes(&sandbox, free.0) >= free_before + 10),
        "the free phone was probed {} more times",
        probes(&sandbox, free.0) - free_before
    );

    assert_eq!(calls_to(&sandbox, leased.0), before);

    // adb's stream ends and comes back: the free phone is prepped and probed again, as it may
    // have been replugged meanwhile, and the leased one is still its holder's.
    world.restart_trackers(|| {});
    let free_before = probes(&sandbox, free.0);
    assert!(poll_until(|| probes(&sandbox, free.0) >= free_before + 5));
    assert_eq!(
        calls_to(&sandbox, leased.0),
        before,
        "{:?}",
        sandbox.calls("adb")
    );
    let last = feed(&sandbox);
    assert_eq!(
        row(&last, leased.1)["text"],
        "leased to plain-held",
        "{last}"
    );
    assert_eq!(by_id(&last["glance"], "lab")["tone"], "busy", "{last}");
}

#[test]
fn a_readiness_older_than_five_minutes_shows_its_age_and_marks_the_card_stale() {
    let (sandbox, world) = lab();
    let skew = sandbox.path("clock-skew");
    let sandbox = sandbox.env("FACTORY_CLOCK_SKEW_FILE", &skew);
    // The default cadence: two minutes, so no re-check comes within the test.
    let wt = world.worktree("plain-stale");
    assert_eq!(run(crate::lab::devlab(&sandbox, &wt, &["status"])).code, 0);
    world.apply(PIXEL).unwrap();
    let (ready, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(ready, "{last}");
    assert!(
        by_id(&last["sections"], "lab").get("stale_since").is_none(),
        "{last}"
    );

    std::fs::write(&skew, "360").expect("move the clock on");
    let last = feed(&sandbox);
    assert_eq!(row(&last, "Pixel 8")["text"], "ready, 6m old", "{last}");
    assert_eq!(row(&last, "Pixel 8")["tone"], "muted", "{last}");
    assert!(
        by_id(&last["sections"], "lab")["stale_since"].is_string(),
        "{last}"
    );
}

#[test]
fn emulators_show_their_state_and_holder_and_the_card_its_room() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-emu");
    let held = owned(&sandbox, &wt, &["acquire", "--emulator", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let last = feed(&sandbox);
    assert_eq!(
        row(&last, "emulator-5580")["text"],
        "ready, plain-emu",
        "{last}"
    );
    assert_eq!(row(&last, "emulator-5580")["tone"], "busy", "{last}");
    let room = row(&last, "room")["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    // One running of a cap of one, and the host's memory free after CI's headroom (#84).
    let free = room
        .strip_prefix("emulators 1 of 1 · ")
        .and_then(|rest| rest.strip_suffix(" GB free after CI"));
    assert!(free.is_some_and(|gb| gb.parse::<f64>().is_ok()), "{last}");
    let chip = by_id(&last["glance"], "lab");
    assert!(
        chip["text"].as_str().is_some_and(|t| t.contains(" 1 ")),
        "{last}"
    );
}

#[test]
fn a_stack_started_outside_the_factory_shows_as_yours() {
    let (sandbox, world) = watching();
    let wt = world.worktree("demo-outside");
    let held = owned(&sandbox, &wt, &["acquire", "--no-device", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "demo slot 0")["text"] == "down, demo-outside"
    });
    assert!(shown, "{last}");
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);

    world.apply("container supabase_db_demo").unwrap();
    let (yours, last) = feed_until(&sandbox, |f| row(f, "demo slot 0")["text"] == "up, yours");
    assert!(yours, "{last}");
    assert_eq!(row(&last, "demo slot 0")["tone"], "ok", "{last}");
    assert!(
        by_id(&last["glance"], "lab")["text"]
            .as_str()
            .is_some_and(|t| t.ends_with(" 1")),
        "{last}"
    );
}

#[test]
fn a_stack_started_outside_the_factory_on_a_slot_never_leased_shows_as_yours() {
    let (sandbox, world) = watching();
    let wt = world.worktree("demo-seen");
    let held = owned(&sandbox, &wt, &["acquire", "--no-device", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);
    // demo has two slots; slot 1 was never leased.
    let (listed, last) = feed_until(&sandbox, |f| row(f, "demo slot 1")["text"] == "down, free");
    assert!(listed, "{last}");

    world.apply("container supabase_db_demo_s1").unwrap();
    let (yours, last) = feed_until(&sandbox, |f| row(f, "demo slot 1")["text"] == "up, yours");
    assert!(yours, "{last}");
}

#[test]
fn the_bar_reads_the_lab_without_writing_to_it() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-read");
    let held = owned(&sandbox, &wt, &["acquire", "--no-device", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    // The grant's event is in the ledger: no write of the daemon's own is left to make.
    assert!(poll_until(|| std::fs::read_to_string(sandbox.ledger())
        .is_ok_and(|ledger| ledger.contains("demo-read"))));
    let (seen, last) = feed_until(&sandbox, |f| {
        row(f, "demo slot 0")["text"] != "unchecked, demo-read"
    });
    assert!(seen, "{last}");

    // Another writer holds the store: a bar that wrote would wait for it, one that reads doesn't.
    let db = rusqlite::Connection::open(sandbox.path("state/factory/lab.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let last = feed(&sandbox);
    let took = started.elapsed();
    db.execute_batch("ROLLBACK").unwrap();

    assert_eq!(
        row(&last, "demo slot 0")["text"],
        "down, demo-read",
        "{last}"
    );
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[test]
fn the_watcher_keeps_its_rechecks_while_adbs_list_keeps_changing() {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""readinessCheckSec": 0.3"#);
    let wt = world.worktree("plain-churn");
    assert_eq!(run(crate::lab::devlab(&sandbox, &wt, &["status"])).code, 0);
    world.apply(PIXEL).unwrap();
    let (ready, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(ready, "{last}");

    let done = AtomicBool::new(false);
    let locked = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
                world
                    .apply("plug FAKEPHONE02 device usb:1-2 model:Pixel_7")
                    .unwrap();
                world.apply("unplug FAKEPHONE02").unwrap();
            }
        });
        let _stop = SetOnDrop(&done);
        world.apply("lock FAKEPHONE01").unwrap();
        feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "unlock it")
    });

    assert!(locked.0, "{}", locked.1);
}
