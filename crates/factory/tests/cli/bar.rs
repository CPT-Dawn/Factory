//! `factory bar`: one daemon call, one line, or with `--json` Zenith's cockpit v1 document; a
//! valid document even with the daemon down.

use serde_json::Value;

use crate::claude::{HOUR, hook, now_secs, statusline_input};
use crate::lab::{Agent, for_owner, lab};
use crate::monitor;
use crate::router::configure;
use crate::support::herdr::FakeHerdr;
use crate::support::world::poll_until;
use crate::support::{Run, Sandbox, run};

fn bar(sandbox: &Sandbox, json: bool) -> Run {
    let args: &[&str] = if json { &["bar", "--json"] } else { &["bar"] };
    sandbox.factory(args)
}

fn feed(sandbox: &Sandbox) -> Value {
    let out = bar(sandbox, true);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    serde_json::from_str(&out.stdout).unwrap()
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

#[test]
fn the_bar_prints_both_claude_windows_and_the_leads_in_one_line() {
    let sandbox = Sandbox::new();
    let herdr = FakeHerdr::start(&sandbox);
    monitor::lead(&sandbox, &herdr, 1, "cart", "working");
    monitor::lead(&sandbox, &herdr, 2, "pay", "working");
    hook(
        &sandbox,
        &statusline_input(12.4, now_secs() + 2 * HOUR, 40.0),
    );
    let mut out = bar(&sandbox, false);
    assert!(
        poll_until(|| {
            out = bar(&sandbox, false);
            out.stdout == "claude 12/40 · codex ?/? · agy unreadable · 2 leads · lab 0/0\n"
        }),
        "{out:?}"
    );
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");

    // A lead that needs the owner is the chip's to say; the line still counts the leads.
    herdr.status("w2:p1", "blocked");
    assert!(
        poll_until(|| chip(&feed(&sandbox), "factory")["text"] == "1 need you"),
        "{:?}",
        bar(&sandbox, true)
    );
    assert_eq!(
        bar(&sandbox, false).stdout,
        "claude 12/40 · codex ?/? · agy unreadable · 2 leads · lab 0/0\n"
    );
}

#[test]
fn the_json_feed_is_one_cockpit_v1_document() {
    let sandbox = Sandbox::new();
    let resets = now_secs() + 2 * HOUR;
    hook(&sandbox, &statusline_input(80.0, resets, 40.0));

    let document = feed(&sandbox);

    assert_eq!(document["version"], 1, "{document}");
    assert!(
        document["generated"]
            .as_str()
            .is_some_and(|g| g.ends_with('Z')),
        "{document}"
    );
    let claude = chip(&document, "claude");
    assert_eq!(claude["text"], "80%", "{document}");
    assert_eq!(claude["tone"], "warn", "{document}");
    assert_eq!(claude["label"], "\u{f06a9}", "{document}");
    assert_eq!(
        claude["tooltip"], "Claude: 5h 80%, weekly 40%",
        "{document}"
    );
    let factory = chip(&document, "factory");
    assert_eq!(factory["label"], "\u{f020f}", "{document}");
    assert!(
        factory["tooltip"]
            .as_str()
            .is_some_and(|t| t.starts_with("Factory: ")),
        "{document}"
    );

    let card = section(&document, "claude");
    let rows = card["rows"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 2, "{card}");
    assert_eq!(rows[0]["kind"], "meter", "{card}");
    assert_eq!(rows[0]["label"], "5h", "{card}");
    assert_eq!(rows[0]["value"], 0.8, "{card}");
    let until = rows[0]["until"].as_str().unwrap_or_default();
    let status: Value = sandbox
        .factory(&["status", "--json"])
        .stdout
        .parse()
        .unwrap();
    assert_eq!(status["claude"]["resets"], resets * 1000, "{status}");
    assert!(
        until.ends_with(":00.000Z") || until.ends_with('Z'),
        "{card}"
    );
    assert_eq!(rows[1]["label"], "weekly", "{card}");

    let card = section(&document, "factory");
    let labels: Vec<&str> = card["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["label"].as_str())
        .collect();
    assert!(labels.contains(&"waits"), "{card}");
    assert!(labels.contains(&"steward"), "{card}");
}

#[test]
fn a_held_claude_chip_is_critical_and_its_card_says_why() {
    let sandbox = Sandbox::new();
    hook(
        &sandbox,
        &statusline_input(41.0, now_secs() + 2 * HOUR, 91.0),
    );

    let document = feed(&sandbox);

    assert_eq!(chip(&document, "claude")["tone"], "critical", "{document}");
    assert!(
        section(&document, "claude")["note"]
            .as_str()
            .is_some_and(|note| note.starts_with("hold: weekly 91% until ")),
        "{document}"
    );
    let line = bar(&sandbox, false).stdout;
    assert!(line.starts_with("claude 41/91 · "), "{line}");
}

#[test]
fn a_stale_window_sets_stale_since_on_its_card() {
    let sandbox = Sandbox::new();
    hook(&sandbox, &statusline_input(97.0, now_secs() - HOUR, 12.0));

    let document = feed(&sandbox);

    let card = section(&document, "claude");
    assert!(card["stale_since"].as_str().is_some(), "{card}");
    assert_eq!(card["rows"][0]["text"], "unknown, 0m old", "{card}");
    assert_eq!(card["rows"][0]["tone"], "muted", "{card}");
    assert_eq!(chip(&document, "claude")["text"], "12%", "{document}");
}

#[test]
fn herdr_unreachable_sets_error_on_the_factory_card() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.factory(&["monitor", "status"]).code, 0);

    let mut document = feed(&sandbox);
    assert!(
        poll_until(|| {
            document = feed(&sandbox);
            section(&document, "factory")["error"].is_string()
        }),
        "{document}"
    );
}

#[test]
fn with_the_daemon_down_the_json_feed_is_one_critical_chip_and_exits_0() {
    let sandbox = Sandbox::new();

    let out = bar(&sandbox, true);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    let document: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(document["version"], 1, "{document}");
    let glance = document["glance"].as_array().cloned().unwrap_or_default();
    assert_eq!(glance.len(), 1, "{document}");
    assert_eq!(glance[0]["text"], "factory daemon down", "{document}");
    assert_eq!(glance[0]["tone"], "critical", "{document}");
    assert!(
        !sandbox.path("run/factory/daemon.pid").exists(),
        "the bar started a daemon"
    );

    let plain = bar(&sandbox, false);
    assert_eq!((plain.code, plain.stdout.as_str()), (3, ""), "{plain:?}");
    assert!(
        plain.stderr.starts_with("factory: factory daemon down: "),
        "{plain:?}"
    );
}

#[test]
fn a_socket_with_no_daemon_behind_it_is_left_alone() {
    // systemd's socket with the service stopped: a call on it would start the daemon.
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.path("run/factory")).unwrap();
    let socket =
        std::os::unix::net::UnixListener::bind(sandbox.path("run/factory/factory.sock")).unwrap();
    socket.set_nonblocking(true).unwrap();

    let begun = std::time::Instant::now();
    let hooked = sandbox.factory_stdin(
        &["hook", "statusline"],
        &statusline_input(40.0, now_secs() + HOUR, 12.0),
    );
    assert_eq!(
        (hooked.code, hooked.stdout.as_str(), hooked.stderr.as_str()),
        (0, "", ""),
        "{hooked:?}"
    );
    assert!(begun.elapsed() < std::time::Duration::from_millis(500));
    let document = feed(&sandbox);
    assert_eq!(chip(&document, "factory")["text"], "factory daemon down");
    assert_eq!(bar(&sandbox, false).code, 3);

    assert!(
        matches!(socket.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "a call reached the socket"
    );
}

#[test]
fn a_live_socket_whose_backlog_is_full_keeps_the_hook_waiting_no_longer_than_its_patience() {
    // A daemon too busy to accept: the kernel queues no more calls, and a connect waits for room.
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.path("run/factory")).unwrap();
    let path = sandbox.path("run/factory/live.sock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let socket = runtime.block_on(async {
        let socket = tokio::net::UnixSocket::new_stream().unwrap();
        socket.bind(&path).unwrap();
        socket.listen(0).unwrap().into_std().unwrap()
    });
    let queued = std::os::unix::net::UnixStream::connect(&path).unwrap();

    let begun = std::time::Instant::now();
    let hooked = sandbox.factory_stdin(
        &["hook", "statusline"],
        &statusline_input(40.0, now_secs() + HOUR, 12.0),
    );
    let took = begun.elapsed();

    assert_eq!(
        (hooked.code, hooked.stdout.as_str(), hooked.stderr.as_str()),
        (0, "", ""),
        "{hooked:?}"
    );
    // The hook's patience is a second; the rest is the binary's start.
    assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
    drop((queued, socket));
}

/// A stand-in daemon on the live socket that takes the hook's call, then sends `reply` over and
/// over and never an answer, until the hook hangs up; true once it got the request.
fn chatter(sandbox: &Sandbox, reply: &'static [u8]) -> std::thread::JoinHandle<bool> {
    use std::io::{BufRead, BufReader, Write};
    // Past the bar's patience, so the bar gives up before the stand-in does.
    let enough = std::time::Duration::from_secs(20);
    std::fs::create_dir_all(sandbox.path("run/factory")).unwrap();
    let socket =
        std::os::unix::net::UnixListener::bind(sandbox.path("run/factory/live.sock")).unwrap();
    socket.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let mut accepted = None;
        if !poll_until(|| {
            accepted = socket.accept().ok();
            accepted.is_some()
        }) {
            return false;
        }
        let Some((stream, _)) = accepted else {
            return false;
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(enough)).unwrap();
        stream.set_write_timeout(Some(enough)).unwrap();
        let mut request = String::new();
        let read = BufReader::new(&stream).read_line(&mut request);
        let call: Option<Value> = serde_json::from_str(&request).ok();
        if read.is_err()
            || !request.ends_with('\n')
            || call.is_none_or(|call| call["method"].is_null())
        {
            return false;
        }
        let begun = std::time::Instant::now();
        while begun.elapsed() < enough && (&stream).write_all(reply).is_ok() {}
        true
    })
}

fn assert_the_hook_keeps_its_patience(sandbox: &Sandbox, daemon: std::thread::JoinHandle<bool>) {
    let begun = std::time::Instant::now();
    let hooked = sandbox.factory_stdin(
        &["hook", "statusline"],
        &statusline_input(40.0, now_secs() + HOUR, 12.0),
    );
    let took = begun.elapsed();

    assert_eq!(
        (hooked.code, hooked.stdout.as_str(), hooked.stderr.as_str()),
        (0, "", ""),
        "{hooked:?}"
    );
    // The hook's patience is a second; the rest is the binary's start.
    assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
    assert!(daemon.join().unwrap(), "the hook never sent its call");
}

#[test]
fn a_daemon_that_only_sends_notes_keeps_the_hook_no_longer_than_its_patience() {
    // Every line in time, the answer never: the patience is one deadline, not one per line.
    let sandbox = Sandbox::new();
    let daemon = chatter(&sandbox, b"{\"id\":1,\"note\":\"busy\"}\n");
    assert_the_hook_keeps_its_patience(&sandbox, daemon);
}

#[test]
fn a_daemon_that_never_answers_leaves_the_bar_a_next_step() {
    let sandbox = Sandbox::new();
    let daemon = chatter(&sandbox, b"{\"id\":1,\"note\":\"busy\"}\n");

    let plain = bar(&sandbox, false);

    assert_eq!((plain.code, plain.stdout.as_str()), (3, ""), "{plain:?}");
    assert!(
        plain.stderr.starts_with("factory: factory daemon down: ")
            && plain.stderr.contains("did not answer within 10s; see ")
            && plain.stderr.trim_end().ends_with(" and retry"),
        "{plain:?}"
    );
    assert!(daemon.join().unwrap(), "the bar never sent its call");
}

#[test]
fn a_reply_that_never_ends_its_line_keeps_the_hook_no_longer_than_its_patience() {
    // Bytes in time, never a whole line: no socket read waits out a timeout.
    let sandbox = Sandbox::new();
    let daemon = chatter(&sandbox, &[b' '; 64]);
    assert_the_hook_keeps_its_patience(&sandbox, daemon);
}

#[test]
fn a_bad_config_is_no_daemon_down_and_keeps_the_exit_table() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.factory(&["monitor", "status"]).code, 0);
    configure(&sandbox, "[claude]\nhold_at_percent = 0\n");

    let plain = bar(&sandbox, false);
    assert_eq!((plain.code, plain.stdout.as_str()), (3, ""), "{plain:?}");
    assert!(
        plain.stderr.contains("config.toml") && !plain.stderr.contains("daemon down"),
        "{plain:?}"
    );
    let out = bar(&sandbox, true);
    assert_eq!((out.code, out.stderr.as_str()), (3, ""), "{out:?}");
    let error: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(error["exit"], 3, "{error}");
    let message = error["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("config.toml") && !message.contains("daemon down"),
        "{error}"
    );
    assert!(error.get("glance").is_none(), "{error}");
}

#[test]
fn the_bar_runs_no_external_command_and_asks_herdr_nothing() {
    let (sandbox, world) = lab();
    let herdr = FakeHerdr::start(&sandbox);
    monitor::lead(&sandbox, &herdr, 1, "cart", "working");
    let a = Agent::start();
    let held = run(for_owner(
        &sandbox,
        &world.worktree("demo-a"),
        a.pid(),
        &["acquire", "--no-device"],
    ));
    assert_eq!(held.code, 0, "{held:?}");
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    // The daemon's first look at herdr is its own, at its start; its phone's probe and its
    // stacks' sweep are its own too, as the phone shows up and at its start.
    let mut last = Value::Null;
    assert!(
        poll_until(|| {
            last = feed(&sandbox);
            let slot = section(&last, "lab")["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| row["label"] == "demo slot 0" && row["text"] != "unchecked, demo-a");
            slot && bar(&sandbox, false)
                .stdout
                .ends_with("· 1 lead · lab 1/1\n")
        }),
        "{last} {:?}",
        bar(&sandbox, false)
    );
    let tools = [
        "adb",
        "docker",
        "systemctl",
        "gh",
        "herdr",
        "codex",
        "claude",
        "agy",
    ];
    let calls = |sandbox: &Sandbox| -> Vec<usize> {
        tools.iter().map(|tool| sandbox.calls(tool).len()).collect()
    };
    let (before, herdr_before) = (calls(&sandbox), herdr.methods());

    for _ in 0..3 {
        assert_eq!(bar(&sandbox, false).code, 0);
        feed(&sandbox);
    }

    assert_eq!(calls(&sandbox), before);
    assert_eq!(herdr.methods(), herdr_before);
}

#[test]
fn the_line_keeps_the_output_contract() {
    let sandbox = Sandbox::new();
    hook(
        &sandbox,
        &statusline_input(12.0, now_secs() + 2 * HOUR, 40.0),
    );

    let out = bar(&sandbox, false);

    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    assert!(out.stdout.chars().count() <= 200, "{out:?}");
}
