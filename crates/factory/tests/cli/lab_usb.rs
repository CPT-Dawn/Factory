//! A phone with USB debugging off (#73): adb doesn't list it, so the daemon finds it on the USB
//! bus, read from a sysfs tree the test lays out under `FACTORY_SYSFS_ROOT`. It shows as "turn
//! on USB debugging" on the bar and in `devlab status`, and no grant takes it.

use std::path::Path;

use serde_json::Value;

use crate::lab::{devlab, lab, owned, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Sandbox, run};

const PIXEL: &str = "plug FAKEPHONE01 device usb:1-1 model:Pixel_8";

/// The golden world with phones and the USB bus looked at ten times a second, its daemon running.
fn watching() -> (Sandbox, World) {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""readinessCheckSec": 0.1"#);
    let wt = world.worktree("plain-usb");
    let status = run(devlab(&sandbox, &wt, &["status"]));
    assert_eq!(status.code, 0, "{status:?}");
    (sandbox, world)
}

/// A USB device on the sandbox's bus, as sysfs shows one.
fn plug_usb(sandbox: &Sandbox, name: &str, vendor: &str, serial: &str) {
    let dir = sandbox.path("sysfs/bus/usb/devices").join(name);
    std::fs::create_dir_all(&dir).expect("make the device");
    std::fs::write(dir.join("idVendor"), format!("{vendor}\n")).expect("write the vendor");
    std::fs::write(dir.join("serial"), format!("{serial}\n")).expect("write the serial");
}

fn unplug_usb(sandbox: &Sandbox, name: &str) {
    std::fs::remove_dir_all(sandbox.path("sysfs/bus/usb/devices").join(name))
        .expect("remove the device");
}

fn feed(sandbox: &Sandbox) -> Value {
    let out = sandbox.factory(&["bar", "--json"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

/// The Lab card's rows.
fn rows(feed: &Value) -> Vec<Value> {
    feed["sections"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["id"] == "lab"))
        .and_then(|lab| lab["rows"].as_array())
        .cloned()
        .unwrap_or_default()
}

fn row(feed: &Value, label: &str) -> Value {
    rows(feed)
        .into_iter()
        .find(|row| row["label"] == label)
        .unwrap_or(Value::Null)
}

fn feed_until(sandbox: &Sandbox, mut done: impl FnMut(&Value) -> bool) -> (bool, Value) {
    let mut last = Value::Null;
    let ok = poll_until(|| {
        last = feed(sandbox);
        done(&last)
    });
    (ok, last)
}

fn status_text(sandbox: &Sandbox, world: &World) -> String {
    let out = run(devlab(sandbox, &world.worktree("plain-usb"), &["status"]));
    assert_eq!(out.code, 0, "{out:?}");
    out.stdout
}

#[test]
fn a_known_vendor_device_adb_does_not_list_shows_on_the_bar_and_in_status() {
    let (sandbox, world) = watching();
    plug_usb(&sandbox, "1-2", "04e8", "SAMSUNG0001");

    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "Samsung")["text"] == "turn on USB debugging"
    });
    assert!(shown, "{last}");
    assert_eq!(row(&last, "Samsung")["tone"], "warn", "{last}");
    let chip = last["glance"]
        .as_array()
        .and_then(|g| g.iter().find(|c| c["id"] == "lab"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(chip["tone"], "warn", "{last}");
    assert!(
        chip["tooltip"]
            .as_str()
            .is_some_and(|t| t.contains("Samsung: turn on USB debugging")),
        "{last}"
    );

    let text = status_text(&sandbox, &world);
    let line = text
        .lines()
        .find(|l| l.contains("SAMSUNG0001"))
        .unwrap_or_else(|| panic!("no line for it in {text}"));
    assert!(
        line.contains("phone") && line.contains("turn on USB debugging"),
        "{text}"
    );

    // Turned on: adb lists it now while it stays on the bus, and it is a phone like any other:
    // never both the phone and its warning, not even in between.
    world
        .apply("plug SAMSUNG0001 device usb:1-2 model:Samsung")
        .unwrap();
    let mut doubled = Vec::new();
    let (cleared, last) = feed_until(&sandbox, |f| {
        let samsungs = rows(f)
            .into_iter()
            .filter(|r| {
                r["label"]
                    .as_str()
                    .is_some_and(|l| l.starts_with("Samsung"))
            })
            .count();
        if samsungs > 1 {
            doubled.push(f.clone());
        }
        row(f, "Samsung")["text"] == "ready"
    });
    assert!(cleared, "{last}");
    assert!(doubled.is_empty(), "{doubled:?}");
    assert!(!status_text(&sandbox, &world).contains("turn on USB debugging"));
}

#[test]
fn devices_that_are_no_hidden_phone_are_left_out() {
    let (sandbox, world) = watching();
    world.apply(PIXEL).unwrap();
    // The Pixel adb lists is on the bus too; a keyboard is no phone.
    plug_usb(&sandbox, "1-1", "18d1", "FAKEPHONE01");
    plug_usb(&sandbox, "1-3", "046d", "KEYBOARD01");
    plug_usb(&sandbox, "1-4", "2717", "XIAOMI0001");
    // Neither an interface, nor a device with no serial or a blank one, is a phone adb could list.
    std::fs::create_dir_all(sandbox.path("sysfs/bus/usb/devices/1-4:1.0")).unwrap();
    plug_usb(&sandbox, "1-6", "2d95", "");
    let bare = sandbox.path("sysfs/bus/usb/devices/1-7");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(bare.join("idVendor"), "22b8\n").unwrap();

    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "Xiaomi")["text"] == "turn on USB debugging"
    });
    assert!(shown, "{last}");
    let (ready, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(ready, "{last}");
    let labels: Vec<Value> = rows(&last).iter().map(|r| r["label"].clone()).collect();
    for absent in [
        "FAKEPHONE01",
        "KEYBOARD01",
        "Google",
        "Logitech",
        "vivo",
        "Motorola",
    ] {
        assert!(
            !labels.iter().any(|l| l == absent),
            "{absent} in {labels:?}"
        );
    }
    let text = status_text(&sandbox, &world);
    assert!(!text.contains("KEYBOARD01"), "{text}");
    assert_eq!(text.matches("turn on USB debugging").count(), 1, "{text}");
}

#[test]
fn a_phone_adb_lists_is_not_flagged_while_adb_s_stream_comes_back() {
    let (sandbox, world) = watching();
    world.apply(PIXEL).unwrap();
    plug_usb(&sandbox, "1-1", "18d1", "FAKEPHONE01");
    let (ready, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(ready, "{last}");

    // With no list to go by, no phone is said to be missing from it.
    world.restart_trackers(|| {});
    let before = sandbox.calls("adb").len();
    assert!(poll_until(|| sandbox.calls("adb").len() >= before + 10));
    let last = feed(&sandbox);
    assert_eq!(row(&last, "Pixel 8")["text"], "ready", "{last}");
    assert!(row(&last, "Google")["text"].is_null(), "{last}");
    assert!(!status_text(&sandbox, &world).contains("turn on USB debugging"));
}

#[test]
fn a_leased_phone_that_left_adb_but_not_the_bus_is_one_row_and_keeps_its_holder() {
    let (sandbox, world) = watching();
    world.apply(PIXEL).unwrap();
    let (ready, last) = feed_until(&sandbox, |f| row(f, "Pixel 8")["text"] == "ready");
    assert!(ready, "{last}");
    let wt = world.worktree("plain-held");
    let held = owned(&sandbox, &wt, &["acquire", "--phone"]);
    assert_eq!(held.code, 0, "{held:?}");

    // Its USB debugging goes off under the lease: adb loses it, the bus still has it.
    plug_usb(&sandbox, "1-1", "18d1", "FAKEPHONE01");
    world.apply("unplug FAKEPHONE01").unwrap();
    assert!(poll_until(
        || status_text(&sandbox, &world).contains("turn on USB debugging")
    ));
    let text = status_text(&sandbox, &world);
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("FAKEPHONE01")).collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(
        lines[0].contains("turn on USB debugging") && lines[0].contains("lease 1 plain-held"),
        "{text}"
    );
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "Google")["text"] == "leased to plain-held"
    });
    assert!(shown, "{last}");
}

#[test]
fn a_phone_said_to_have_its_debugging_off_is_forgotten_when_unplugged_while_adb_is_away() {
    let (sandbox, world) = watching();
    plug_usb(&sandbox, "1-2", "04e8", "SAMSUNG0001");
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "Samsung")["text"] == "turn on USB debugging"
    });
    assert!(shown, "{last}");

    world.restart_trackers(|| unplug_usb(&sandbox, "1-2"));
    let (gone, last) = feed_until(&sandbox, |f| row(f, "Samsung")["text"].is_null());
    assert!(gone, "{last}");
}

#[test]
fn no_grant_takes_a_phone_with_usb_debugging_off() {
    let (sandbox, world) = watching();
    plug_usb(&sandbox, "1-2", "04e8", "SAMSUNG0001");
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "Samsung")["text"] == "turn on USB debugging"
    });
    assert!(shown, "{last}");

    let refused = owned(
        &sandbox,
        &world.worktree("plain-usb"),
        &["acquire", "--phone", "--wait", "0"],
    );
    assert_eq!(refused.code, 75, "{refused:?}");
    assert!(!refused.stdout.contains("SAMSUNG0001"), "{refused:?}");
}

#[test]
fn the_sweep_runs_no_process() {
    let (sandbox, _world) = watching();
    plug_usb(&sandbox, "1-2", "04e8", "SAMSUNG0001");
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "Samsung")["text"] == "turn on USB debugging"
    });
    assert!(shown, "{last}");
    let before = sandbox.calls("adb").len();

    // Several sweeps later (it looks every tenth of a second), one more appears and goes.
    plug_usb(&sandbox, "1-5", "2d95", "VIVO0001");
    let (shown, last) = feed_until(&sandbox, |f| {
        row(f, "vivo")["text"] == "turn on USB debugging"
    });
    assert!(shown, "{last}");
    unplug_usb(&sandbox, "1-5");
    let (gone, last) = feed_until(&sandbox, |f| row(f, "vivo")["text"].is_null());
    assert!(gone, "{last}");

    assert_eq!(
        sandbox.calls("adb").len(),
        before,
        "{:?}",
        sandbox.calls("adb")
    );
    assert!(Path::new(&sandbox.path("sysfs")).exists());
}
