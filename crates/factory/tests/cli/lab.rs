//! `factory lab` beyond what the golden transcripts show: lease ids, owners, the store's write
//! lock while the fakes of docker and systemctl run, the devices the daemon tracks and boots,
//! the queue across daemon restarts, stacks being stopped, and the output contract under the
//! `factory lab` name.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::process::Signal;

pub(super) use crate::support::stop_daemon;
use crate::support::world::{World, fixtures, poll_until};
use crate::support::{Run, Sandbox, finish, freeze, kill_group, run, spawn, thaw};

/// The sandbox with the golden world installed and the built binary on PATH as `devlab`.
pub(super) fn lab() -> (Sandbox, World) {
    let sandbox = Sandbox::new();
    let world = World::install(&sandbox);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_factory"), sandbox.path("bin/devlab"))
        .expect("link devlab");
    (sandbox, world)
}

pub(super) fn devlab(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Command {
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

pub(super) fn write_machine_config(sandbox: &Sandbox, extra: &str) {
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
fn factory_lab_names_a_file_with_devlabs_help_and_takes_the_global_json_flag() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-name");
    let as_devlab = owned(&sandbox, &wt, &["help"]);
    for args in [&["lab", "help"][..], &["lab"]] {
        let as_factory = sandbox.factory_in(&wt, args);
        let line = one_line(&as_factory);
        assert!(line.starts_with("factory lab: acquire, run, "), "{line}");
        assert_eq!(named_file(line), as_devlab.stdout, "{line}");
    }

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
            "devlab: --wait takes seconds, or a whole number and s, m or h such as 90s or 9m (0 = do not wait)\n"
        )
    );
}

#[test]
fn a_wait_takes_a_unit_too() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-unit");

    let unit = owned(&sandbox, &wt, &["acquire", "--no-device", "--wait", "1m"]);
    let bad = owned(&sandbox, &wt, &["acquire", "--no-device", "--wait", "1.5m"]);

    assert_eq!(unit.code, 0, "{unit:?}");
    assert!(unit.stdout.contains("DEVLAB_SLOT="), "{unit:?}");
    assert_eq!(
        (bad.code, bad.stderr.as_str()),
        (
            2,
            "devlab: --wait takes seconds, or a whole number and s, m or h such as 90s or 9m (0 = do not wait)\n"
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
            "devlab: --wait takes seconds, or a whole number and s, m or h such as 90s or 9m (0 = do not wait)\n"
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
        &[
            "run",
            "--no-device",
            "--quiet",
            "--keep",
            "--",
            "sh",
            "-c",
            parent,
        ],
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
            "devlab: --wait takes seconds, or a whole number and s, m or h such as 90s or 9m (0 = do not wait)\n"
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

/// Fakes `tool` as the world does, after it records whether the lab could write meanwhile: a
/// `devlab gc`, which writes to the store, that answers within its deadline finds no write
/// transaction open around `tool`.
fn probe_lock(sandbox: &Sandbox, tool: &str, probes: &Path) {
    let body = fs::read_to_string(fixtures().join(format!("fakes/{tool}.sh")))
        .expect("read a fake")
        .replace("@WORLD@", &sandbox.path("world").display().to_string());
    let probe = r#"if timeout 3 devlab gc >/dev/null 2>&1; then
  echo '@TOOL@ free' >> '@PROBES@'
else
  echo "@TOOL@ held: devlab gc exited $?" >> '@PROBES@'
fi
"#
    .replace("@PROBES@", &probes.display().to_string())
    .replace("@TOOL@", tool);
    sandbox.fake(tool, &(probe + &body));
}

#[test]
fn docker_runs_while_the_store_is_unlocked() {
    let (sandbox, world) = lab();
    let probes: PathBuf = sandbox.path("probes");
    let wt = world.worktree("demo-lock");
    probe_lock(&sandbox, "docker", &probes);
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
    assert!(seen.contains("docker free"), "docker never ran: {seen}");
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
    // The lease went with it: nothing is held here.
    let env = owned(&sandbox, &wt, &["env"]);
    assert_eq!(env.code, 4, "{env:?}");
}

#[test]
fn an_emulator_a_dead_daemon_left_booting_is_stopped_and_forgotten_never_ready() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    world.apply("stall emulator-5580").unwrap();
    let child = boot_in_background(&sandbox, &wt, "--emulator");
    stop_daemon(&sandbox);

    // The next daemon stops the emulator its dead one left booting, never marking it ready.
    let kill = vec![
        "-s".to_owned(),
        "emulator-5580".to_owned(),
        "emu".to_owned(),
        "kill".to_owned(),
    ];
    let stopped = poll_until(|| sandbox.calls("adb").contains(&kill));
    assert!(
        stopped,
        "the emulator was not stopped: {:?}",
        sandbox.calls("adb")
    );
    // The caller asks the next daemon again, and an emulator that was never ready is not there
    // to reuse: its acquire boots afresh.
    fs::write(sandbox.path("world/stalled"), "").unwrap();
    let again = finish(child);
    assert_eq!(again.code, 0, "{again:?}");
    assert!(again.stderr.contains("booting 1 emulator(s)"), "{again:?}");
}

#[test]
fn a_pair_whose_emulator_fails_to_boot_gives_back_its_phone_too() {
    let (sandbox, world) = lab();
    // A phone and an emulator: over the one device a lab of one phone leaves an owner.
    write_machine_config(&sandbox, r#""maxDevicesPerOwner": 2"#);
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
    assert_eq!(env.code, 4, "{env:?}");
}

#[test]
fn a_failed_emulator_boot_frees_the_slot_for_the_next_owner() {
    let (sandbox, world) = lab();
    let (a, b) = (Agent::start(), Agent::start());
    let [wa, wb] = ["demo-a", "demo-b"].map(|n| world.worktree(n));
    world.apply("break systemd-run").unwrap();

    let failed = run(for_owner(
        &sandbox,
        &wa,
        a.pid(),
        &["acquire", "--emulator"],
    ));
    assert_eq!(failed.code, 77, "{failed:?}");
    world.apply("mend systemd-run").unwrap();
    let again = run(for_owner(
        &sandbox,
        &wb,
        b.pid(),
        &["acquire", "--no-device"],
    ));

    // Both slots of demo are free again: the failed lease took neither.
    assert!(again.stdout.contains("DEVLAB_SLOT=0"), "{again:?}");
    let env = run(for_owner(&sandbox, &wa, a.pid(), &["env"]));
    assert_eq!(env.code, 4, "{env:?}");
}

#[test]
fn a_top_up_whose_emulator_fails_to_boot_gives_the_whole_lease_back() {
    let (sandbox, world) = lab();
    // A phone and an emulator: over the one device a lab of one phone leaves an owner.
    write_machine_config(&sandbox, r#""maxDevicesPerOwner": 2"#);
    let wt = world.worktree("plain-a");
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    let held = owned(&sandbox, &wt, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    world.apply("break systemd-run").unwrap();

    let more = owned(
        &sandbox,
        &wt,
        &["acquire", "--devices", "2", "--emulator", "--quiet"],
    );

    assert_eq!(more.code, 77, "{more:?}");
    // The phone it already held is free again too.
    let env = owned(&sandbox, &wt, &["env"]);
    assert_eq!(env.code, 4, "{env:?}");
    assert!(
        status(&sandbox, &wt).contains("Pixel 8        ready        free"),
        "{}",
        status(&sandbox, &wt)
    );
}

#[test]
fn a_phone_unplugged_while_its_pair_boots_gives_the_acquire_exit_76() {
    let (sandbox, world) = lab();
    // A phone and an emulator: over the one device a lab of one phone leaves an owner.
    write_machine_config(&sandbox, r#""maxDevicesPerOwner": 2"#);
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

/// `devlab` run in `dir` for the owner process `pid`.
pub(crate) fn for_owner(sandbox: &Sandbox, dir: &Path, pid: u32, args: &[&str]) -> Command {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", pid.to_string());
    cmd
}

/// A stand-in agent: a live process to own leases, killed when dropped.
pub(crate) struct Agent(std::process::Child);

impl Agent {
    pub(crate) fn start() -> Agent {
        Agent(
            Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("start an owner"),
        )
    }

    pub(crate) fn pid(&self) -> u32 {
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

/// The world's fake of `tool`, except that its first `holds` calls of `verb` each mark that they
/// began, then hold until the test opens their gate (or the test deadline passes).
fn hold(sandbox: &Sandbox, tool: &str, verb: &str, holds: u32) -> Holds {
    let dir = sandbox.path(format!("holds-{tool}"));
    fs::create_dir_all(&dir).expect("create the holds dir");
    let body = fs::read_to_string(fixtures().join(format!("fakes/{tool}.sh")))
        .expect("read a fake")
        .replace("@WORLD@", &sandbox.path("world").display().to_string());
    let hold = format!(
        "if [ \"$1\" = {verb} ]; then\n\
         \x20 n=1; while ! mkdir '{dir}/began-'$n 2>/dev/null; do n=$((n+1)); done\n\
         \x20 if [ $n -le {holds} ]; then i=0\n\
         \x20   while [ ! -e '{dir}/gate-'$n ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\n\
         \x20 fi\n\
         fi\n",
        dir = dir.display()
    );
    sandbox.fake(tool, &(hold + &body));
    Holds(dir)
}

/// The calls [`hold`] holds, by number from 1.
struct Holds(PathBuf);

impl Holds {
    fn began(&self, n: u32) -> bool {
        self.0.join(format!("began-{n}")).exists()
    }

    fn open(&self, n: u32) {
        fs::write(self.0.join(format!("gate-{n}")), "").expect("open a gate");
    }
}

/// Owner a holds demo's slot 0 with its stack up and b holds slot 1. While a's
/// `release --stop-stack` is still stopping the stack, `free` drops a's lease some other way:
/// slot 0 stays held until the stack is down, then goes to the next owner.
fn a_slot_stays_held_while_its_stack_stops(free: impl FnOnce(&Sandbox, &Path, &mut Agent, &Holds)) {
    let (sandbox, world) = lab();
    let (mut a, b, c) = (Agent::start(), Agent::start(), Agent::start());
    let [wa, wb, wc] = ["demo-a", "demo-b", "demo-c"].map(|n| world.worktree(n));
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
    let holds = hold(&sandbox, "supabase", "stop", 2);
    let stopping = spawn(for_owner(
        &sandbox,
        &wa,
        a.pid(),
        &["release", "--stop-stack"],
    ));
    assert!(poll_until(|| holds.began(1)), "supabase stop never ran");

    free(&sandbox, &wa, &mut a, &holds);
    let mid_stop = run(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "0"],
    ));
    holds.open(1);
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
    a_slot_stays_held_while_its_stack_stops(|sandbox, wa, a, _| {
        let plain = run(for_owner(sandbox, wa, a.pid(), &["release"]));
        assert_eq!(
            plain.stderr,
            format!("devlab: released lease 1 ({})\n", wa.display()),
            "{plain:?}"
        );
    });
}

#[test]
fn an_owner_that_exits_mid_stop_leaves_the_slot_held_until_the_stack_is_down() {
    a_slot_stays_held_while_its_stack_stops(|sandbox, wa, a, _| {
        a.exit();
        // The daemon frees it the moment a exits, or this gc does: either way it is gone.
        let gc = run(devlab(sandbox, wa, &["gc"]));
        assert_eq!(gc.code, 0, "{gc:?}");
        assert!(
            gc.stdout.is_empty() || gc.stdout.starts_with("released lease 1 "),
            "{gc:?}"
        );
    });
}

#[test]
fn a_second_stop_that_ends_first_leaves_the_slot_held_for_the_first() {
    a_slot_stays_held_while_its_stack_stops(|sandbox, wa, a, holds| {
        let second = spawn(for_owner(
            sandbox,
            wa,
            a.pid(),
            &["release", "--stop-stack"],
        ));
        assert!(
            poll_until(|| holds.began(2)),
            "the second supabase stop never ran"
        );
        holds.open(2);
        let second = finish(second);
        assert_eq!(
            second.stderr,
            format!("devlab: released lease 1 ({})\n", wa.display()),
            "{second:?}"
        );
    });
}

/// The worktrees `devlab status` shows waiting, oldest first.
pub(crate) fn waiting(sandbox: &Sandbox, dir: &Path) -> Vec<String> {
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
    let holds = hold(&sandbox, "docker", "inspect", 1);
    let agent = Agent::start();
    let asking = spawn(for_owner(
        &sandbox,
        &wt,
        agent.pid(),
        &["acquire", "--no-device"],
    ));
    // The daemon looks at the stacks before it grants: the caller leaves meanwhile.
    assert!(poll_until(|| holds.began(1)), "docker inspect never ran");
    kill_group(asking.id(), Signal::KILL);
    let _ = finish(asking);
    holds.open(1);

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

    let ran = lab(&["exec", "--", "sh", "-c", "echo one; echo two >&2; exit 3"]);
    assert_eq!((ran.code, ran.stderr.as_str()), (3, ""), "{ran:?}");
    assert_eq!(ran.stdout.lines().count(), 1, "{ran:?}");
    assert!(
        ran.stdout.starts_with("sh exited 3 in lease 1; output in "),
        "{ran:?}"
    );
    assert_eq!(named_file(ran.stdout.trim_end()), "one\ntwo\n");
    let ran = lab(&[
        "run",
        "--json",
        "--no-device",
        "--",
        "sh",
        "-c",
        "echo $DEVLAB_SLOT",
    ]);
    assert_eq!(ran.code, 0, "{ran:?}");
    let ran = one_json(&ran);
    assert_eq!(
        (&ran["lease"], &ran["exit"]),
        (&serde_json::json!(1), &serde_json::json!(0)),
        "{ran}"
    );
    let output = ran["outputFile"].as_str().unwrap_or_default();
    assert_eq!(
        fs::read_to_string(output).unwrap_or_default(),
        "0\n",
        "{ran}"
    );

    assert_eq!(
        one_line(&lab(&["release"])),
        format!("released lease 1 ({})", wt.display())
    );
    assert_eq!(
        one_json(&lab(&["--json", "release"]))["released"],
        serde_json::json!([])
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
fn factory_lab_preps_and_watches_as_devlab_does_in_one_line_each() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-prep");
    let me = std::process::id();
    let lab = |args: &[&str]| native(&sandbox, &wt, me, &[&["lab"], args].concat());
    for phone in ["FAKEPHONE01", "FAKEPHONE03"] {
        world
            .apply(&format!("plug {phone} device usb:1-1 model:Pixel_7"))
            .unwrap();
    }
    world.apply("lock FAKEPHONE03").unwrap();

    assert_eq!(
        one_line(&lab(&["prep", "FAKEPHONE01"])),
        "FAKEPHONE01: ready"
    );
    let both = one_line(&lab(&["prep"])).to_owned();
    assert!(
        both.starts_with("prepped 2 devices, 1 with a warning; list in "),
        "{both}"
    );
    assert_eq!(
        named_file(&both),
        "FAKEPHONE01: ready\nFAKEPHONE03: lock screen still showing: unlock the phone once\n"
    );
    let json = one_json(&lab(&["prep", "--json", "FAKEPHONE03"]));
    assert_eq!(json["devices"][0]["serial"], "FAKEPHONE03", "{json}");
    assert!(
        json["devices"][0]["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with("lock screen")),
        "{json}"
    );

    assert_eq!(
        one_line(&lab(&["watch"])),
        "the factory daemon frees leases and stops idle emulators and stacks now; nothing to start"
    );
    assert_eq!(one_json(&lab(&["watch", "--json"]))["watcher"], "daemon");
}

#[test]
fn factory_lab_reserve_help_is_there_where_its_refusal_points() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-lead");
    let lab = |args: &[&str]| {
        native(
            &sandbox,
            &wt,
            std::process::id(),
            &[&["lab"], args].concat(),
        )
    };

    let refused = lab(&["reserve", "--soon"]);
    assert_eq!(
        (refused.code, refused.stderr.as_str()),
        (
            2,
            "factory: unknown option --soon; factory lab reserve --help\n"
        ),
        "{refused:?}"
    );
    let help = one_line(&lab(&["reserve", "--help"])).to_owned();
    assert!(help.starts_with("factory lab reserve: usage in "), "{help}");
    assert!(
        named_file(&help).starts_with("usage: factory lab reserve [--slots N]"),
        "{help}"
    );
}

#[test]
fn a_failed_boot_takes_back_nothing_another_lease_was_granted_since() {
    let (sandbox, world) = lab();
    // A phone and an emulator: over the one device a lab of one phone leaves an owner.
    write_machine_config(&sandbox, r#""maxDevicesPerOwner": 2"#);
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
fn a_wait_of_minus_one_queues_for_the_configured_default_and_other_negatives_are_refused() {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""waitSec": 1"#);
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

    let queued = run(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "-1"],
    ));
    let ran = run(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["run", "--no-device", "--wait", "-1", "--", "true"],
    ));
    let refused = run(for_owner(
        &sandbox,
        &wc,
        c.pid(),
        &["acquire", "--no-device", "--wait", "-2"],
    ));

    for out in [&queued, &ran] {
        assert_eq!(out.code, 75, "{out:?}");
        assert!(
            out.stderr
                .ends_with("devlab: gave up after 1 s: all 2 stack slots of demo are held\n"),
            "{out:?}"
        );
    }
    assert_eq!(refused.code, 2, "{refused:?}");
    assert_eq!(
        refused.stderr,
        "devlab: --wait takes seconds, or a whole number and s, m or h such as 90s or 9m (0 = do not wait)\n"
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

fn epoch_ms() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock past 1970");
    i64::try_from(since.as_millis()).expect("a clock before 2262")
}

#[test]
fn a_wait_that_ran_out_while_the_daemon_was_down_takes_no_slot_freed_meanwhile() {
    let (sandbox, world) = lab();
    let (mut a, b, c) = (Agent::start(), Agent::start(), Agent::start());
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
        &["acquire", "--no-device", "--wait", "3"],
    ));
    let me = std::process::id();
    let mut deadline = 0;
    assert!(poll_until(|| {
        let board = one_json(&native(&sandbox, &wa, me, &["lab", "status", "--json"]));
        deadline = board["waiters"][0]["deadline_ms"].as_i64().unwrap_or(0);
        deadline > 0
    }));

    // The caller sleeps through the restart; its wait runs out and a slot comes free meanwhile.
    kill_group(queued.id(), Signal::STOP);
    stop_daemon(&sandbox);
    assert!(
        epoch_ms() < deadline,
        "the daemon gave up before it stopped"
    );
    a.exit();
    assert!(poll_until(|| epoch_ms() > deadline));
    kill_group(queued.id(), Signal::CONT);
    let gave_up = finish(queued);

    assert_eq!(gave_up.code, 75, "{gave_up:?}");
    assert_eq!(
        gave_up.stderr.lines().last(),
        Some("devlab: gave up after 3 s: the wait ran out while the daemon restarted"),
        "{gave_up:?}"
    );
    assert!(!gave_up.stdout.contains("DEVLAB_SLOT"), "{gave_up:?}");
}

/// Field 22 of `/proc/<pid>/stat`: when the process started.
fn started(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("a live process");
    stat.rsplit(')')
        .next()
        .and_then(|rest| rest.split_whitespace().nth(19))
        .expect("a start time")
        .to_owned()
}

/// Sends the daemon a `lab.acquire` for a one-slot project straight over its socket, by
/// `owner` from the devlab call `client`; the connection, and the first line that holds `until`
/// (`queued` or `result`) or an error.
fn ask(
    sandbox: &Sandbox,
    worktree: &Path,
    owner: u32,
    client: u32,
    until: &str,
) -> (UnixStream, serde_json::Value) {
    let params = serde_json::json!({
        "worktree": worktree,
        "owner": {"pid": owner, "start": started(owner), "label": format!("sleep:{owner}"), "found": true},
        "lease": null,
        "project": {"name": "solo", "max_slots": 1, "stride": 10, "cap": 1, "ports": [], "stack": null},
        "wait_sec": 30,
        "client": {"pid": client, "start": started(client)},
    });
    let mut stream =
        UnixStream::connect(sandbox.path("run/factory/factory.sock")).expect("reach the daemon");
    let request = serde_json::json!({"id": 1, "method": "lab.acquire", "params": params});
    writeln!(stream, "{request}").expect("send a request");
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).expect("read the daemon") > 0,
            "the daemon hung up"
        );
        let answer: serde_json::Value = serde_json::from_str(&line).expect("a JSON line");
        if answer.get(until).is_some() || answer.get("error").is_some() {
            return (stream, answer);
        }
    }
}

#[test]
fn a_request_its_caller_sends_again_keeps_the_place_it_never_heard_of() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-a");
    let (a, b, caller) = (Agent::start(), Agent::start(), Agent::start());
    let daemon = || run(devlab(&sandbox, &wt, &["status"]));
    assert_eq!(daemon().code, 0);
    let (mut first, held) = ask(&sandbox, &wt, a.pid(), caller.pid(), "result");
    assert!(held["result"]["lease"]["slot"] == 0, "{held}");
    assert_eq!(received(&mut first)["result"], true);
    let (gone, queued) = ask(&sandbox, &wt, b.pid(), caller.pid(), "queued");
    assert!(queued["queued"].is_i64(), "{queued}");

    // The daemon restarts before the caller hears of its place: it asks again from scratch.
    stop_daemon(&sandbox);
    drop(gone);
    assert_eq!(daemon().code, 0);
    let (_again, requeued) = ask(&sandbox, &wt, b.pid(), caller.pid(), "queued");
    let board = one_json(&native(
        &sandbox,
        &wt,
        std::process::id(),
        &["lab", "status", "--json"],
    ));

    assert_eq!(requeued["queued"], queued["queued"], "{requeued}");
    assert_eq!(
        board["waiters"].as_array().map(Vec::len),
        Some(1),
        "{board}"
    );
}

#[test]
fn gc_has_nothing_to_say_of_leases_the_daemon_already_freed() {
    let (sandbox, world) = lab();
    let (mut a, mut b) = (Agent::start(), Agent::start());
    let [wa, wb] = ["demo-a", "demo-b"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let held = run(for_owner(
            &sandbox,
            wt,
            agent.pid(),
            &["acquire", "--no-device"],
        ));
        assert_eq!(held.code, 0, "{held:?}");
    }
    a.exit();
    b.exit();
    let gone = poll_until(|| {
        [(&wa, &a), (&wb, &b)]
            .iter()
            .all(|(wt, agent)| run(for_owner(&sandbox, wt, agent.pid(), &["env"])).code == 4)
    });
    assert!(
        gone,
        "the daemon never freed the leases of the owners that exited"
    );

    // The daemon freed them: gc has nothing of its own to say.
    let gc = native(&sandbox, &wa, std::process::id(), &["lab", "gc"]);
    assert_eq!(one_line(&gc), "no lease of an exited owner");
    let plain = run(devlab(&sandbox, &wa, &["gc"]));
    assert_eq!((plain.code, plain.stdout.as_str()), (0, ""), "{plain:?}");
}

/// A lead that reserved a slot and exited: the keeper watches leases' owners, not reservations',
/// so only a write that finds the reservation frees it, and a `gc` that is the next call does.
fn orphaned_reservations(sandbox: &Sandbox, world: &World) -> (PathBuf, Vec<u32>) {
    let (mut a, mut b) = (Agent::start(), Agent::start());
    let [wa, wb] = ["demo-a", "demo-b"].map(|n| world.worktree(n));
    for (agent, wt) in [(&a, &wa), (&b, &wb)] {
        let reserved = run(for_owner(sandbox, wt, agent.pid(), &["reserve"]));
        assert_eq!(reserved.code, 0, "{reserved:?}");
    }
    let pids = vec![a.pid(), b.pid()];
    a.exit();
    b.exit();
    (wa, pids)
}

#[test]
fn gc_names_a_file_with_the_reservations_it_freed_itself() {
    let (sandbox, world) = lab();
    let (wa, pids) = orphaned_reservations(&sandbox, &world);

    let gc = native(&sandbox, &wa, std::process::id(), &["lab", "gc"]);

    let line = one_line(&gc);
    assert!(
        line.starts_with("released 2 leases of exited owners; list in "),
        "{line}"
    );
    let freed = named_file(line);
    for pid in pids {
        assert!(
            freed.contains(&format!("(sleep:{pid}, owner exited)")),
            "{freed}"
        );
    }
    // Said once: the next gc finds nothing.
    let again = run(devlab(&sandbox, &wa, &["gc"]));
    assert_eq!((again.code, again.stdout.as_str()), (0, ""), "{again:?}");
}

#[test]
fn devlab_gc_says_a_freed_reservation_once() {
    let (sandbox, world) = lab();
    let (wa, pids) = orphaned_reservations(&sandbox, &world);

    let gc = run(devlab(&sandbox, &wa, &["gc"]));
    let again = run(devlab(&sandbox, &wa, &["gc"]));

    assert_eq!(gc.code, 0, "{gc:?}");
    assert_eq!(gc.stdout.lines().count(), 2, "{gc:?}");
    for (line, pid) in gc.stdout.lines().zip(&pids) {
        assert!(
            line.starts_with("released reservation ")
                && line.ends_with(&format!("(sleep:{pid}, owner exited)")),
            "{line}"
        );
    }
    assert_eq!((again.code, again.stdout.as_str()), (0, ""), "{again:?}");
}

/// Tells the daemon the grant on `stream` reached its caller; its confirmation.
fn received(stream: &mut UnixStream) -> serde_json::Value {
    let said = serde_json::json!({"id": 2, "method": "lab.received"});
    writeln!(stream, "{said}").expect("say the grant arrived");
    let mut line = String::new();
    BufReader::new(stream.try_clone().expect("clone the stream"))
        .read_line(&mut line)
        .expect("read the daemon");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
}

/// The ids of the leases on the one-slot project [`ask`] asks for.
fn solo_leases(sandbox: &Sandbox, dir: &Path) -> Vec<i64> {
    let board = one_json(&native(
        sandbox,
        dir,
        std::process::id(),
        &["lab", "status", "--json"],
    ));
    board["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|lease| lease["project"] == "solo")
        .filter_map(|lease| lease["id"].as_i64())
        .collect()
}

#[test]
fn a_grant_its_caller_never_says_it_received_is_given_back() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-a");
    let (a, caller) = (Agent::start(), Agent::start());
    assert_eq!(run(devlab(&sandbox, &wt, &["status"])).code, 0);
    let (unread, held) = ask(&sandbox, &wt, a.pid(), caller.pid(), "result");
    assert!(held["result"]["lease"]["id"].is_i64(), "{held}");

    drop(unread);

    assert!(
        poll_until(|| solo_leases(&sandbox, &wt).is_empty()),
        "the grant outlived its caller"
    );
}

#[test]
fn a_grant_lost_in_a_restart_goes_to_its_caller_when_it_asks_again() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-a");
    let (a, mut caller) = (Agent::start(), Agent::start());
    let daemon = || run(devlab(&sandbox, &wt, &["status"]));
    assert_eq!(daemon().code, 0);
    let (lost, held) = ask(&sandbox, &wt, a.pid(), caller.pid(), "result");

    // The daemon restarts before it hears that the grant arrived.
    stop_daemon(&sandbox);
    drop(lost);
    assert_eq!(daemon().code, 0);
    let (mut again, regranted) = ask(&sandbox, &wt, a.pid(), caller.pid(), "result");
    let confirmed = received(&mut again);
    drop(again);
    caller.exit();

    assert_eq!(
        regranted["result"]["lease"], held["result"]["lease"],
        "{regranted}"
    );
    assert_eq!(confirmed["result"], true, "{confirmed}");
    assert_eq!(
        solo_leases(&sandbox, &wt),
        vec![held["result"]["lease"]["id"].as_i64().unwrap_or_default()]
    );
}

#[test]
fn a_grant_lost_in_a_restart_is_given_back_once_its_caller_exits() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-a");
    let (a, mut caller) = (Agent::start(), Agent::start());
    let daemon = || run(devlab(&sandbox, &wt, &["status"]));
    assert_eq!(daemon().code, 0);
    let (lost, held) = ask(&sandbox, &wt, a.pid(), caller.pid(), "result");
    assert!(held["result"]["lease"]["id"].is_i64(), "{held}");

    stop_daemon(&sandbox);
    drop(lost);
    caller.exit();
    assert_eq!(daemon().code, 0);

    assert_eq!(solo_leases(&sandbox, &wt), Vec::<i64>::new());
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
pub(super) fn await_waiter(sandbox: &Sandbox, dir: &Path) {
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
fn a_run_whose_emulator_fails_to_boot_records_no_lease_it_gave_back() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-boot-fail");
    world.apply("break systemd-run").unwrap();

    let out = owned(
        &sandbox,
        &wt,
        &["run", "--emulator", "--quiet", "--", "true"],
    );

    assert_eq!(out.code, 77, "{out:?}");
    let event = the_lab_run(&sandbox);
    assert!(event.get("lease").is_none(), "{event}");
    assert!(event.get("slot").is_none(), "{event}");
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
fn a_stopped_run_keeps_its_lease_until_its_command_is_gone_then_gives_it_back() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-stopping");
    let script = r#"trap 'kill $!; touch got; while [ ! -e go ]; do sleep 0.05; done; exit 0' TERM; sleep 30 & touch started; wait"#;
    let child = spawn_run(&sandbox, &wt, script);
    assert!(
        poll_until(|| wt.join("started").exists()),
        "the command never started"
    );

    signal(child.id(), Signal::TERM);
    assert!(
        poll_until(|| wt.join("got").exists()),
        "the command never got the signal"
    );
    // The command still runs, cleaning up: the lease is still its.
    let held = leases_on(&board(&sandbox, &wt));
    assert_eq!(held.len(), 1, "{held:?}");

    fs::write(wt.join("go"), "").expect("write go");
    let out = finish(child);
    assert_eq!(out.code, 143, "{out:?}");
    assert!(leases_on(&board(&sandbox, &wt)).is_empty());
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

/// The lab board, as `factory lab status --json` shows it.
fn board(sandbox: &Sandbox, dir: &Path) -> serde_json::Value {
    one_json(&native(
        sandbox,
        dir,
        std::process::id(),
        &["lab", "status", "--json"],
    ))
}

/// Each lease on the board: its id, slot and worktree.
fn leases_on(board: &serde_json::Value) -> Vec<(i64, i64, String)> {
    board["leases"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|lease| {
            (
                lease["id"].as_i64().unwrap_or_default(),
                lease["slot"].as_i64().unwrap_or(-1),
                lease["worktree"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// The waiters on the board, oldest first: their ids and worktrees.
fn waiters_on(board: &serde_json::Value) -> Vec<(i64, String)> {
    board["waiters"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|waiter| {
            (
                waiter["id"].as_i64().unwrap_or_default(),
                waiter["worktree"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// A command for `spawn_run` that starts, then ends with `code` once the test writes `go`.
fn until_go(code: u8) -> String {
    format!("echo $$ > child.pid; while [ ! -e go ]; do sleep 0.1; done; exit {code}")
}

#[test]
fn a_run_whose_command_runs_across_a_daemon_restart_keeps_its_lease_and_records_its_exit() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-restart");
    let child = spawn_run(&sandbox, &wt, &until_go(3));
    assert!(
        poll_until(|| wt.join("child.pid").exists()),
        "the command never started"
    );
    let held = leases_on(&board(&sandbox, &wt));

    let stopping = std::time::Instant::now();
    stop_daemon(&sandbox);
    let took = stopping.elapsed();
    let kept = leases_on(&board(&sandbox, &wt));
    fs::write(wt.join("go"), "").expect("write go");
    let out = finish(child);

    assert!(
        took < std::time::Duration::from_secs(5),
        "the stop took {took:?}"
    );
    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(kept, held);
    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "exit 3", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
    assert_eq!(event["lease"], held[0].0, "{event}");
    assert_eq!(event["slot"], 0, "{event}");
}

#[test]
fn a_run_whose_command_outlasts_a_daemon_answer_is_still_followed_across_a_restart() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-long");
    // Longer than devlab waits for any one answer of the daemon's (10 s).
    let script = "sleep 11; touch slept; while [ ! -e go ]; do sleep 0.1; done; exit 4".to_owned();
    let child = spawn_run(&sandbox, &wt, &script);
    assert!(
        poll_until(|| wt.join("slept").exists()),
        "the command never slept"
    );

    stop_daemon(&sandbox);
    let _ = board(&sandbox, &wt);
    fs::write(wt.join("go"), "").expect("write go");
    let out = finish(child);

    assert_eq!(out.code, 4, "{out:?}");
    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "exit 4", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
}

#[test]
fn a_signal_while_a_run_waits_for_its_end_to_be_recorded_keeps_the_commands_exit() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-settling");
    // A daemon a devlab started would be hung up on, frozen, once that devlab exits (its
    // process group orphaned): this one's starter is gone before the freeze.
    let _ = board(&sandbox, &wt);
    let child = spawn_run(&sandbox, &wt, &until_go(3));
    assert!(
        poll_until(|| wt.join("child.pid").exists()),
        "the command never started"
    );
    let command: u32 = fs::read_to_string(wt.join("child.pid"))
        .expect("child.pid")
        .trim()
        .parse()
        .expect("a pid");
    let daemon: u32 = fs::read_to_string(sandbox.path("run/factory/daemon.pid"))
        .expect("a daemon runs")
        .trim()
        .parse()
        .expect("a pid");

    // A frozen daemon never answers the run's end, so devlab waits for it.
    kill_group(daemon, Signal::STOP);
    fs::write(wt.join("go"), "").expect("write go");
    let ended = poll_until(|| !Path::new(&format!("/proc/{command}")).exists());
    signal(child.id(), Signal::TERM);
    let out = finish(child);
    kill_group(daemon, Signal::CONT);

    assert!(ended, "the command never ended");
    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.stderr, "", "{out:?}");
    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "exit 3", "{event}");
}

#[test]
fn runs_queued_across_a_daemon_restart_keep_their_places_and_are_granted_in_turn() {
    let (sandbox, world) = lab();
    hold_the_owners_slot(&sandbox, &world);
    let (first, second) = (world.worktree("demo-first"), world.worktree("demo-second"));
    let a = spawn_run(&sandbox, &first, "echo ran > ran");
    assert!(poll_until(
        || waiters_on(&board(&sandbox, &first)).len() == 1
    ));
    let b = spawn_run(&sandbox, &second, "echo ran > ran");
    assert!(poll_until(
        || waiters_on(&board(&sandbox, &first)).len() == 2
    ));
    let queued = waiters_on(&board(&sandbox, &first));

    stop_daemon(&sandbox);
    let requeued = waiters_on(&board(&sandbox, &first));
    let holder = world.worktree("demo-holder");
    assert_eq!(owned(&sandbox, &holder, &["release"]).code, 0);
    let out_a = finish(a);
    let second_waited = !second.join("ran").exists();
    assert_eq!(owned(&sandbox, &first, &["release"]).code, 0);
    let out_b = finish(b);

    assert_eq!(requeued, queued);
    assert_eq!(out_a.code, 0, "{out_a:?}");
    assert!(second_waited, "the second run went before the first");
    assert_eq!(out_b.code, 0, "{out_b:?}");
    assert!(poll_until(|| lab_runs(&sandbox).len() == 2));
    for event in lab_runs(&sandbox) {
        assert_eq!(event["outcome"], "exit 0", "{event}");
        assert_eq!(event["phase"], "running", "{event}");
    }
}

#[test]
fn a_run_a_daemon_stop_cut_off_for_good_is_recorded_as_daemon_stopped_not_lost() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-cut-off");
    let child = spawn_run(&sandbox, &wt, "echo $$ > child.pid; exec sleep 30");
    assert!(
        poll_until(|| wt.join("child.pid").exists()),
        "the command never started"
    );

    // Frozen, the run can't get back to a daemon before it is killed.
    freeze(&child);
    stop_daemon(&sandbox);
    kill_group(child.id(), Signal::KILL);
    let _ = finish(child);
    let after = board(&sandbox, &wt);

    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "daemon-stopped", "{event}");
    assert_eq!(event["phase"], "running", "{event}");
    // The lease the run made ends with it, however it ended (#53).
    assert!(leases_on(&after).is_empty(), "{after}");
}

#[test]
fn a_run_still_queued_when_the_daemon_stops_for_good_is_recorded_as_daemon_stopped() {
    let (sandbox, world) = lab();
    hold_the_owners_slot(&sandbox, &world);
    let wt = world.worktree("demo-queued-gone");
    let child = spawn_run(&sandbox, &wt, "true");
    await_waiter(&sandbox, &wt);

    freeze(&child);
    stop_daemon(&sandbox);
    kill_group(child.id(), Signal::KILL);
    let _ = finish(child);
    let after = board(&sandbox, &wt);

    let event = the_lab_run(&sandbox);
    assert_eq!(event["outcome"], "daemon-stopped", "{event}");
    assert_eq!(event["phase"], "queued", "{event}");
    assert_eq!(event["run_s"], 0, "{event}");
    assert_eq!(waiters_on(&after), Vec::new());
}

/// A process that stands in for an agent owning leases, until the test kills it.
fn owner_process() -> std::process::Child {
    let mut cmd = Command::new("sleep");
    cmd.arg("120").stdin(std::process::Stdio::null());
    spawn(cmd)
}

/// `devlab` run with `owner` as its owner.
fn owned_by(sandbox: &Sandbox, dir: &Path, owner: u32, args: &[&str]) -> Run {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", owner.to_string());
    run(cmd)
}

/// Kills an [`owner_process`] and reaps it, as its parent would.
fn kill_owner(owner: std::process::Child) {
    signal(owner.id(), Signal::KILL);
    let _ = finish(owner);
}

#[test]
fn an_owner_that_exits_frees_its_lease_within_a_second_even_before_it_is_reaped() {
    let (sandbox, world) = lab();
    let (a, b) = (owner_process(), owner_process());
    for (owner, name) in [(&a, "demo-a"), (&b, "demo-b")] {
        let held = owned_by(
            &sandbox,
            &world.worktree(name),
            owner.id(),
            &["acquire", "--no-device", "--quiet"],
        );
        assert_eq!(held.code, 0, "{held:?}");
    }
    let wt = world.worktree("demo-c");
    // Status waits for adb's first device list, whose news would wake the queue by chance.
    assert!(status(&sandbox, &wt).starts_with("devices: none attached\n"));
    let mut queued = devlab(
        &sandbox,
        &wt,
        &["acquire", "--no-device", "--quiet", "--wait", "60"],
    );
    queued.env("DEVLAB_OWNER_PID", std::process::id().to_string());
    let queued = spawn(queued);
    await_waiter(&sandbox, &wt);

    // Killed and not yet reaped: a zombie with its pid and start time, which no longer owns.
    let killed = std::time::Instant::now();
    signal(a.id(), Signal::KILL);
    let out = finish(queued);
    let took = killed.elapsed();
    let _ = finish(a);
    kill_owner(b);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.contains("export DEVLAB_SLOT=0\n"), "{out:?}");
    assert!(
        took < std::time::Duration::from_secs(1),
        "the lease was freed after {took:?}"
    );
}

/// adb's call that takes back the reverse of device port `port` on `serial`.
fn unreverse(serial: &str, port: &str) -> Vec<String> {
    ["-s", serial, "reverse", "--remove", port]
        .map(str::to_owned)
        .to_vec()
}

#[test]
fn a_release_takes_back_the_reverses_its_phone_was_given() {
    let (sandbox, world) = lab();
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    let wt = world.worktree("demo-reverse");
    let held = owned(&sandbox, &wt, &["acquire", "--phone", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    let given = ["-s", "FAKEPHONE01", "reverse", "tcp:8787", "tcp:27787"].map(str::to_owned);
    assert!(
        sandbox.calls("adb").contains(&given.to_vec()),
        "{:?}",
        sandbox.calls("adb")
    );

    let out = owned(&sandbox, &wt, &["release"]);

    assert_eq!(out.code, 0, "{out:?}");
    let calls = sandbox.calls("adb");
    assert!(
        calls.contains(&unreverse("FAKEPHONE01", "tcp:8787")),
        "{calls:?}"
    );
}

#[test]
fn the_lease_of_an_owner_that_exits_takes_back_its_reverses_too() {
    let (sandbox, world) = lab();
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    let wt = world.worktree("demo-reverse");
    let owner = owner_process();
    let held = owned_by(
        &sandbox,
        &wt,
        owner.id(),
        &["acquire", "--phone", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");

    kill_owner(owner);

    let taken_back = poll_until(|| {
        sandbox
            .calls("adb")
            .contains(&unreverse("FAKEPHONE01", "tcp:8787"))
    });
    assert!(taken_back, "{:?}", sandbox.calls("adb"));
    assert!(
        status(&sandbox, &wt)
            .contains("FAKEPHONE01        phone    Pixel 8        ready        free"),
        "{}",
        status(&sandbox, &wt)
    );
}

#[test]
fn an_emulator_idle_for_the_configured_minutes_is_stopped_and_forgotten() {
    let (sandbox, world) = lab();
    // 0.02 minutes: about a second.
    write_machine_config(&sandbox, r#""emulatorIdleMin": 0.02"#);
    let wt = world.worktree("plain-idle");
    let held = owned(&sandbox, &wt, &["acquire", "--emulator", "--quiet"]);
    assert_eq!(held.code, 0, "{held:?}");
    assert!(
        status(&sandbox, &wt).contains("emulator-5580"),
        "{}",
        status(&sandbox, &wt)
    );

    assert_eq!(owned(&sandbox, &wt, &["release"]).code, 0);

    let stopped = poll_until(|| status(&sandbox, &wt).starts_with("devices: none attached\n"));
    assert!(stopped, "{}", status(&sandbox, &wt));
    let kill = ["-s", "emulator-5580", "emu", "kill"]
        .map(str::to_owned)
        .to_vec();
    assert!(
        sandbox.calls("adb").contains(&kill),
        "{:?}",
        sandbox.calls("adb")
    );
}

/// The fake supabase's calls that stopped the stack of `project_id`.
fn stack_stops(sandbox: &Sandbox, project_id: &str) -> usize {
    sandbox
        .calls("supabase")
        .iter()
        .filter(|c| c.first().is_some_and(|a| a == "stop") && c.iter().any(|a| a == project_id))
        .count()
}

#[test]
fn a_stack_devlab_started_is_stopped_once_idle_and_one_started_outside_devlab_never_is() {
    let (sandbox, world) = lab();
    // 0.02 minutes: about a second.
    write_machine_config(&sandbox, r#""stackIdleMin": 0.02"#);
    let ours = world.worktree("demo-ours");
    let held = owned(
        &sandbox,
        &ours,
        &["acquire", "--no-device", "--stack", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");
    // Slot 1's stack was started outside devlab; a lease that doesn't start it uses it.
    let containers = sandbox.path("world/containers");
    let mut listed = fs::read_to_string(&containers).unwrap_or_default();
    listed.push_str("supabase_db_demo_s1\n");
    fs::write(&containers, listed).expect("start a stack outside devlab");
    let other = owner_process();
    let outside = world.worktree("demo-outside");
    let held = owned_by(
        &sandbox,
        &outside,
        other.id(),
        &["acquire", "--no-device", "--quiet"],
    );
    assert!(held.stdout.contains("export DEVLAB_SLOT=1\n"), "{held:?}");

    // The outside stack idles first, so a look that stops ours had it due too.
    assert_eq!(
        owned_by(&sandbox, &outside, other.id(), &["release"]).code,
        0
    );
    assert_eq!(owned(&sandbox, &ours, &["release"]).code, 0);

    // The fake records the stop before it takes the container down.
    let stopped = poll_until(|| {
        stack_stops(&sandbox, "demo") == 1
            && !fs::read_to_string(&containers)
                .unwrap_or_default()
                .lines()
                .any(|c| c == "supabase_db_demo")
    });
    kill_owner(other);
    assert!(stopped, "{:?}", sandbox.calls("supabase"));
    let up = fs::read_to_string(&containers).unwrap_or_default();
    assert!(up.lines().any(|c| c == "supabase_db_demo_s1"), "{up}");
    assert_eq!(
        stack_stops(&sandbox, "demo_s1"),
        0,
        "{:?}",
        sandbox.calls("supabase")
    );
}

#[test]
fn status_names_the_daemon_as_the_watcher_and_devlab_watch_starts_no_poller() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-watch");
    let text = status(&sandbox, &wt);
    assert!(
        text.lines()
            .last()
            .is_some_and(|l| l.ends_with("; watcher active")),
        "{text}"
    );
    let json = owned(&sandbox, &wt, &["status", "--json"]);
    let json: serde_json::Value =
        serde_json::from_str(&json.stdout).unwrap_or_else(|e| panic!("{e}: {json:?}"));
    assert_eq!(json["watcher"], "active", "{json}");

    let watch = owned(&sandbox, &wt, &["watch"]);
    assert_eq!(
        (watch.code, watch.stdout.as_str()),
        (
            0,
            "devlab watch: the factory daemon frees leases and stops idle emulators and stacks now; nothing to start\n"
        ),
        "{watch:?}"
    );
}

/// Whether the daemon holds a wait on lease `id`, as `factory status --json` lists its waits.
fn lease_waited_on(sandbox: &Sandbox, id: &str) -> bool {
    let out = sandbox.factory(&["status", "--json"]);
    let status: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_default();
    status["waits"]
        .as_array()
        .is_some_and(|waits| waits.iter().any(|w| w["kind"] == "lease" && w["on"] == id))
}

#[test]
fn factory_wait_lease_returns_once_the_lease_is_released() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-wait-lease");
    let me = std::process::id();
    let acquired = native(&sandbox, &wt, me, &["lab", "acquire", "--no-device"]);
    assert!(one_line(&acquired).starts_with("lease 1,"), "{acquired:?}");
    let waiter = spawn(sandbox.factory_command(&["wait", "lease", "1"]));

    assert!(
        poll_until(|| lease_waited_on(&sandbox, "1")),
        "the daemon never held the wait"
    );
    let released = native(&sandbox, &wt, me, &["lab", "release"]);
    assert_eq!(released.code, 0, "{released:?}");
    let out = finish(waiter);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "lease 1 ended\n", ""),
        "{out:?}"
    );
    // An ended lease is answered at once, in JSON too.
    let again = one_json(&sandbox.factory(&["wait", "lease", "1", "--json"]));
    assert_eq!(again, serde_json::json!({"lease": 1, "held": false}));
    assert!(!lease_waited_on(&sandbox, "1"));
}

#[test]
fn factory_wait_lease_carries_on_across_a_daemon_restart_and_hears_what_happened_meanwhile() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-wait-restart");
    let mut owner = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn the owner");
    let acquired = native(
        &sandbox,
        &wt,
        owner.id(),
        &["lab", "acquire", "--no-device"],
    );
    assert!(one_line(&acquired).starts_with("lease 1,"), "{acquired:?}");
    let waiter = spawn(sandbox.factory_command(&["wait", "lease", "1"]));
    assert!(
        poll_until(|| lease_waited_on(&sandbox, "1")),
        "the daemon never held the wait"
    );

    // The CLI sleeps through the restart; the owner exits while no daemon runs.
    freeze(&waiter);
    stop_daemon(&sandbox);
    owner.kill().expect("kill the owner");
    owner.wait().expect("reap the owner");
    thaw(&waiter);
    let out = finish(waiter);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "lease 1 ended\n", ""),
        "{out:?}"
    );
}

#[test]
fn factory_wait_lease_returns_once_the_owner_exits() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-wait-owner");
    let mut owner = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn the owner");
    let acquired = native(
        &sandbox,
        &wt,
        owner.id(),
        &["lab", "acquire", "--no-device"],
    );
    assert!(one_line(&acquired).starts_with("lease 1,"), "{acquired:?}");
    let waiter = spawn(sandbox.factory_command(&["wait", "lease", "1"]));
    assert!(
        poll_until(|| lease_waited_on(&sandbox, "1")),
        "the daemon never held the wait"
    );

    owner.kill().expect("kill the owner");
    owner.wait().expect("reap the owner");
    let out = finish(waiter);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (0, "lease 1 ended\n", ""),
        "{out:?}"
    );
}

#[test]
fn factory_wait_lease_refuses_an_id_never_granted_and_times_out_on_a_held_one() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-wait-refused");
    let me = std::process::id();
    let acquired = native(&sandbox, &wt, me, &["lab", "acquire", "--no-device"]);
    assert_eq!(acquired.code, 0, "{acquired:?}");

    let never = sandbox.factory(&["wait", "lease", "7"]);
    assert_eq!(
        (never.code, never.stdout.as_str(), never.stderr.as_str()),
        (
            4,
            "",
            "factory: no lease 7 was ever granted; see factory lab status\n"
        ),
        "{never:?}"
    );
    let held = sandbox.factory(&["wait", "lease", "1", "--timeout", "1s"]);
    assert_eq!(
        (held.code, held.stdout.as_str(), held.stderr.as_str()),
        (
            124,
            "",
            "factory: lease 1 still held after 1s; wait again\n"
        ),
        "{held:?}"
    );
    let bad = sandbox.factory(&["wait", "lease", "one"]);
    assert_eq!(bad.code, 2, "{bad:?}");
}

/// The machine config that lets one owner hold both slots of the demo project.
fn let_one_owner_hold_both_slots(sandbox: &Sandbox) {
    write_machine_config(sandbox, "\"maxSlotsPerOwner\": 2");
}

/// Owner `a` holds lease 1 (a phone, in `plain-one`) and lease 2 (slot 0, in `demo-two`); the
/// worktrees are returned in that order.
fn owner_holding_in_two_worktrees(
    sandbox: &Sandbox,
    world: &World,
    a: &Agent,
) -> (PathBuf, PathBuf) {
    let_one_owner_hold_both_slots(sandbox);
    world
        .apply("plug FAKEPHONE01 device usb:1-1 model:Pixel_8")
        .unwrap();
    let (one, two) = (world.worktree("plain-one"), world.worktree("demo-two"));
    let first = run(for_owner(
        sandbox,
        &one,
        a.pid(),
        &["acquire", "--phone", "--quiet"],
    ));
    assert_eq!(first.code, 0, "{first:?}");
    let second = run(for_owner(
        sandbox,
        &two,
        a.pid(),
        &["acquire", "--no-device", "--quiet"],
    ));
    assert_eq!(second.code, 0, "{second:?}");
    (one, two)
}

#[test]
fn a_release_from_the_second_worktree_names_what_it_released_and_what_is_still_held() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let (one, two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);

    let released = run(for_owner(&sandbox, &two, a.pid(), &["release"]));

    assert_eq!(
        (released.code, released.stdout.as_str(), released.stderr.as_str()),
        (
            0,
            "",
            format!(
                "devlab: released lease 2 ({})\n\
                 devlab: still held: lease 1, {}, phone FAKEPHONE01 (Pixel 8): end it with devlab release --lease 1\n",
                two.display(),
                one.display()
            )
            .as_str()
        ),
        "{released:?}"
    );
}

#[test]
fn a_release_where_nothing_is_held_says_so_and_lists_the_callers_other_leases() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let (one, two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let third = world.worktree("plain-three");

    let released = run(for_owner(&sandbox, &third, a.pid(), &["release"]));

    assert_eq!(released.code, 0, "{released:?}");
    let lines: Vec<&str> = released.stderr.lines().collect();
    assert_eq!(lines.len(), 3, "{released:?}");
    assert_eq!(lines[0], "devlab: nothing held here");
    assert!(
        lines[1].starts_with(&format!("devlab: still held: lease 1, {}", one.display()))
            && lines[1].ends_with("end it with devlab release --lease 1"),
        "{released:?}"
    );
    assert!(
        lines[2].starts_with(&format!(
            "devlab: still held: lease 2, {}, slot 0",
            two.display()
        )) && lines[2].ends_with("end it with devlab release --lease 2"),
        "{released:?}"
    );
}

#[test]
fn release_lease_ends_that_lease_from_any_folder() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let (one, two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let elsewhere = world.worktree("plain-elsewhere");

    let released = run(for_owner(
        &sandbox,
        &elsewhere,
        a.pid(),
        &["release", "--lease", "1"],
    ));

    assert_eq!(
        (released.code, released.stderr.as_str()),
        (
            0,
            format!(
                "devlab: released lease 1 ({})\n\
                 devlab: still held: lease 2, {}, slot 0: end it with devlab release --lease 2\n",
                one.display(),
                two.display()
            )
            .as_str()
        ),
        "{released:?}"
    );
    let gone = run(for_owner(&sandbox, &one, a.pid(), &["env"]));
    assert_eq!(gone.code, 4, "{gone:?}");
}

#[test]
fn release_lease_of_another_owners_lease_is_refused_naming_the_holder_and_leaves_it_held() {
    let (sandbox, world) = lab();
    let (a, b) = (Agent::start(), Agent::start());
    let (one, _two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let theirs = world.worktree("plain-b");

    let by_flag = run(for_owner(
        &sandbox,
        &theirs,
        b.pid(),
        &["release", "--lease", "1"],
    ));
    let mut by_env = for_owner(&sandbox, &theirs, b.pid(), &["release"]);
    by_env.env("DEVLAB_LEASE", "1");
    let by_env = run(by_env);

    for refused in [&by_flag, &by_env] {
        assert_eq!(refused.code, 4, "{refused:?}");
        assert_eq!(refused.stdout, "", "{refused:?}");
        assert!(
            refused.stderr.starts_with(&format!(
                "devlab: lease 1 belongs to sleep:{} ({}), not to you: it stays held",
                a.pid(),
                one.display()
            )),
            "{refused:?}"
        );
    }
    let mut stopping = for_owner(&sandbox, &theirs, b.pid(), &["release", "--stop-stack"]);
    stopping.env("DEVLAB_LEASE", "1");
    let stopping = run(stopping);
    assert_eq!(stopping.code, 4, "{stopping:?}");
    let still = run(for_owner(&sandbox, &one, a.pid(), &["env"]));
    assert_eq!(still.code, 0, "{still:?}");
}

#[test]
fn release_refuses_stop_stack_with_a_lease_or_all_mine_and_two_targets_together() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    for args in [
        &["release", "--stop-stack", "--all-mine"][..],
        &["release", "--stop-stack", "--lease", "1"],
        &["release", "--all-mine", "--lease", "1"],
        &["release", "--lease", "x"],
    ] {
        let out = run(for_owner(&sandbox, &wt, std::process::id(), args));
        assert_eq!(out.code, 2, "{args:?}: {out:?}");
    }
}

#[test]
fn release_lease_of_a_lease_that_does_not_exist_exits_4() {
    let (sandbox, world) = lab();
    let wt = world.worktree("plain-a");
    let out = run(for_owner(
        &sandbox,
        &wt,
        std::process::id(),
        &["release", "--lease", "9"],
    ));
    assert_eq!(
        (out.code, out.stderr.as_str()),
        (
            4,
            "devlab: no lease 9: see devlab status for the leases held\n"
        ),
        "{out:?}"
    );
}

#[test]
fn release_all_mine_ends_every_lease_of_the_owner_and_no_one_elses() {
    let (sandbox, world) = lab();
    let (a, b) = (Agent::start(), Agent::start());
    let (one, two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let theirs = world.worktree("demo-b");
    let b_held = run(for_owner(
        &sandbox,
        &theirs,
        b.pid(),
        &["acquire", "--no-device", "--quiet"],
    ));
    assert_eq!(b_held.code, 0, "{b_held:?}");
    let elsewhere = world.worktree("plain-elsewhere");

    let released = run(for_owner(
        &sandbox,
        &elsewhere,
        a.pid(),
        &["release", "--all-mine"],
    ));

    assert_eq!(
        (released.code, released.stderr.as_str()),
        (
            0,
            format!(
                "devlab: released lease 1 ({})\ndevlab: released lease 2 ({})\n",
                one.display(),
                two.display()
            )
            .as_str()
        ),
        "{released:?}"
    );
    for wt in [&one, &two] {
        assert_eq!(run(for_owner(&sandbox, wt, a.pid(), &["env"])).code, 4);
    }
    let shown = status(&sandbox, &elsewhere);
    assert!(
        !shown.contains("lease 1") && !shown.contains("lease 2 "),
        "{shown}"
    );
    assert_eq!(run(for_owner(&sandbox, &theirs, b.pid(), &["env"])).code, 0);
}

#[test]
fn factory_lab_release_says_in_one_line_and_in_json_what_is_still_held() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let (one, two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let at = |wt: &Path, args: &[&str]| native(&sandbox, wt, a.pid(), args);

    let released = at(&two, &["lab", "release", "--json"]);
    let json = one_json(&released);
    assert_eq!(json["released"][0]["id"], 2, "{json}");
    assert_eq!(json["stillHeld"][0]["id"], 1, "{json}");
    assert_eq!(
        json["stillHeld"][0]["worktree"],
        one.to_string_lossy().as_ref(),
        "{json}"
    );

    let line = at(&one, &["lab", "release"]);
    assert_eq!(
        one_line(&line),
        format!("released lease 1 ({})", one.display()),
        "{line:?}"
    );
    let nothing = at(&one, &["lab", "release"]);
    assert_eq!(one_line(&nothing), "nothing held here");
}

#[test]
fn release_by_id_ends_its_lease_whatever_the_devlab_json_of_the_folder_it_runs_in() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let (one, _two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let broken = world.worktree("plain-broken");
    fs::write(broken.join("devlab.json"), "{").expect("break the project config");

    let by_id = run(for_owner(
        &sandbox,
        &broken,
        a.pid(),
        &["release", "--lease", "1"],
    ));
    let all = run(for_owner(
        &sandbox,
        &broken,
        a.pid(),
        &["release", "--all-mine"],
    ));

    assert_eq!(by_id.code, 0, "{by_id:?}");
    assert!(
        by_id
            .stderr
            .starts_with(&format!("devlab: released lease 1 ({})", one.display())),
        "{by_id:?}"
    );
    assert_eq!(all.code, 0, "{all:?}");
}

#[test]
fn factory_lab_release_names_a_file_when_the_leases_it_ends_would_run_the_line_long() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let_one_owner_hold_both_slots(&sandbox);
    for name in ["demo-1".repeat(12), "demo-2".repeat(12)] {
        let wt = world.worktree(&name);
        let held = native(&sandbox, &wt, a.pid(), &["lab", "acquire", "--no-device"]);
        assert_eq!(held.code, 0, "{held:?}");
    }
    let elsewhere = world.worktree("plain-elsewhere");

    let out = native(
        &sandbox,
        &elsewhere,
        a.pid(),
        &["lab", "release", "--all-mine"],
    );

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.chars().count() <= 201, "{out:?}");
    assert!(
        out.stdout.starts_with("released 2 leases, list in "),
        "{out:?}"
    );
    let listed = named_file(out.stdout.trim_end());
    assert!(
        listed.contains("released lease 1 (") && listed.contains("released lease 2 ("),
        "{listed}"
    );
}

#[test]
fn factory_lab_release_json_names_the_file_its_long_line_would_name() {
    let (sandbox, world) = lab();
    let a = Agent::start();
    let_one_owner_hold_both_slots(&sandbox);
    for name in ["demo-1".repeat(12), "demo-2".repeat(12)] {
        let wt = world.worktree(&name);
        let held = native(&sandbox, &wt, a.pid(), &["lab", "acquire", "--no-device"]);
        assert_eq!(held.code, 0, "{held:?}");
    }
    let elsewhere = world.worktree("plain-elsewhere");

    let out = native(
        &sandbox,
        &elsewhere,
        a.pid(),
        &["lab", "release", "--all-mine", "--json"],
    );

    let json = one_json(&out);
    let file = json["releasedFile"]
        .as_str()
        .unwrap_or_else(|| panic!("no releasedFile in {json}"));
    let listed = fs::read_to_string(file).expect("read the release list");
    assert!(
        listed.contains("released lease 1 (") && listed.contains("released lease 2 ("),
        "{listed}"
    );
}

#[test]
fn a_stack_that_fails_to_stop_still_says_what_the_release_freed() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-stop");
    let held = owned(
        &sandbox,
        &wt,
        &["acquire", "--no-device", "--stack", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");
    world.apply("break supabase stop").unwrap();

    let out = owned(&sandbox, &wt, &["release", "--stop-stack"]);

    assert_ne!(out.code, 0, "{out:?}");
    assert!(
        out.stderr
            .starts_with(&format!("devlab: released lease 1 ({})\n", wt.display())),
        "{out:?}"
    );
    assert_eq!(owned(&sandbox, &wt, &["env"]).code, 4);
}

#[test]
fn a_devlab_lease_release_with_stop_stack_from_a_broken_folder_still_ends_or_refuses_it() {
    let (sandbox, world) = lab();
    let (a, b) = (Agent::start(), Agent::start());
    let (one, _two) = owner_holding_in_two_worktrees(&sandbox, &world, &a);
    let broken = world.worktree("plain-broken");
    fs::write(broken.join("devlab.json"), "{").expect("break the project config");
    let by_env = |owner: &Agent| {
        let mut cmd = for_owner(&sandbox, &broken, owner.pid(), &["release", "--stop-stack"]);
        cmd.env("DEVLAB_LEASE", "1");
        run(cmd)
    };

    let refused = by_env(&b);
    let ended = by_env(&a);

    assert_eq!(refused.code, 4, "{refused:?}");
    assert!(refused.stderr.contains("lease 1 belongs to"), "{refused:?}");
    assert_eq!(ended.code, 0, "{ended:?}");
    assert!(
        ended
            .stderr
            .starts_with(&format!("devlab: released lease 1 ({})", one.display())),
        "{ended:?}"
    );
}

#[test]
fn stop_stack_leaves_a_stack_alone_when_the_folder_now_names_another_project() {
    let (sandbox, world) = lab();
    let wt = world.worktree("demo-switched");
    let held = owned(
        &sandbox,
        &wt,
        &["acquire", "--no-device", "--stack", "--quiet"],
    );
    assert_eq!(held.code, 0, "{held:?}");
    // The folder now holds project other, whose stack runs on the same slot.
    let config = fs::read_to_string(wt.join("devlab.json")).expect("read devlab.json");
    fs::write(
        wt.join("devlab.json"),
        config.replace(r#""name": "demo""#, r#""name": "other""#),
    )
    .expect("switch the project");
    fs::write(wt.join("supabase/config.toml"), "project_id = \"other\"\n")
        .expect("switch the stack");
    let containers = sandbox.path("world/containers");
    let mut listed = fs::read_to_string(&containers).unwrap_or_default();
    listed.push_str("supabase_db_other\n");
    fs::write(&containers, listed).expect("run project other's stack");

    let out = owned(&sandbox, &wt, &["release", "--stop-stack"]);

    assert_eq!(stack_stops(&sandbox, "other"), 0, "{out:?}");
    assert_ne!(out.code, 0, "{out:?}");
    assert!(
        out.stderr
            .starts_with(&format!("devlab: released lease 1 ({})\n", wt.display())),
        "{out:?}"
    );
    assert!(out.stderr.contains("other"), "{out:?}");
}
