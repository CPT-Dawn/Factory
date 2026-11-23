//! `factory lab` under the `devlab` name, beyond what the golden transcripts show: lease ids,
//! owners, and the store's write lock while the fakes of docker and systemctl run.

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
    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);
    stop_daemon(&sandbox);
    let third = lease_id(&owned(&sandbox, &wt, &["acquire", "--no-device"]));

    assert_eq!(
        (first.as_str(), second.as_str(), third.as_str()),
        ("1", "2", "3")
    );
}

#[test]
fn the_owner_is_the_nearest_claude_above_the_shell() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-owner");
    let agent = sandbox.path("agent");
    fs::create_dir(&agent).expect("create the agent dir");
    std::os::unix::fs::symlink("/bin/sh", agent.join("claude")).expect("link a claude");
    // The trailing `true` keeps the shell from exec-ing into devlab, so it stays the parent.
    let mut cmd = sandbox.command(
        agent.join("claude"),
        &[
            "-c",
            "devlab acquire --no-device --quiet >/dev/null && devlab status --json; true",
        ],
    );
    cmd.current_dir(&wt);
    let child = spawn(cmd);
    let pid = child.id();
    let out = finish(child);

    let status: serde_json::Value =
        serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(
        status["owners"][0]["owner"],
        format!("claude:{pid}"),
        "{out:?}"
    );
    assert!(
        !daemon_log(&sandbox).contains("warning"),
        "{}",
        daemon_log(&sandbox)
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
