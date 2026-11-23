//! `factory lab` under the `devlab` name, beyond what the golden transcripts show: lease ids,
//! owners, the store's write lock while the fakes of docker and systemctl run, and the devices
//! the daemon tracks and boots.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::process::Signal;

use crate::support::world::{World, fixtures, poll_until};
use crate::support::{Run, Sandbox, finish, kill_group, run, spawn};

/// The sandbox with the golden world installed and the built binary on PATH as `devlab`.
fn lab() -> (Sandbox, World) {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_factory"), sandbox.path("bin/devlab"))
        .expect("link devlab");
    (sandbox, world)
}

fn devlab(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = sandbox.command(sandbox.path("bin/devlab"), args);
    cmd.current_dir(dir);
    cmd
}

/// `devlab` run with the test process as its owner.
fn owned(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Run {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    run(cmd)
}

fn lease_id(out: &Run) -> String {
    assert_eq!(out.code, 0, "{out:?}");
    out.stdout
        .lines()
        .find_map(|line| line.strip_prefix("export DEVLAB_LEASE="))
        .unwrap_or_else(|| panic!("no DEVLAB_LEASE in {out:?}"))
        .trim_matches('\'')
        .to_owned()
}

/// Stops the sandbox's daemon and waits until it is gone.
fn stop_daemon(sandbox: &Sandbox) {
    let pid: u32 = fs::read_to_string(sandbox.path("run/factory/daemon.pid"))
        .expect("a daemon runs")
        .trim()
        .parse()
        .expect("a pid");
    kill_group(pid, Signal::TERM);
    let gone = poll_until(|| {
        fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit(')')
                .next()
                .is_some_and(|s| s.trim_start().starts_with('Z'))
        })
    });
    assert!(gone, "daemon {pid} outlived its TERM");
}

fn write_machine_config(sandbox: &Sandbox, extra: &str) {
    let base = fs::read_to_string(fixtures().join("machine.json")).expect("read machine.json");
    let merged =
        base.trim_end().trim_end_matches('}').trim_end().to_owned() + ",\n  " + extra + "\n}\n";
    fs::write(sandbox.path("config/devlab/config.json"), merged).expect("write the machine config");
}

#[test]
fn lease_ids_climb_and_are_never_reused_even_across_a_daemon_restart() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-ids");
    let first = lease_id(&owned(&sandbox, &wt, &["acquire", "--no-device"]));
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);
    // The highest id was just freed: a plain max+1 would hand it out again.
    let second = lease_id(&owned(&sandbox, &wt, &["acquire", "--no-device"]));
    stop_daemon(&sandbox);
    // The lease outlives the daemon that granted it.
    let held = lease_id(&owned(&sandbox, &wt, &["env"]));
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);
    let third = lease_id(&owned(&sandbox, &wt, &["acquire", "--no-device"]));

    assert_eq!(
        (
            first.as_str(),
            second.as_str(),
            held.as_str(),
            third.as_str()
        ),
        ("1", "2", "2", "3")
    );
}

#[test]
fn the_owner_is_the_nearest_agent_above_the_shell() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-owner");
    let agent = sandbox.path("agent");
    fs::create_dir(&agent).expect("create the agent dir");
    for name in ["claude", "codex"] {
        std::os::unix::fs::symlink("/bin/sh", agent.join(name)).expect("link an agent");
    }
    // A claude runs a codex, which runs the shell devlab is called from: codex is the nearest.
    // Each trailing `true` keeps a shell from exec-ing into its last command, so it stays a parent.
    let inner =
        "echo $$; devlab acquire --no-device --quiet >/dev/null && devlab status --json; true";
    let outer = format!("{} -c '{inner}'; true", agent.join("codex").display());
    let mut cmd = sandbox.command(agent.join("claude"), &["-c", &outer]);
    cmd.current_dir(&wt);
    let child = spawn(cmd);
    let claude = child.id();
    let out = finish(child);

    let (codex, json) = out
        .stdout
        .split_once('\n')
        .unwrap_or_else(|| panic!("{out:?}"));
    let status: serde_json::Value =
        serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_ne!(codex, claude.to_string());
    assert_eq!(
        status["owners"][0]["owner"],
        format!("codex:{codex}"),
        "{out:?}"
    );
    assert!(
        !daemon_log(&sandbox).contains("warning"),
        "{}",
        daemon_log(&sandbox)
    );
}

#[test]
fn factory_lab_is_devlab_and_takes_the_global_json_flag() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-name");
    let as_devlab = owned(&sandbox, &wt, &["help"]);
    let as_factory = sandbox.factory_in(&wt, &["lab", "help"]);
    assert_eq!(
        (as_factory.code, &as_factory.stdout),
        (0, &as_devlab.stdout)
    );

    let status = sandbox.factory_in(&wt, &["--json", "lab", "status"]);
    assert_eq!(status.code, 0, "{status:?}");
    let status: serde_json::Value =
        serde_json::from_str(&status.stdout).unwrap_or_else(|e| panic!("{e}: {status:?}"));
    assert!(status["slots"].is_array(), "{status}");
}

#[test]
fn a_wait_that_is_no_number_of_seconds_is_refused() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-wait");
    let out = owned(&sandbox, &wt, &["acquire", "--no-device", "--wait", "soon"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            2,
            "",
            "devlab: --wait takes a number of seconds (0 = do not wait)\n"
        )
    );
}

#[test]
fn a_wait_that_is_no_number_of_seconds_stops_run_before_it_runs_anything() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-runwait");
    let marker = sandbox.path("ran");
    let marker_arg = marker.to_string_lossy().into_owned();
    let out = owned(
        &sandbox,
        &wt,
        &[
            "run",
            "--no-device",
            "--wait",
            "soon",
            "--",
            "touch",
            &marker_arg,
        ],
    );

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            2,
            "",
            "devlab: --wait takes a number of seconds (0 = do not wait)\n"
        )
    );
    assert!(!marker.exists(), "the command ran");
}

/// Target-repo device runners walk their ancestors for a command line like `devlab run ...` to
/// time their budget, so the command must be a child of the live `devlab`, not replace it.
#[test]
fn run_and_exec_keep_devlab_alive_as_the_parent_of_the_command() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-parent");
    let parent = r#"echo "$PPID $(tr '\0' ' ' < /proc/$PPID/cmdline)""#;
    let ran = owned(
        &sandbox,
        &wt,
        &["run", "--no-device", "--quiet", "--", "sh", "-c", parent],
    );
    let held = owned(&sandbox, &wt, &["exec", "--", "sh", "-c", parent]);

    for (call, out) in [("run", ran), ("exec", held)] {
        assert_eq!(out.code, 0, "{call}: {out:?}");
        let (pid, line) = out.stdout.split_once(' ').expect("a pid and a line");
        assert!(pid.parse::<u32>().is_ok(), "{call}: {out:?}");
        assert!(
            line.contains(&format!("bin/devlab {call} ")),
            "{call}: {out:?}"
        );
    }
}

#[test]
fn the_command_runs_with_default_signal_dispositions() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-signals");
    let piped = owned(
        &sandbox,
        &wt,
        &[
            "run",
            "--no-device",
            "--quiet",
            "--",
            "sh",
            "-c",
            "kill -PIPE $$",
        ],
    );

    assert_eq!(piped.code, 141, "{piped:?}");
}

#[test]
fn a_bad_wait_is_refused_even_when_the_machine_config_is_broken() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-brokencfg");
    fs::write(sandbox.path("config/devlab/config.json"), "{").expect("break the config");
    let out = owned(
        &sandbox,
        &wt,
        &["run", "--no-device", "--wait", "soon", "--", "true"],
    );

    assert_eq!(
        (out.code, out.stderr.as_str()),
        (
            2,
            "devlab: --wait takes a number of seconds (0 = do not wait)\n"
        )
    );
}

#[test]
fn a_wait_too_long_to_count_down_still_gets_a_lease() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-longwait");
    let out = owned(
        &sandbox,
        &wt,
        &[
            "run",
            "--no-device",
            "--quiet",
            "--wait",
            "1e19",
            "--",
            "sh",
            "-c",
            "exit 6",
        ],
    );

    assert_eq!(out.code, 6, "{out:?}");
}

fn daemon_log(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.path("run/factory/daemon.log")).unwrap_or_default()
}

#[test]
fn a_shell_with_no_agent_above_it_gets_a_lease_and_a_warning() {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""ownerNames": ["nobody-here"]"#);
    let wt = world.worktree("demo-orphan");
    let out = run(devlab(&sandbox, &wt, &["acquire", "--no-device"]));

    assert_eq!(lease_id(&out), "1");
    let log = daemon_log(&sandbox);
    assert!(
        log.contains("warning: the devlab call in")
            && log.contains("has no claude or codex ancestor"),
        "{log}"
    );
}

/// Fakes `tool` as the world does, after it records whether lab.db's write lock was free.
fn probe_lock(sandbox: &Sandbox, tool: &str, probes: &Path) {
    let db = sandbox.path("state/factory/lab.db");
    let body = fs::read_to_string(fixtures().join(format!("fakes/{tool}.sh")))
        .expect("read a fake")
        .replace("@WORLD@", &sandbox.path("world").display().to_string());
    let probe = r#"python3 - '@DB@' <<'PY' >> '@PROBES@'
import sqlite3, sys
try:
    c = sqlite3.connect(sys.argv[1], timeout=0, isolation_level=None)
    c.execute('BEGIN IMMEDIATE')
    c.execute('ROLLBACK')
    print('@TOOL@ free')
except sqlite3.OperationalError as e:
    print('@TOOL@ held:', e)
PY
"#
    .replace("@DB@", &db.display().to_string())
    .replace("@PROBES@", &probes.display().to_string())
    .replace("@TOOL@", tool);
    sandbox.fake(tool, &(probe + &body));
}

#[test]
fn docker_and_systemctl_run_while_the_store_is_unlocked() {
    let (sandbox, world) = lab();
    let probes: PathBuf = sandbox.path("probes");
    let wt = world.worktree("demo-lock");
    // The daemon makes lab.db on its first call; probe once it is there.
    assert_eq!(owned(&sandbox, &wt, &["status"]).code, 0);
    for tool in ["docker", "systemctl"] {
        probe_lock(&sandbox, tool, &probes);
    }
    let other = world.worktree("demo-other");
    // A second owner, so its lease takes the other slot instead of queueing behind the cap.
    let mut second = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("start a second owner");
    for (dir, owner, args) in [
        (
            &wt,
            std::process::id(),
            &["acquire", "--no-device", "--stack"][..],
        ),
        (&other, second.id(), &["acquire", "--no-device"][..]),
        (&wt, std::process::id(), &["status"][..]),
        (&wt, std::process::id(), &["stack", "status"][..]),
        (&wt, std::process::id(), &["release", "--stop-stack"][..]),
        (&wt, std::process::id(), &["gc"][..]),
    ] {
        let mut cmd = devlab(&sandbox, dir, args);
        cmd.env("DEVLAB_OWNER_PID", owner.to_string());
        let out = run(cmd);
        assert_eq!(out.code, 0, "{args:?}: {out:?}");
    }
    let _ = second.kill();
    let _ = second.wait();

    let seen = fs::read_to_string(&probes).unwrap_or_default();
    for tool in ["docker", "systemctl"] {
        assert!(
            seen.contains(&format!("{tool} free")),
            "{tool} never ran: {seen}"
        );
    }
    assert!(!seen.contains("held"), "{seen}");
}

/// Runs `devlab status` and returns its stdout.
fn status(sandbox: &Sandbox, dir: &Path) -> String {
    let out = owned(sandbox, dir, &["status"]);
    assert_eq!(out.code, 0, "{out:?}");
    out.stdout
}

/// Starts `devlab acquire --emulator` for the test's owner, and waits until status shows its
/// emulator booting.
fn boot_in_background(sandbox: &Sandbox, dir: &Path, ask: &str) -> std::process::Child {
    let mut cmd = devlab(sandbox, dir, &["acquire", ask]);
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    let child = spawn(cmd);
    // Booting shows from the grant; the emulator's unit starts a moment later.
    let unit_ports = sandbox.path("world/unit-ports");
    let booting = poll_until(|| {
        status(sandbox, dir).contains(" booting ")
            && fs::read_to_string(&unit_ports).is_ok_and(|u| u.contains("devlab-emu-0"))
    });
    assert!(
        booting,
        "the emulator never showed as booting: {}",
        status(sandbox, dir)
    );
    child
}

#[test]
fn a_phone_plugged_or_unplugged_is_known_from_one_stream_without_polling_adb() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    assert!(status(&sandbox, &wt).starts_with("devices: none attached\n"));

    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    let plugged = poll_until(|| {
        status(&sandbox, &wt).starts_with(
            "devices\n  FAKEPHONE01        phone    Pixel 8        ready        free\n",
        )
    });
    world.apply("unplug FAKEPHONE01").unwrap();
    let unplugged = poll_until(|| status(&sandbox, &wt).starts_with("devices: none attached\n"));

    assert!(plugged && unplugged, "{}", status(&sandbox, &wt));
    let calls = sandbox.calls("adb");
    let tracks = calls
        .iter()
        .filter(|c| *c == &["track-devices", "-l"])
        .count();
    assert_eq!(tracks, 1, "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|c| c.first().is_some_and(|a| a == "devices")),
        "adb devices was polled: {calls:?}"
    );
}

#[test]
fn an_emulator_whose_unit_ends_while_it_boots_is_never_ready_and_is_forgotten() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    world.apply("stall emulator-5580").unwrap();
    let child = boot_in_background(&sandbox, &wt, "--emulator");

    // The emulator's process dies: its unit ends and adb no longer lists it.
    let killed = run(sandbox.command("systemctl", &["--user", "stop", "devlab-emu-0"]));
    assert_eq!(killed.code, 0, "{killed:?}");
    let out = finish(child);

    assert_eq!(out.code, 77, "{out:?}");
    assert_eq!(
        out.stderr,
        "devlab: booting 1 emulator(s), about a minute (quick boot) or three (first boot)\n\
         devlab: devlab-emu-0 exited while booting; see: journalctl --user -u devlab-emu-0 -n 50\n"
    );
    // Status shows what adb lists, which drops the emulator once its stream says so.
    let gone = poll_until(|| status(&sandbox, &wt).starts_with("devices: none attached\n"));
    assert!(gone, "{}", status(&sandbox, &wt));
    // The lease stays, with no device in it.
    let env = owned(&sandbox, &wt, &["env"]);
    assert_eq!(
        (env.code, env.stdout.as_str()),
        (0, "export DEVLAB_LEASE=1\n")
    );
}

#[test]
fn an_emulator_a_dead_daemon_left_booting_is_stopped_and_forgotten_never_ready() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    world.apply("stall emulator-5580").unwrap();
    let child = boot_in_background(&sandbox, &wt, "--emulator");
    stop_daemon(&sandbox);
    assert_ne!(finish(child).code, 0);

    let settled = poll_until(|| status(&sandbox, &wt).starts_with("devices: none attached\n"));
    assert!(
        settled,
        "the emulator was never forgotten: {}",
        status(&sandbox, &wt)
    );
    assert!(
        sandbox.calls("adb").contains(&vec![
            "-s".to_owned(),
            "emulator-5580".to_owned(),
            "emu".to_owned(),
            "kill".to_owned()
        ]),
        "the emulator was not stopped: {:?}",
        sandbox.calls("adb")
    );
    // Never marked ready, it is not there to reuse: the next emulator asked for boots afresh.
    fs::write(sandbox.path("world/stalled"), "").unwrap();
    let again = owned(&sandbox, &wt, &["acquire", "--emulator", "--quiet"]);
    assert_eq!(again.code, 0, "{again:?}");
    assert_eq!(
        again.stderr,
        "devlab: booting 1 emulator(s), about a minute (quick boot) or three (first boot)\n"
    );
}

#[test]
fn a_pair_whose_emulator_fails_to_boot_gives_back_its_phone_too() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    world.apply("break systemd-run").unwrap();

    let out = owned(&sandbox, &wt, &["acquire", "--pair"]);

    assert_eq!(out.code, 77, "{out:?}");
    assert!(
        status(&sandbox, &wt).starts_with(
            "devices\n  FAKEPHONE01        phone    Pixel 8        ready        free\n"
        ),
        "{}",
        status(&sandbox, &wt)
    );
    let env = owned(&sandbox, &wt, &["env"]);
    assert_eq!(
        (env.code, env.stdout.as_str()),
        (0, "export DEVLAB_LEASE=1\n")
    );
}

#[test]
fn a_phone_unplugged_while_its_pair_boots_gives_the_acquire_exit_76() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    world.apply("stall emulator-5580").unwrap();
    let child = boot_in_background(&sandbox, &wt, "--pair");

    world.apply("unplug FAKEPHONE01").unwrap();
    let seen = poll_until(|| !status(&sandbox, &wt).contains("Pixel 8        ready"));
    assert!(seen, "the unplug never showed: {}", status(&sandbox, &wt));
    fs::write(sandbox.path("world/stalled"), "").unwrap();
    let out = finish(child);

    assert_eq!(out.code, 76, "{out:?}");
    assert!(
        out.stderr.ends_with(
            "devlab: phone FAKEPHONE01 (Pixel 8) is unplugged: run devlab acquire to replace it\n"
        ),
        "{out:?}"
    );
}

#[test]
fn a_failed_boot_takes_back_nothing_another_lease_was_granted_since() {
    let (sandbox, world) = lab();
    let (a, b) = (world.worktree("plain-a"), world.worktree("plain-b"));
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    world.apply("stall emulator-5580").unwrap();
    let child = boot_in_background(&sandbox, &a, "--pair");
    let released = owned(&sandbox, &a, &["release"]);
    assert_eq!(released.code, 0, "{released:?}");
    let taken = owned(&sandbox, &b, &["acquire", "--phone", "--quiet"]);
    assert_eq!(taken.code, 0, "{taken:?}");

    let killed = run(sandbox.command("systemctl", &["--user", "stop", "devlab-emu-0"]));
    assert_eq!(killed.code, 0, "{killed:?}");
    assert_eq!(finish(child).code, 77);

    let env = owned(&sandbox, &b, &["env"]);
    assert_eq!(env.code, 0, "{env:?}");
    assert!(env.stdout.contains("ANDROID_SERIAL=FAKEPHONE01"), "{env:?}");
    assert!(
        !status(&sandbox, &b).contains("Pixel 8        ready        free"),
        "{}",
        status(&sandbox, &b)
    );
}

/// The `lab-run` events in the ledger.
fn lab_runs(sandbox: &Sandbox) -> Vec<serde_json::Value> {
    sandbox
        .ledger_lines()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event.to_string().contains("\"lab-run\""))
        .collect()
}

/// Waits for the ledger's only `lab-run` event and returns it.
fn the_lab_run(sandbox: &Sandbox) -> serde_json::Value {
    poll_until(|| !lab_runs(sandbox).is_empty());
    let runs = lab_runs(sandbox);
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs.into_iter().next().expect("one run")
}

/// A `devlab run` started beside the test, owned by the test process.
fn spawn_run(sandbox: &Sandbox, dir: &Path, script: &str) -> std::process::Child {
    let mut cmd = devlab(
        sandbox,
        dir,
        &["run", "--no-device", "--quiet", "--", "sh", "-c", script],
    );
    cmd.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    spawn(cmd)
}

/// Sends `signal` to one process.
fn signal(pid: u32, signal: Signal) {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .expect("a pid");
    rustix::process::kill_process(pid, signal).expect("signal the process");
}

/// Waits until `devlab status` shows a waiter.
fn await_waiter(sandbox: &Sandbox, dir: &Path) {
    let queued = poll_until(|| {
        run(devlab(sandbox, dir, &["status"]))
            .stdout
            .contains("waiting:")
    });
    assert!(queued, "no run queued");
}

/// Holds the test owner's only demo slot, so its next run in demo queues.
fn hold_the_owners_slot(sandbox: &Sandbox, world: &World) {
    let holder = world.worktree("demo-holder");
    let held = owned(sandbox, &holder, &["acquire", "--no-device", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
}

#[test]
fn a_run_lands_in_the_ledger_with_its_command_slot_and_exit() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-ledger");
    let out = owned(
        &sandbox,
        &wt,
        &["run", "--no-device", "--quiet", "--", "sh", "-c", "exit 3"],
    );
    assert_eq!(out.code, 3, "{out:?}");

    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "exit 3", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
    assert_eq!(event["command"], "sh -c exit 3", "{event}");
    assert_eq!(event["worktree"], wt.display().to_string(), "{event}");
    assert_eq!(event["slot"], 0, "{event}");
    assert!(event["lease"].is_u64(), "{event}");
    assert!(event["queue_s"].is_u64(), "{event}");
    assert!(event["run_s"].is_u64(), "{event}");
}

#[test]
fn sigterm_while_queued_leaves_the_queue_and_the_ledger_says_so() {
    let (sandbox, world) = lab();
    hold_the_owners_slot(&sandbox, &world);
    let wt = world.worktree("demo-queued");
    let child = spawn_run(&sandbox, &wt, "true");
    await_waiter(&sandbox, &wt);

    signal(child.id(), Signal::TERM);
    let out = finish(child);

    assert_eq!(out.code, 143, "{out:?}");
    assert!(
        out.stderr.contains("devlab: stopped by SIGTERM after ")
            && out.stderr.contains(" s, while queued: "),
        "{out:?}"
    );
    let status = run(devlab(&sandbox, &wt, &["status"]));
    assert!(!status.stdout.contains("waiting:"), "{status:?}");
    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "stopped SIGTERM", "{event}");
    assert_eq!(event["phase"], "queued", "{event}");
    assert_eq!(event["run_s"], 0, "{event}");
}

#[test]
fn sigterm_while_the_command_runs_reaches_the_command_and_the_ledger() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-running");
    let got = wt.join("got");
    let script =
        r#"trap 'kill $!; echo TERM > got; exit 0' TERM; sleep 30 & echo $$ > child.pid; wait"#;
    let child = spawn_run(&sandbox, &wt, script);
    assert!(
        poll_until(|| wt.join("child.pid").exists()),
        "the command never started"
    );

    signal(child.id(), Signal::TERM);
    let out = finish(child);

    assert_eq!(out.code, 143, "{out:?}");
    assert!(
        out.stderr.contains("devlab: sh stopped by SIGTERM after "),
        "{out:?}"
    );
    assert_eq!(
        fs::read_to_string(&got).ok().as_deref(),
        Some("TERM\n"),
        "{out:?}"
    );
    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "stopped SIGTERM", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
}

#[test]
fn a_command_killed_by_sigkill_is_named_and_recorded_as_a_signal() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-killed");
    let child = spawn_run(&sandbox, &wt, "echo $$ > child.pid; exec sleep 30");
    let pid = wt.join("child.pid");
    assert!(
        poll_until(|| fs::read_to_string(&pid).is_ok_and(|p| p.ends_with('\n'))),
        "the command never started"
    );
    let command: u32 = fs::read_to_string(&pid)
        .expect("child.pid")
        .trim()
        .parse()
        .expect("a pid");

    signal(command, Signal::KILL);
    let out = finish(child);

    assert_eq!(out.code, 137, "{out:?}");
    assert!(
        out.stderr.contains("devlab: sh killed by SIGKILL after "),
        "{out:?}"
    );
    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "signal SIGKILL", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
}

#[test]
fn a_client_killed_while_its_command_runs_is_recorded_as_lost() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-lost");
    let child = spawn_run(&sandbox, &wt, "echo $$ > child.pid; sleep 30");
    assert!(
        poll_until(|| wt.join("child.pid").exists()),
        "the command never started"
    );
    let devlab_pid = child.id();

    signal(devlab_pid, Signal::KILL);
    let event = the_lab_run(&sandbox);
    kill_group(devlab_pid, Signal::KILL);
    let out = finish(child);

    assert_eq!(out.code, -1, "{out:?}");
    assert_eq!(event["outcome"], "lost", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
}

#[test]
fn a_client_killed_while_queued_is_recorded_as_lost_and_leaves_the_queue() {
    let (sandbox, world) = lab();
    hold_the_owners_slot(&sandbox, &world);
    let wt = world.worktree("demo-lost-queued");
    let child = spawn_run(&sandbox, &wt, "true");
    await_waiter(&sandbox, &wt);

    signal(child.id(), Signal::KILL);
    let _ = finish(child);

    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "lost", "{event}");
    assert_eq!(event["phase"], "queued", "{event}");
    let status = run(devlab(&sandbox, &wt, &["status"]));
    assert!(!status.stdout.contains("waiting:"), "{status:?}");
}

#[test]
fn a_daemon_stopping_under_a_run_records_it_as_lost_without_waiting_for_it() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-restart");
    let child = spawn_run(&sandbox, &wt, "echo $$ > child.pid; exec sleep 30");
    assert!(
        poll_until(|| wt.join("child.pid").exists()),
        "the command never started"
    );
    let devlab_pid = child.id();

    let stopping = std::time::Instant::now();
    stop_daemon(&sandbox);
    let took = stopping.elapsed();
    let event = the_lab_run(&sandbox);
    kill_group(devlab_pid, Signal::KILL);
    let _ = finish(child);

    assert!(
        took < std::time::Duration::from_secs(5),
        "the stop took {took:?}"
    );
    assert_eq!(event["outcome"], "lost", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
}
