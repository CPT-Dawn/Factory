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
    let booting = poll_until(|| status(sandbox, dir).contains(" booting "));
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
    assert!(status(&sandbox, &wt).starts_with("devices: none attached\n"));
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
