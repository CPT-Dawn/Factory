//! `factory lab` beyond what the golden transcripts show: lease ids, owners, the store's write
//! lock while the fakes of docker and systemctl run, the queue across daemon restarts, stacks
//! being stopped, and the output contract under the `factory lab` name.

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
    // A zombie leader can outlast its other threads, which still hold the socket and the lock:
    // the daemon is gone once the leader is all that is left of it.
    let gone = poll_until(|| {
        let threads = fs::read_dir(format!("/proc/{pid}/task")).map_or(0, Iterator::count);
        fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            threads <= 1
                && stat
                    .rsplit(')')
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
fn factory_lab_shows_devlabs_help_and_takes_the_global_json_flag() {
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

/// `devlab` run in `dir` for the owner process `pid`.
fn for_owner(sandbox: &Sandbox, dir: &Path, pid: u32, args: &[&str]) -> Command {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", pid.to_string());
    cmd
}

/// A stand-in agent: a live process to own leases, killed when dropped.
struct Agent(std::process::Child);

impl Agent {
    fn start() -> Agent {
        Agent(
            Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("start an owner"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn exit(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.exit();
    }
}

/// Fakes the project's supabase CLI as the world does, except that `stop` first marks
/// `began`, then holds until `gate` exists (for at most the test deadline).
fn hold_stops(sandbox: &Sandbox, began: &Path, gate: &Path) {
    let body = fs::read_to_string(fixtures().join("fakes/supabase.sh"))
        .expect("read a fake")
        .replace("@WORLD@", &sandbox.path("world").display().to_string());
    let hold = format!(
        "if [ \"$1\" = stop ]; then\n  : > '{began}'\n  i=0\n  while [ ! -e '{gate}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\nfi\n",
        began = began.display(),
        gate = gate.display()
    );
    sandbox.fake("supabase", &(hold + &body));
}

/// Owner a holds demo's slot 0 with its stack up and b holds slot 1. While a's
/// `release --stop-stack` is still stopping the stack, `free` drops a's lease some other way:
/// slot 0 stays held until the stack is down, then goes to the next owner.
fn a_slot_stays_held_while_its_stack_stops(free: impl FnOnce(&Sandbox, &Path, &mut Agent)) {
    let (sandbox, world) = lab();
    let (mut a, b, c) = (Agent::start(), Agent::start(), Agent::start());
    let (wa, wb, wc) = (
        world.worktree("demo-a"),
        world.worktree("demo-b"),
        world.worktree("demo-c"),
    );
    let up = run(for_owner(
        &sandbox,
        &wa,
        a.pid(),
        &["acquire", "--no-device", "--stack"],
    ));
    assert!(up.stdout.contains("DEVLAB_SLOT=0"), "{up:?}");
    let other = run(for_owner(
        &sandbox,
        &wb,
        b.pid(),
        &["acquire", "--no-device"],
    ));
    assert!(other.stdout.contains("DEVLAB_SLOT=1"), "{other:?}");
    let (began, gate) = (sandbox.path("stop-began"), sandbox.path("stop-gate"));
    hold_stops(&sandbox, &began, &gate);
    let stopping = spawn(for_owner(
        &sandbox,
        &wa,
        a.pid(),
        &["release", "--stop-stack"],
    ));
    assert!(poll_until(|| began.exists()), "supabase stop never ran");

    free(&sandbox, &wa, &mut a);
    let mid_stop = run(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "0"],
    ));
    fs::write(&gate, "").expect("open the gate");
    let stopped = finish(stopping);
    let after = run(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "0"],
    ));

    assert_eq!(
        (mid_stop.code, mid_stop.stderr.as_str()),
        (
            75,
            "devlab: gave up after 0 s: all 2 stack slots of demo are held\n"
        ),
        "{mid_stop:?}"
    );
    assert_eq!(stopped.code, 0, "{stopped:?}");
    assert!(after.stdout.contains("DEVLAB_SLOT=0"), "{after:?}");
}

#[test]
fn a_plain_release_mid_stop_leaves_the_slot_held_until_the_stack_is_down() {
    a_slot_stays_held_while_its_stack_stops(|sandbox, wa, a| {
        let plain = run(for_owner(sandbox, wa, a.pid(), &["release"]));
        assert_eq!(plain.stderr, "devlab: released lease 1\n", "{plain:?}");
    });
}

#[test]
fn an_owner_that_exits_mid_stop_leaves_the_slot_held_until_the_stack_is_down() {
    a_slot_stays_held_while_its_stack_stops(|sandbox, wa, a| {
        a.exit();
        let gc = run(devlab(sandbox, wa, &["gc"]));
        assert!(gc.stdout.starts_with("released lease 1 "), "{gc:?}");
    });
}

/// The worktrees `devlab status` shows waiting, oldest first.
fn waiting(sandbox: &Sandbox, dir: &Path) -> Vec<String> {
    let out = run(devlab(sandbox, dir, &["status", "--json"]));
    let report: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_default();
    report["waiting"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| {
            let after = line.as_str()?.split(" @ ").nth(1)?;
            after.split(" for ").next().map(str::to_owned)
        })
        .collect()
}

#[test]
fn queued_requests_keep_their_places_across_a_daemon_restart() {
    let (sandbox, world) = lab();
    let (a, b, c, d) = (
        Agent::start(),
        Agent::start(),
        Agent::start(),
        Agent::start(),
    );
    let [wa, wb, wc, wd] = ["demo-a", "demo-b", "demo-c", "demo-d"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let queue = ["acquire", "--no-device", "--wait", "30"];
    let first = spawn(for_owner(&sandbox, &wc, c.pid(), &queue));
    assert!(poll_until(|| waiting(&sandbox, &wa) == ["demo-c"]));
    let second = spawn(for_owner(&sandbox, &wd, d.pid(), &queue));
    assert!(poll_until(|| waiting(&sandbox, &wa) == ["demo-c", "demo-d"]));

    // The first in line can't speak up while the daemon restarts.
    kill_group(first.id(), Signal::STOP);
    stop_daemon(&sandbox);
    let freed = run(for_owner(&sandbox, &wa, a.pid(), &["release"]));
    let after_restart = waiting(&sandbox, &wa);
    kill_group(first.id(), Signal::CONT);
    let first = finish(first);
    let freed_too = run(for_owner(&sandbox, &wb, b.pid(), &["release"]));
    let second = finish(second);

    assert_eq!(
        (freed.code, freed_too.code),
        (0, 0),
        "{freed:?} {freed_too:?}"
    );
    assert_eq!(after_restart, ["demo-c", "demo-d"]);
    assert!(first.stdout.contains("DEVLAB_SLOT=0"), "{first:?}");
    assert!(second.stdout.contains("DEVLAB_SLOT=1"), "{second:?}");
}

#[test]
fn a_queued_request_whose_caller_died_while_the_daemon_was_down_is_pruned() {
    let (sandbox, world) = lab();
    let (a, b, c) = (Agent::start(), Agent::start(), Agent::start());
    let [wa, wb, wc] = ["demo-a", "demo-b", "demo-c"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let queued = spawn(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "30"],
    ));
    assert!(poll_until(|| waiting(&sandbox, &wa) == ["demo-c"]));
    kill_group(queued.id(), Signal::STOP);
    stop_daemon(&sandbox);
    kill_group(queued.id(), Signal::KILL);
    let _ = finish(queued);

    assert_eq!(waiting(&sandbox, &wa), Vec::<String>::new());
}

#[test]
fn a_lease_granted_after_its_caller_left_is_given_back() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-gone");
    let (began, gate) = (sandbox.path("inspect-began"), sandbox.path("inspect-gate"));
    let body = fs::read_to_string(fixtures().join("fakes/docker.sh"))
        .expect("read a fake")
        .replace("@WORLD@", &sandbox.path("world").display().to_string());
    let hold = format!(
        "if [ \"$1\" = inspect ]; then\n  : > '{began}'\n  i=0\n  while [ ! -e '{gate}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\nfi\n",
        began = began.display(),
        gate = gate.display()
    );
    sandbox.fake("docker", &(hold + &body));
    let agent = Agent::start();
    let asking = spawn(for_owner(
        &sandbox,
        &wt,
        agent.pid(),
        &["acquire", "--no-device"],
    ));
    // The daemon looks at the stacks before it grants: the caller leaves meanwhile.
    assert!(poll_until(|| began.exists()), "docker inspect never ran");
    kill_group(asking.id(), Signal::KILL);
    let _ = finish(asking);
    fs::write(&gate, "").expect("open the gate");

    let given_back = poll_until(|| {
        let out = run(devlab(&sandbox, &wt, &["status", "--json"]));
        let report: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_default();
        let slots = report["slots"].as_array().cloned().unwrap_or_default();
        !slots.is_empty() && slots.iter().all(|s| s["holder"] == "free")
    });
    assert!(given_back, "the lease outlived its caller");
    assert_eq!(waiting(&sandbox, &wt), Vec::<String>::new());
}

/// `factory <args>` in `dir` for the owner process `pid`: the lab under its own name.
fn native(sandbox: &Sandbox, dir: &Path, pid: u32, args: &[&str]) -> Run {
    let mut cmd = sandbox.factory_command(args);
    cmd.current_dir(dir)
        .env("DEVLAB_OWNER_PID", pid.to_string());
    run(cmd)
}

/// One line on stdout and nothing on stderr: the line.
fn one_line(out: &Run) -> &str {
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout.lines().count(), 1, "{out:?}");
    assert!(out.stdout.chars().count() <= 201, "{out:?}");
    out.stdout.trim_end()
}

/// One JSON object on stdout and nothing on stderr.
fn one_json(out: &Run) -> serde_json::Value {
    assert_eq!(out.stderr, "", "{out:?}");
    assert_eq!(out.stdout.lines().count(), 1, "{out:?}");
    serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

/// The file a line names last, as `… in <file>`.
fn named_file(line: &str) -> String {
    let path = line.rsplit(" in ").next().unwrap_or_default();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{e}: {line}"))
}

#[test]
fn factory_lab_says_one_line_per_verb_and_json_with_the_flag() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-native");
    let me = std::process::id();
    let lab = |args: &[&str]| native(&sandbox, &wt, me, &[&["lab"], args].concat());

    let acquired = lab(&["acquire", "--no-device"]);
    let line = one_line(&acquired);
    assert!(
        line.starts_with("lease 1, slot 0 (stack http://127.0.0.1:27321, down); env in "),
        "{line}"
    );
    assert!(
        named_file(line).contains("export DEVLAB_LEASE=1\n"),
        "{line}"
    );
    let env = one_line(&lab(&["env"])).to_owned();
    assert!(
        named_file(&env).contains("export API_PORT=27787\n"),
        "{env}"
    );
    let env = one_json(&lab(&["--json", "env"]));
    assert_eq!(env["lease"]["id"], 1, "{env}");
    assert_eq!(env["env"]["DEVLAB_SLOT"], "0", "{env}");
    assert!(env["envFile"].is_string(), "{env}");

    let status = one_line(&lab(&["status"])).to_owned();
    assert!(named_file(&status).contains("stack slots"), "{status}");
    let board = one_json(&lab(&["status", "--json"]));
    assert_eq!(board["leases"][0]["id"], 1, "{board}");
    assert!(board["waiters"].is_array(), "{board}");
    let globally = sandbox.factory_in(&wt, &["--json", "lab", "status"]);
    assert!(one_json(&globally)["slots"].is_array(), "{globally:?}");

    assert_eq!(one_line(&lab(&["stack"])), "stack demo on slot 0 is down");
    assert_eq!(
        one_json(&lab(&["--json", "stack", "status"]))["state"],
        "down"
    );
    assert_eq!(one_line(&lab(&["gc"])), "no lease of an exited owner");
    assert_eq!(
        one_json(&lab(&["gc", "--json"]))["released"],
        serde_json::json!([])
    );
    let help = one_json(&lab(&["help", "--json"]));
    assert!(
        help["help"].as_str().is_some_and(|h| h.contains("devlab")),
        "{help}"
    );

    assert_eq!(one_line(&lab(&["release"])), "released lease 1");
    assert_eq!(
        one_json(&lab(&["--json", "release"]))["released"],
        serde_json::Value::Null
    );
    assert_eq!(one_line(&lab(&["release"])), "nothing held here");

    let refused = lab(&["frobnicate"]);
    assert_eq!(
        (
            refused.code,
            refused.stdout.as_str(),
            refused.stderr.as_str()
        ),
        (
            2,
            "",
            "factory: unknown command frobnicate; factory lab help\n"
        )
    );
}

#[test]
fn factory_lab_is_silent_while_it_waits() {
    let (sandbox, world) = lab();
    let (a, b) = (Agent::start(), Agent::start());
    let [wa, wb, wc] = ["demo-a", "demo-b", "demo-c"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let me = std::process::id();
    let waited = native(
        &sandbox,
        &wc,
        me,
        &["lab", "acquire", "--no-device", "--wait", "1"],
    );
    let as_json = native(
        &sandbox,
        &wc,
        me,
        &["lab", "acquire", "--no-device", "--wait", "0", "--json"],
    );

    assert_eq!(
        (waited.code, waited.stdout.as_str(), waited.stderr.as_str()),
        (
            75,
            "",
            "factory: gave up after 1 s: all 2 stack slots of demo are held\n"
        )
    );
    assert_eq!(as_json.code, 75, "{as_json:?}");
    assert_eq!(
        one_json(&as_json),
        serde_json::json!({"exit": 75, "error": "gave up after 0 s: all 2 stack slots of demo are held"})
    );
}

#[test]
fn a_queued_request_keeps_its_deadline_across_a_daemon_restart() {
    let (sandbox, world) = lab();
    let (a, b, c) = (Agent::start(), Agent::start(), Agent::start());
    let [wa, wb, wc] = ["demo-a", "demo-b", "demo-c"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    let queued = spawn(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "30"],
    ));
    let me = std::process::id();
    let waiter = || {
        let board = one_json(&native(&sandbox, &wa, me, &["lab", "status", "--json"]));
        board["waiters"][0].clone()
    };
    let mut before = serde_json::Value::Null;
    assert!(poll_until(|| {
        before = waiter();
        !before.is_null()
    }));
    stop_daemon(&sandbox);
    let after = waiter();
    kill_group(queued.id(), Signal::TERM);
    let _ = finish(queued);

    assert!(before["deadline_ms"].is_i64(), "{before}");
    assert_eq!(
        (&after["id"], &after["deadline_ms"]),
        (&before["id"], &before["deadline_ms"])
    );
}
