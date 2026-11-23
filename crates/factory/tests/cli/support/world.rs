//! The fake world devlab's golden transcripts run in.
//!
//! A [`World`] scripts the fakes of adb, docker, systemctl, systemd-run, avdmanager, git and
//! the project's supabase CLI (`tests/golden/devlab/fakes`) to play back the state held in its
//! `world` directory: the devices adb lists, the containers docker runs, the systemd units that
//! are active and the operations that are broken. Steps like `plug` and `break` change that
//! state between two devlab calls.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{DEADLINE, Sandbox, kill_group};
use rustix::process::Signal;

/// Where the fixtures live: the transcripts, the fakes, the machine config and the project.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/devlab")
}

/// The tools a world fakes, each scripted by `fakes/<tool>.sh`.
const WORLD_TOOLS: &[&str] = &[
    "adb",
    "docker",
    "systemctl",
    "systemd-run",
    "avdmanager",
    "supabase",
    "git",
    "notify-send",
];

/// The files that hold a world's state, one entry per line.
const STATE_FILES: &[&str] = &["devices", "containers", "units", "broken", "locked"];

/// How often a wait on a file looks again.
const POLL: Duration = Duration::from_millis(20);

pub struct World {
    dir: PathBuf,
    worktrees: PathBuf,
}

impl World {
    /// Installs the world's fakes in `sandbox`, with no device, container or unit, the
    /// fixtures' machine config, and an empty directory for worktrees.
    pub fn install(sandbox: &Sandbox) -> World {
        let world = World {
            dir: sandbox.path("world"),
            worktrees: sandbox.path("wt"),
        };
        fs::create_dir_all(&world.dir).expect("create the world dir");
        fs::create_dir_all(&world.worktrees).expect("create the worktrees dir");
        for file in STATE_FILES {
            fs::write(world.dir.join(file), "").expect("create a world file");
        }
        let fakes = fixtures().join("fakes");
        let at = world.dir.display().to_string();
        for tool in WORLD_TOOLS {
            let body = fs::read_to_string(fakes.join(format!("{tool}.sh"))).expect("read a fake");
            sandbox.fake(tool, &body.replace("@WORLD@", &at));
        }
        let config = sandbox.path("config/devlab");
        fs::create_dir_all(&config).expect("create the devlab config dir");
        fs::copy(fixtures().join("machine.json"), config.join("config.json"))
            .expect("copy the machine config");
        world
    }

    /// Applies one world step:
    /// - `plug <serial> <state> [<prop>...]`: adb lists the device (replacing its old line),
    /// - `unplug <serial>`: adb no longer lists it,
    /// - `lock <serial>`: its lock screen stays up,
    /// - `break <operation>` / `mend <operation>`: `supabase start` or `systemd-run` fails,
    ///   or works again.
    pub fn apply(&self, step: &str) -> Result<(), String> {
        let words: Vec<&str> = step.split_whitespace().collect();
        match words.as_slice() {
            ["plug", serial, _state, ..] => {
                self.remove("devices", |line| first_word(line) == *serial);
                self.append("devices", &words[1..].join(" "));
            }
            ["unplug", serial] => self.remove("devices", |line| first_word(line) == *serial),
            ["lock", serial] => self.append("locked", serial),
            ["break", what @ ..] if !what.is_empty() => self.append("broken", &what.join(" ")),
            ["mend", what @ ..] if !what.is_empty() => {
                let what = what.join(" ");
                self.remove("broken", |line| line == what);
            }
            _ => return Err(format!("unknown world step: {step}")),
        }
        Ok(())
    }

    /// A worktree named `name` under the sandbox, made on first use. One whose name starts
    /// with `demo-` holds the fixtures' project: a devlab.json with a supabase stack.
    pub fn worktree(&self, name: &str) -> PathBuf {
        let dir = self.worktrees.join(name);
        if !dir.exists() {
            fs::create_dir_all(&dir).expect("create a worktree");
            if name.starts_with("demo-") {
                copy_tree(&fixtures().join("project"), &dir);
            }
        }
        dir
    }

    /// Starts `adb track-devices` on the world, for tests of the stream itself.
    pub fn track_devices(&self, sandbox: &Sandbox) -> Tracking {
        let out = self.dir.join("track-devices.out");
        let mut cmd = sandbox.command("adb", &["track-devices"]);
        cmd.stdout(fs::File::create(&out).expect("create the stream file"));
        Tracking {
            child: spawn_quiet(cmd),
            out,
        }
    }

    fn append(&self, file: &str, line: &str) {
        let path = self.dir.join(file);
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        text.push_str(line);
        text.push('\n');
        fs::write(path, text).expect("write a world file");
    }

    fn remove(&self, file: &str, gone: impl Fn(&str) -> bool) {
        let path = self.dir.join(file);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let kept: String = text
            .lines()
            .filter(|line| !gone(line))
            .map(|line| format!("{line}\n"))
            .collect();
        fs::write(path, kept).expect("write a world file");
    }
}

/// A running `adb track-devices`, stopped when dropped.
pub struct Tracking {
    child: Child,
    out: PathBuf,
}

impl Tracking {
    /// Waits until the stream has shown exactly `frames`, failing the test after [`DEADLINE`].
    pub fn wait_for(&self, frames: &str) {
        let shown = wait_until(&self.out, |text| text == frames);
        assert_eq!(shown, frames, "adb track-devices");
    }
}

impl Drop for Tracking {
    fn drop(&mut self) {
        kill_group(self.child.id(), Signal::KILL);
        let _ = self.child.wait();
    }
}

/// Reads `path` until `done` accepts its contents or [`DEADLINE`] passes, and returns what
/// it read last.
pub fn wait_until(path: &Path, done: impl Fn(&str) -> bool) -> String {
    let start = Instant::now();
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if done(&text) || start.elapsed() > DEADLINE {
            return text;
        }
        thread::sleep(POLL);
    }
}

/// Spawns `cmd` in its own process group with whatever stdout and stderr it was given.
fn spawn_quiet(mut cmd: std::process::Command) -> Child {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0).stderr(Stdio::null());
    cmd.spawn().expect("spawn the command")
}

fn first_word(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("read a fixture dir") {
        let entry = entry.expect("read a fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("stat a fixture entry").is_dir() {
            fs::create_dir_all(&target).expect("create a dir");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy a fixture file");
        }
    }
}
