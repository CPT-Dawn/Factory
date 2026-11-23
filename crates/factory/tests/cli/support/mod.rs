//! The sandbox every binary-seam test runs in.
//!
//! A [`Sandbox`] is a temp directory holding the state, runtime, config and home directories
//! the binary sees, and a `bin` directory first on its PATH. Every tool in [`FAKED_TOOLS`] gets
//! a fake there that records each call and refuses with exit 127 until a test scripts it with
//! [`Sandbox::fake`], so a test can never reach the real adb, docker, gh or reviewer.
//!
//! The binary runs with a cleared environment: only the variables set here, plus those a test
//! adds with [`Sandbox::env`], reach it. A daemon the binary starts is stopped when the sandbox
//! drops.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use tempfile::TempDir;

pub mod github;
pub mod golden;
pub mod herdr;
pub mod world;

/// External tools faked on every sandbox's PATH. Add a tool here when a command starts using it.
pub const FAKED_TOOLS: &[&str] = &[
    "adb",
    "docker",
    "systemctl",
    "gh",
    "herdr",
    "agy",
    "codex",
    "claude",
    "systemd-run",
    "avdmanager",
    "emulator",
    "notify-send",
];

/// How long any one process a test runs may take before the test fails.
pub const DEADLINE: Duration = Duration::from_secs(30);

const FACTORY: &str = env!("CARGO_BIN_EXE_factory");
/// Separates the arguments of one recorded call, and one call from the next.
const ARG_END: char = '\u{1f}';
const CALL_END: char = '\u{1e}';

pub struct Sandbox {
    dir: TempDir,
    env: Vec<(String, OsString)>,
}

/// What a finished process showed: its exit code, stdout and stderr.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let dir = tempfile::Builder::new()
            .prefix("factory-test-")
            .tempdir()
            .expect("create the sandbox dir");
        let sandbox = Sandbox {
            dir,
            env: Vec::new(),
        };
        for sub in ["home", "state", "config", "bin", "calls"] {
            fs::create_dir(sandbox.path(sub)).expect("create a sandbox subdir");
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(sandbox.path("run"))
            .expect("create the runtime dir");
        for tool in FAKED_TOOLS {
            sandbox.fake(
                tool,
                &format!("echo \"fake {tool}: no script for: $*\" >&2\nexit 127"),
            );
        }
        sandbox
    }

    /// A path inside the sandbox.
    pub fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Adds a variable to the environment of every process the sandbox runs.
    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Sandbox {
        self.env.push((key.to_owned(), value.as_ref().to_owned()));
        self
    }

    /// The event ledger factory-stats reads, under the sandbox's default state dir. Tests read it
    /// directly because it is a public output format, read by another tool, not daemon state; no
    /// factory command reads it back.
    pub fn ledger(&self) -> PathBuf {
        self.path("state/factory/events.jsonl")
    }

    /// The ledger's lines; none when it doesn't exist yet.
    pub fn ledger_lines(&self) -> Vec<String> {
        lines_of(&self.ledger())
    }

    /// Scripts the fake `tool`: `body` is shell that runs after the call is recorded, with the
    /// call's arguments in `$@`.
    pub fn fake(&self, tool: &str, body: &str) {
        let calls = self.path("calls").join(tool);
        let script = format!(
            "#!/bin/sh\n# Fake {tool}, written by the factory test harness.\n\
             {{ for a in \"$@\"; do printf '%s\\037' \"$a\"; done; printf '\\036'; }} >> '{calls}'\n\
             {body}\n",
            calls = calls.display()
        );
        let path = self.path("bin").join(tool);
        fs::write(&path, script).expect("write a fake tool");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("make a fake executable");
    }

    /// Every call the fake `tool` received, oldest first, as its arguments.
    pub fn calls(&self, tool: &str) -> Vec<Vec<String>> {
        let recorded = fs::read_to_string(self.path("calls").join(tool)).unwrap_or_default();
        recorded
            .split_terminator(CALL_END)
            .map(|call| call.split_terminator(ARG_END).map(str::to_owned).collect())
            .collect()
    }

    /// A command for `program` with the sandbox's environment and nothing else.
    pub fn command(&self, program: impl AsRef<OsStr>, args: &[&str]) -> Command {
        let mut cmd = Command::new(program);
        let path = format!("{}:/usr/bin:/bin", self.path("bin").display());
        cmd.args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(self.dir.path())
            .stdin(Stdio::null());
        cmd
    }

    /// A command for the built `factory` binary.
    pub fn factory_command(&self, args: &[&str]) -> Command {
        self.command(FACTORY, args)
    }

    /// Runs the built `factory` binary to completion.
    pub fn factory(&self, args: &[&str]) -> Run {
        run(self.factory_command(args))
    }

    /// Runs the built `factory` binary to completion with `dir` as its working directory.
    pub fn factory_in(&self, dir: &Path, args: &[&str]) -> Run {
        let mut cmd = self.factory_command(args);
        cmd.current_dir(dir);
        run(cmd)
    }

    /// A git repository named `repo` in the sandbox: `main` holds one commit, and the
    /// checked-out branch `work` adds `a.txt` on top of it in one more.
    pub fn repo(&self) -> PathBuf {
        let repo = self.path("repo");
        fs::create_dir(&repo).expect("create the repo dir");
        self.git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("README.md"), "# repo\n").expect("write README.md");
        self.git(&repo, &["add", "."]);
        self.git(&repo, &["commit", "-q", "-m", "chore: start"]);
        self.git(&repo, &["checkout", "-q", "-b", "work"]);
        fs::write(repo.join("a.txt"), "a\n").expect("write a.txt");
        self.git(&repo, &["add", "."]);
        self.git(&repo, &["commit", "-q", "-m", "feat: add a"]);
        repo
    }

    /// Runs git in `dir` as the sandbox's committer, failing the test when it fails.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut cmd = self.command(
            "git",
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ],
        );
        cmd.args(args).current_dir(dir);
        let out = run(cmd);
        assert_eq!(out.code, 0, "git {args:?}: {out:?}");
        out.stdout
    }

    /// Where `factory review` keeps its files for the sandbox's [`Sandbox::repo`].
    pub fn reviews(&self) -> PathBuf {
        self.path("state/factory/reviews/repo")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // The daemon leads its own process group; stop it and anything it started.
        if let Ok(pid) = fs::read_to_string(self.path("run/factory/daemon.pid")) {
            kill_group(pid.trim().parse().unwrap_or(0), Signal::TERM);
        }
    }
}

impl Run {
    pub fn stdout_lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }

    pub fn stderr_lines(&self) -> Vec<&str> {
        self.stderr.lines().collect()
    }
}

/// Runs `cmd` to completion, failing the test when it outlives [`DEADLINE`].
pub fn run(cmd: Command) -> Run {
    finish(spawn(cmd))
}

/// Starts `cmd` in its own process group with its stdout and stderr captured, for tests that
/// run several at once.
pub fn spawn(mut cmd: Command) -> Child {
    cmd.process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the command")
}

/// Waits for a child [`spawn`] started, failing the test and killing its process group when it
/// outlives [`DEADLINE`].
pub fn finish(child: Child) -> Run {
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(child.wait_with_output()));
    let output: Output = match rx.recv_timeout(DEADLINE) {
        Ok(output) => output.expect("wait for the command"),
        Err(_) => {
            kill_group(pid, Signal::KILL);
            panic!("process {pid} outlived {DEADLINE:?}");
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

pub(crate) fn kill_group(leader: u32, signal: Signal) {
    if let Some(group) = i32::try_from(leader).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(group, signal);
    }
}

/// The lines of a file; none when it doesn't exist.
pub fn lines_of(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
