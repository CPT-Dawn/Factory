//! The daemon's one `adb track-devices` stream (#42): restarted, with backoff and one log line
//! each, when it ends or stops answering, and its adb child dies with the daemon however the
//! daemon dies. The adb here is a script that plays a different part on each run.

use std::fs;
use std::path::{Path, PathBuf};

use rustix::process::Signal;

use super::lab::devlab;
use crate::support::world::{World, poll_until};
use crate::support::{Sandbox, kill_group, run};

/// The sandbox's adb: each `track-devices` run is numbered, writes its pid to `adb/pid.<run>`, and
/// then plays the part `script` gives it for that run (shell, with `$n` the run and `frame
/// <lines>` writing one device list, which `adb/devices` then holds too). `adb devices -l`, which
/// the daemon probes with, lists what `adb/devices` holds; it hangs while `adb/probe-hangs`
/// exists (its pid is then in `adb/probe.pid`, and that of a child it left running in
/// `adb/probe-child.pid`), and notes each call's pid in `adb/probes`. While `adb/chatty` exists,
/// it says far more than a pipe holds before its list.
fn adb_playing(sandbox: &Sandbox, script: &str) -> PathBuf {
    let dir = sandbox.path("adb");
    fs::create_dir_all(&dir).expect("create the adb dir");
    let body = format!(
        r#"w='{dir}'
frame() {{ printf '%s\n' "$1" > "$w/devices"; printf '%04x' "$(printf '%s\n' "$1" | wc -c)"; printf '%s\n' "$1"; }}
case "$1" in
devices)
  echo "$$" >> "$w/probes"
  if [ -e "$w/probe-hangs" ]; then
    sleep 1000 &
    echo "$!" > "$w/probe-child.pid"
    echo "$$" > "$w/probe.pid"
    exec sleep 1000
  fi
  [ -e "$w/chatty" ] && yes '* adb says a lot' | head -c 200000
  echo 'List of devices attached'
  cat "$w/devices" 2>/dev/null
  exit 0 ;;
track-devices)
  n=$(( $(cat "$w/runs" 2>/dev/null || echo 0) + 1 ))
  echo "$n" > "$w/runs"
  echo "$$" > "$w/pid.$n"
  {script}
  ;;
esac
exit 0"#,
        dir = dir.display()
    );
    sandbox.fake("adb", &body);
    dir
}

const PHONE_A: &str = "PHONEA01 device usb:1-1 model:Pixel_8";
const PHONE_B: &str = "PHONEB02 device usb:1-2 model:Pixel_9";

fn setup(sandbox: Sandbox) -> (Sandbox, PathBuf) {
    let world = World::install(&sandbox);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_factory"), sandbox.path("bin/devlab"))
        .expect("link devlab");
    let wt = world.worktree("plain-a");
    (sandbox, wt)
}

fn status(sandbox: &Sandbox, wt: &Path) -> String {
    let mut cmd = devlab(sandbox, wt, &["status"]);
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    run(cmd).stdout
}

fn daemon_log(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.path("run/factory/daemon.log")).unwrap_or_default()
}

/// Whether process `pid` runs and is not a zombie.
fn runs(pid: &str) -> bool {
    // An empty or garbled pid would name /proc/stat itself.
    pid.trim().parse::<u32>().is_ok()
        && fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|stat| {
            !stat
                .rsplit(')')
                .next()
                .is_some_and(|s| s.trim_start().starts_with('Z'))
        })
}

/// The parent of process `pid`; empty when it is gone.
fn parent_of(pid: &str) -> String {
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
    stat.rsplit(')')
        .next()
        .and_then(|rest| rest.split_whitespace().nth(1))
        .unwrap_or("")
        .to_owned()
}

fn pid_of(dir: &Path, run: u32) -> String {
    fs::read_to_string(dir.join(format!("pid.{run}"))).unwrap_or_default()
}

fn shows(sandbox: &Sandbox, wt: &Path, serial: &str) -> bool {
    status(sandbox, wt).contains(serial)
}

#[test]
fn a_stream_that_ends_is_restarted_and_the_devices_seen_after_it_are_tracked() {
    let sandbox = Sandbox::new().env("FACTORY_ADB_BACKOFF_MS", "50");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(
            r#"if [ "$n" = 1 ]; then frame '{PHONE_A}'; exit 0; fi
  frame '{PHONE_A}
{PHONE_B}'; exec sleep 1000"#
        ),
    );

    assert!(
        poll_until(|| shows(&sandbox, &wt, "PHONEA01")),
        "no first list"
    );
    let tracked = poll_until(|| shows(&sandbox, &wt, "PHONEB02"));

    assert!(
        tracked,
        "{}\n{}",
        status(&sandbox, &wt),
        daemon_log(&sandbox)
    );
    assert!(
        pid_of(&adb, 2).trim().parse::<u32>().is_ok(),
        "run 2 never started"
    );
    assert!(!runs(&pid_of(&adb, 1)), "the first stream still runs");
}

#[test]
fn a_stream_that_never_shows_a_list_is_cut_off_and_restarted_within_a_bound() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_LIST_MS", "300");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(r#"[ "$n" = 1 ] && exec sleep 1000; frame '{PHONE_A}'; exec sleep 1000"#),
    );

    let tracked = poll_until(|| shows(&sandbox, &wt, "PHONEA01"));

    assert!(
        tracked,
        "{}\n{}",
        status(&sandbox, &wt),
        daemon_log(&sandbox)
    );
    assert!(!runs(&pid_of(&adb, 1)), "the hung stream still runs");
    assert!(
        daemon_log(&sandbox).contains("adb track-devices"),
        "{}",
        daemon_log(&sandbox)
    );
}

#[test]
fn a_stream_whose_adb_stops_answering_after_its_list_is_cut_off_and_restarted() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_PROBE_MS", "1000");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(
            r#"if [ "$n" = 1 ]; then touch "$w/probe-hangs"; frame '{PHONE_A}'; exec sleep 1000; fi
  rm -f "$w/probe-hangs"
  frame '{PHONE_A}
{PHONE_B}'; exec sleep 1000"#
        ),
    );

    assert!(
        poll_until(|| shows(&sandbox, &wt, "PHONEA01")),
        "no first list"
    );
    let tracked = poll_until(|| shows(&sandbox, &wt, "PHONEB02"));

    assert!(
        tracked,
        "{}\n{}",
        status(&sandbox, &wt),
        daemon_log(&sandbox)
    );
    assert!(!runs(&pid_of(&adb, 1)), "the hung stream still runs");
    let question = fs::read_to_string(adb.join("probe.pid")).unwrap_or_default();
    let its_child = fs::read_to_string(adb.join("probe-child.pid")).unwrap_or_default();
    assert!(
        poll_until(|| !runs(&question) && !runs(&its_child)),
        "the question {question:?} that went unanswered, or its child {its_child:?}, still runs"
    );
}

#[test]
fn a_stream_that_is_silent_but_adb_answers_is_left_alone() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_PROBE_MS", "500");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(&sandbox, &format!(r#"frame '{PHONE_A}'; exec sleep 1000"#));

    assert!(
        poll_until(|| shows(&sandbox, &wt, "PHONEA01")),
        "no first list"
    );
    // Three questions, each answered, and the stream is still the first.
    let asked =
        poll_until(|| fs::read_to_string(adb.join("probes")).is_ok_and(|p| p.lines().count() >= 3));
    assert!(asked, "adb was never asked whether it answers");

    assert!(runs(&pid_of(&adb, 1)), "a quiet stream was restarted");
    assert!(!adb.join("pid.2").exists(), "{}", daemon_log(&sandbox));
}

#[test]
fn restarts_back_off_and_log_one_line_each() {
    let sandbox = Sandbox::new().env("FACTORY_ADB_BACKOFF_MS", "100");
    let (sandbox, wt) = setup(sandbox);
    adb_playing(&sandbox, "exit 1");

    status(&sandbox, &wt);
    let logged = poll_until(|| restart_lines(&sandbox).len() >= 4);

    let lines = restart_lines(&sandbox);
    assert!(logged, "{lines:?}");
    let delays: Vec<&str> = lines
        .iter()
        .take(4)
        .map(|l| {
            l.split("restarting in ")
                .nth(1)
                .unwrap_or("")
                .split(' ')
                .next()
                .unwrap_or("")
        })
        .collect();
    assert_eq!(delays, ["100ms", "200ms", "400ms", "800ms"], "{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with("factory: ")),
        "{lines:?}"
    );
}

fn restart_lines(sandbox: &Sandbox) -> Vec<String> {
    daemon_log(sandbox)
        .lines()
        .filter(|l| l.contains("adb track-devices") && l.contains("restarting in"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn after_the_daemon_is_killed_no_adb_it_started_is_left_running() {
    let (sandbox, wt) = setup(Sandbox::new());
    let adb = adb_playing(&sandbox, &format!(r#"frame '{PHONE_A}'; exec sleep 1000"#));
    assert!(
        poll_until(|| shows(&sandbox, &wt, "PHONEA01")),
        "no first list"
    );
    let adb_pid = pid_of(&adb, 1);
    assert!(runs(&adb_pid), "adb never ran: {adb_pid:?}");

    let daemon: u32 = fs::read_to_string(sandbox.path("run/factory/daemon.pid"))
        .expect("a daemon runs")
        .trim()
        .parse()
        .expect("a pid");
    let wrapper = parent_of(&adb_pid);
    // Only the daemon: the adb it started leads a process group of its own.
    kill_group(daemon, Signal::KILL);

    let gone = poll_until(|| !runs(&adb_pid) && !runs(&wrapper));
    assert!(
        gone,
        "adb {adb_pid} or its wrapper {wrapper} outlived the daemon"
    );
}

#[test]
fn after_the_daemon_is_killed_mid_question_the_question_to_adb_does_not_linger() {
    let sandbox = Sandbox::new().env("FACTORY_ADB_PROBE_MS", "3000");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(r#"touch "$w/probe-hangs"; frame '{PHONE_A}'; exec sleep 1000"#),
    );
    assert!(
        poll_until(|| shows(&sandbox, &wt, "PHONEA01")),
        "no first list"
    );
    let asking =
        poll_until(|| runs(&fs::read_to_string(adb.join("probe.pid")).unwrap_or_default()));
    assert!(asking, "adb was never asked whether it answers");
    let question = fs::read_to_string(adb.join("probe.pid")).unwrap_or_default();
    let its_child = fs::read_to_string(adb.join("probe-child.pid")).unwrap_or_default();

    let daemon: u32 = fs::read_to_string(sandbox.path("run/factory/daemon.pid"))
        .expect("a daemon runs")
        .trim()
        .parse()
        .expect("a pid");
    kill_group(daemon, Signal::KILL);

    let gone = poll_until(|| !runs(&question) && !runs(&its_child) && !runs(&pid_of(&adb, 1)));
    assert!(
        gone,
        "the question {question:?} ({}), its child {its_child:?} or the stream {:?} ({}) outlived the daemon",
        runs(&question),
        pid_of(&adb, 1),
        runs(&pid_of(&adb, 1))
    );
}

#[test]
fn a_first_list_that_stops_halfway_is_as_hung_as_none() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_LIST_MS", "300");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(
            r#"[ "$n" = 1 ] && {{ printf '00'; exec sleep 1000; }}; frame '{PHONE_A}'; exec sleep 1000"#
        ),
    );

    let tracked = poll_until(|| shows(&sandbox, &wt, "PHONEA01"));

    assert!(
        tracked,
        "{}\n{}",
        status(&sandbox, &wt),
        daemon_log(&sandbox)
    );
    assert!(
        !runs(&pid_of(&adb, 1)),
        "the stream that stopped halfway still runs"
    );
}

#[test]
fn a_later_list_that_stops_halfway_is_cut_off_though_adb_still_answers() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_LIST_MS", "300")
        .env("FACTORY_ADB_PROBE_MS", "100");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(
            r#"if [ "$n" = 1 ]; then frame '{PHONE_A}'; printf '00'; exec sleep 1000; fi
  frame '{PHONE_A}
{PHONE_B}'; exec sleep 1000"#
        ),
    );

    let tracked = poll_until(|| shows(&sandbox, &wt, "PHONEB02"));

    assert!(
        tracked,
        "{}\n{}",
        status(&sandbox, &wt),
        daemon_log(&sandbox)
    );
    assert!(
        !runs(&pid_of(&adb, 1)),
        "the stream that stopped halfway still runs"
    );
}

#[test]
fn a_stream_that_misses_a_change_adb_knows_of_is_cut_off_and_restarted() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_LIST_MS", "300")
        .env("FACTORY_ADB_PROBE_MS", "200");
    let (sandbox, wt) = setup(sandbox);
    // Run 1 shows phone A, then adb sees phone B come, and the stream never says so.
    let adb = adb_playing(
        &sandbox,
        &format!(
            r#"if [ "$n" = 1 ]; then
    frame '{PHONE_A}'
    printf '%s\n%s\n' '{PHONE_A}' '{PHONE_B}' > "$w/devices"
    exec sleep 1000
  fi
  frame '{PHONE_A}
{PHONE_B}'; exec sleep 1000"#
        ),
    );

    let tracked = poll_until(|| shows(&sandbox, &wt, "PHONEB02"));

    assert!(
        tracked,
        "{}\n{}",
        status(&sandbox, &wt),
        daemon_log(&sandbox)
    );
    assert!(!runs(&pid_of(&adb, 1)), "the stale stream still runs");
    assert!(
        daemon_log(&sandbox).contains("no list with the devices adb lists"),
        "{}",
        daemon_log(&sandbox)
    );
}

#[test]
fn a_quiet_stream_whose_adb_answers_at_length_is_left_alone() {
    let sandbox = Sandbox::new()
        .env("FACTORY_ADB_BACKOFF_MS", "50")
        .env("FACTORY_ADB_PROBE_MS", "500");
    let (sandbox, wt) = setup(sandbox);
    let adb = adb_playing(
        &sandbox,
        &format!(r#"touch "$w/chatty"; frame '{PHONE_A}'; exec sleep 1000"#),
    );

    assert!(
        poll_until(|| shows(&sandbox, &wt, "PHONEA01")),
        "no first list"
    );
    let asked =
        poll_until(|| fs::read_to_string(adb.join("probes")).is_ok_and(|p| p.lines().count() >= 3));
    assert!(asked, "adb was never asked for its devices");

    assert!(runs(&pid_of(&adb, 1)), "{}", daemon_log(&sandbox));
    assert!(!adb.join("pid.2").exists(), "{}", daemon_log(&sandbox));
}
