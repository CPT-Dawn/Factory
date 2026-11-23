//! Golden transcripts of devlab: scenarios recorded from today's TypeScript devlab, replayed
//! against any devlab binary, byte for byte.
//!
//! A transcript (`tests/golden/devlab/*.golden`) is a scenario and what each of its devlab
//! calls showed. One step per line:
//!
//! - `# ...` and blank lines are notes.
//! - `$ <owner>@<worktree> <shell>` runs `<shell>` with `sh -c` in worktree `<worktree>`
//!   (see [`World::worktree`]) on behalf of owner `<owner>`: a live process named `claude` that
//!   the call names in `DEVLAB_OWNER_PID`. `devlab` on its PATH is the binary under test.
//! - `& <tag> <owner>@<worktree> <shell>` starts such a call in the background;
//!   `until <tag> <text>` waits until its output shows `<text>`; `wait <tag>` waits for it to end;
//!   `signal <tag> <TERM|INT|HUP|KILL>` sends its process (the shell, or what it `exec`s) a signal.
//! - `kill <owner>` ends the owner's process.
//! - Anything else is a world step ([`World::apply`]): `plug`, `unplug`, `lock`, `break`, `mend`.
//!
//! After a `$` or `wait` step come the lines it showed: `1> ` for each stdout line, `2> ` for
//! each stderr line, `1~ ` or `2~ ` for a last line with no newline, and `? <exit code>`.
//! The sandbox's path shows as `<sandbox>`, owner `a`'s pid as `claude:<a>`, free memory,
//! which no fake controls, as `<mem>`, and the seconds a run lasted before a signal ended it
//! (`killed by SIGKILL after <n> s`) as `<n>`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use rustix::process::{Pid, Signal, kill_process};

pub use super::world::fixtures;
use super::world::{World, poll_until};
use super::{DEADLINE, Sandbox, kill_group};

/// The devlab transcript `file` replays against: the path in `GOLDEN_DEVLAB`, else the built
/// `factory`, which answers to the name `devlab`, for the groups it serves (`slots-*`, `cli-*`);
/// none skips it. Today's devlab is `GOLDEN_DEVLAB=$HOME/.local/bin/devlab`; see
/// `tests/golden/devlab/README.md`. The binary is linked as `devlab` in the sandbox, so a
/// multicall binary sees that name.
pub fn devlab_under_test(file: &str) -> Option<PathBuf> {
    match std::env::var_os("GOLDEN_DEVLAB") {
        Some(path) => Some(resolve_devlab(Path::new(&path))),
        None => SERVED
            .iter()
            .any(|group| file.starts_with(group))
            .then(|| PathBuf::from(env!("CARGO_BIN_EXE_factory"))),
    }
}

/// The transcript groups the built `factory` replays.
const SERVED: [&str; 3] = ["slots-", "devices-", "cli-"];

/// `path` as given, or read from the workspace root when relative: `cargo test` runs in the
/// crate's directory, but `GOLDEN_DEVLAB=target/debug/factory` is written from the root.
pub fn resolve_devlab(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// `GOLDEN_RECORD=1` rewrites each transcript with what the devlab under test showed.
pub fn recording() -> bool {
    std::env::var_os("GOLDEN_RECORD").is_some_and(|v| v == "1")
}

/// Replays the transcript at `fixture` against `devlab`. Without `record`, returns every
/// call whose stdout, stderr or exit code differs, naming the first differing byte; with it,
/// rewrites the transcript with what each call showed.
pub fn replay(fixture: &Path, devlab: &Path, record: bool) -> Result<(), String> {
    let name = fixture
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let text = fs::read_to_string(fixture).map_err(|e| format!("{name}: {e}"))?;
    let steps = parse(&text).map_err(|(line, e)| format!("{name}:{line}: {e}"))?;
    let mut stage = Stage::new(devlab).map_err(|e| format!("{name}: {e}"))?;
    let mut report = String::new();
    let mut recorded = String::new();
    for step in &steps {
        let shown = stage
            .play(&step.text)
            .map_err(|e| format!("{name}:{}: {e}", step.line))?;
        recorded.push_str(&step.text);
        recorded.push('\n');
        let Some(shown) = shown else { continue };
        let shown = stage.normalise(shown);
        recorded.push_str(&shown.lines().join(""));
        if !record {
            report.push_str(&compare(
                &format!("{name}:{} `{}`", step.line, step.text),
                step.shown.as_ref(),
                &shown,
            ));
        }
    }
    if record {
        fs::write(fixture, recorded).map_err(|e| format!("{name}: {e}"))?;
    }
    if report.is_empty() {
        Ok(())
    } else {
        Err(report)
    }
}

/// What one call showed.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Shown {
    /// The transcript lines for this output, each ending in a newline.
    fn lines(&self) -> Vec<String> {
        let mut out = stream_lines('1', &self.stdout);
        out.extend(stream_lines('2', &self.stderr));
        out.push(format!("? {}\n", self.code));
        out
    }
}

fn stream_lines(fd: char, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(end) = rest.find('\n') {
        out.push(prefixed(fd, '>', &rest[..end]));
        rest = &rest[end + 1..];
    }
    if !rest.is_empty() {
        out.push(prefixed(fd, '~', rest));
    }
    out
}

fn prefixed(fd: char, mark: char, line: &str) -> String {
    if line.is_empty() {
        format!("{fd}{mark}\n")
    } else {
        format!("{fd}{mark} {line}\n")
    }
}

/// One step of a transcript, with the output recorded after it.
struct Step {
    line: usize,
    text: String,
    shown: Option<Shown>,
}

fn parse(text: &str) -> Result<Vec<Step>, (usize, String)> {
    let mut steps: Vec<Step> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if let Some(part) = output_part(line) {
            let step = steps
                .last_mut()
                .filter(|s| shows(&s.text))
                .ok_or((n, "output lines must follow a $ or wait step".to_owned()))?;
            let shown = step.shown.get_or_insert(Shown {
                stdout: String::new(),
                stderr: String::new(),
                code: -1,
            });
            match part {
                Part::Out(s) => shown.stdout.push_str(&s),
                Part::Err(s) => shown.stderr.push_str(&s),
                Part::Code(code) => {
                    shown.code = code.ok_or((n, format!("bad exit code: {line}")))?
                }
            }
        } else {
            steps.push(Step {
                line: n,
                text: line.to_owned(),
                shown: None,
            });
        }
    }
    Ok(steps)
}

enum Part {
    Out(String),
    Err(String),
    Code(Option<i32>),
}

fn output_part(line: &str) -> Option<Part> {
    if let Some(code) = line.strip_prefix("? ") {
        return Some(Part::Code(code.parse().ok()));
    }
    let (head, body) = line.split_at_checked(2)?;
    let body = match body.strip_prefix(' ') {
        Some(text) => text,
        None if body.is_empty() => "",
        None => return None,
    };
    let (fd, mark) = {
        let mut chars = head.chars();
        (chars.next()?, chars.next()?)
    };
    let text = match mark {
        '>' => format!("{body}\n"),
        '~' => body.to_owned(),
        _ => return None,
    };
    match fd {
        '1' => Some(Part::Out(text)),
        '2' => Some(Part::Err(text)),
        _ => None,
    }
}

/// Whether a step shows output: a call run now, or the wait for a background one.
fn shows(step: &str) -> bool {
    step.starts_with("$ ") || step.starts_with("wait ")
}

fn compare(what: &str, expected: Option<&Shown>, actual: &Shown) -> String {
    let Some(expected) = expected else {
        return format!("{what}: nothing recorded; record it with GOLDEN_RECORD=1\n");
    };
    let mut out = String::new();
    for (stream, fd, want, got) in [
        ("stdout", '1', &expected.stdout, &actual.stdout),
        ("stderr", '2', &expected.stderr, &actual.stderr),
    ] {
        if want != got {
            let at = want
                .bytes()
                .zip(got.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            out.push_str(&format!(
                "{what}: {stream} differs at byte {at}\n  expected:\n"
            ));
            for line in stream_lines(fd, want) {
                out.push_str(&format!("    {line}"));
            }
            out.push_str("  actual:\n");
            for line in stream_lines(fd, got) {
                out.push_str(&format!("    {line}"));
            }
        }
    }
    if expected.code != actual.code {
        out.push_str(&format!(
            "{what}: exit {}, expected {}\n",
            actual.code, expected.code
        ));
    }
    out
}

/// The sandbox a transcript plays in: its world, its owners and its background calls.
struct Stage {
    // Background calls and owners end before the sandbox they run in is removed.
    background: BTreeMap<String, Background>,
    owners: BTreeMap<String, Owner>,
    world: World,
    sandbox: Sandbox,
}

struct Owner {
    pid: u32,
    child: Option<Child>,
}

struct Background {
    child: Option<Child>,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Stage {
    fn new(devlab: &Path) -> Result<Stage, String> {
        let devlab = fs::canonicalize(devlab)
            .ok()
            .filter(|path| path.is_file())
            .ok_or_else(|| format!("no devlab at {}", devlab.display()))?;
        let sandbox = Sandbox::new();
        let world = World::install(&sandbox);
        let bin = sandbox.path("bin");
        std::os::unix::fs::symlink(devlab, bin.join("devlab")).expect("link the devlab under test");
        // Today's devlab is a bun script; its launcher finds bun on PATH.
        if let Some(bun) = on_path("bun") {
            std::os::unix::fs::symlink(bun, bin.join("bun")).expect("link bun");
        }
        for dir in ["owners", "background"] {
            fs::create_dir(sandbox.path(dir)).expect("create a stage dir");
        }
        std::os::unix::fs::symlink(sleep_binary(), sandbox.path("owners/claude"))
            .expect("link the owner binary");
        Ok(Stage {
            background: BTreeMap::new(),
            owners: BTreeMap::new(),
            world,
            sandbox,
        })
    }

    /// Plays one step; returns what it showed, if it is a step that shows output.
    fn play(&mut self, step: &str) -> Result<Option<Shown>, String> {
        if step.is_empty() || step.starts_with('#') {
            return Ok(None);
        }
        if let Some(call) = step.strip_prefix("$ ") {
            let cmd = self.call(call)?;
            let out = super::run(cmd);
            return Ok(Some(Shown {
                stdout: out.stdout,
                stderr: out.stderr,
                code: out.code,
            }));
        }
        if let Some(rest) = step.strip_prefix("& ") {
            let (tag, call) = rest.split_once(' ').ok_or("& needs a tag and a call")?;
            if self.background.get(tag).is_some_and(|b| b.child.is_some()) {
                return Err(format!("background call {tag} is still running"));
            }
            let stdout = self.sandbox.path(format!("background/{tag}.out"));
            let stderr = self.sandbox.path(format!("background/{tag}.err"));
            let mut cmd = self.call(call)?;
            cmd.stdout(fs::File::create(&stdout).map_err(|e| e.to_string())?)
                .stderr(fs::File::create(&stderr).map_err(|e| e.to_string())?)
                .process_group(0);
            let child = cmd.spawn().map_err(|e| e.to_string())?;
            self.background.insert(
                tag.to_owned(),
                Background {
                    child: Some(child),
                    stdout,
                    stderr,
                },
            );
            return Ok(None);
        }
        if let Some(rest) = step.strip_prefix("until ") {
            let (tag, text) = rest.split_once(' ').ok_or("until needs a tag and a text")?;
            let call = self
                .background
                .get(tag)
                .ok_or(format!("no background call {tag}"))?;
            let shows = |path: &Path| fs::read_to_string(path).unwrap_or_default().contains(text);
            if !poll_until(|| shows(&call.stdout) || shows(&call.stderr)) {
                return Err(format!("{tag} did not show {text:?} within {DEADLINE:?}"));
            }
            return Ok(None);
        }
        if let Some(tag) = step.strip_prefix("wait ") {
            let call = self
                .background
                .get_mut(tag)
                .ok_or(format!("no background call {tag}"))?;
            let mut child = call
                .child
                .take()
                .ok_or(format!("{tag} was already waited for"))?;
            let code = wait_child(&mut child).ok_or(format!("{tag} outlived {DEADLINE:?}"))?;
            let read =
                |p: &Path| String::from_utf8_lossy(&fs::read(p).unwrap_or_default()).into_owned();
            return Ok(Some(Shown {
                stdout: read(&call.stdout),
                stderr: read(&call.stderr),
                code,
            }));
        }
        if let Some(rest) = step.strip_prefix("signal ") {
            let (tag, name) = rest
                .split_once(' ')
                .ok_or("signal needs a tag and a signal")?;
            let signal = match name {
                "TERM" => Signal::TERM,
                "INT" => Signal::INT,
                "HUP" => Signal::HUP,
                "KILL" => Signal::KILL,
                other => return Err(format!("signal knows TERM, INT, HUP and KILL, not {other}")),
            };
            let pid = self
                .background
                .get(tag)
                .and_then(|call| call.child.as_ref())
                .and_then(|child| i32::try_from(child.id()).ok())
                .and_then(Pid::from_raw)
                .ok_or(format!("no background call {tag} is running"))?;
            kill_process(pid, signal).map_err(|e| format!("signal {tag}: {e}"))?;
            return Ok(None);
        }
        if let Some(name) = step.strip_prefix("kill ") {
            let owner = self.owner(name)?;
            if let Some(mut child) = owner.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Ok(None);
        }
        self.world.apply(step).map(|()| None)
    }

    /// The command for `<owner>@<worktree> <shell>`.
    fn call(&mut self, call: &str) -> Result<Command, String> {
        let (who, shell) = call
            .split_once(' ')
            .ok_or("a call needs <owner>@<worktree> and a command")?;
        let (owner, worktree) = who
            .split_once('@')
            .ok_or("a call needs <owner>@<worktree>")?;
        let pid = self.owner(owner)?.pid;
        let mut cmd = self.sandbox.command("sh", &["-c", shell]);
        cmd.current_dir(self.world.worktree(worktree))
            .env("DEVLAB_OWNER_PID", pid.to_string());
        Ok(cmd)
    }

    /// Owner `name`, started on first use.
    fn owner(&mut self, name: &str) -> Result<&mut Owner, String> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase()) {
            return Err(format!(
                "an owner is named in lowercase letters, not {name:?}"
            ));
        }
        if !self.owners.contains_key(name) {
            let child = Command::new(self.sandbox.path("owners/claude"))
                .arg("infinity")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .map_err(|e| format!("start owner {name}: {e}"))?;
            self.owners.insert(
                name.to_owned(),
                Owner {
                    pid: child.id(),
                    child: Some(child),
                },
            );
        }
        self.owners
            .get_mut(name)
            .ok_or_else(|| format!("no owner {name}"))
    }

    fn normalise(&self, shown: Shown) -> Shown {
        let clean = |text: String| {
            let mut text = text;
            let raw = self.sandbox.path("");
            let raw = raw.to_string_lossy();
            let raw = raw.trim_end_matches('/');
            if let Ok(real) = fs::canonicalize(raw) {
                text = text.replace(real.to_string_lossy().as_ref(), "<sandbox>");
            }
            text = text.replace(raw, "<sandbox>");
            for (name, owner) in &self.owners {
                text = replace_number(
                    &text,
                    &format!("claude:{}", owner.pid),
                    &format!("claude:<{name}>"),
                );
            }
            text = hide_memory(&text, "memory ", " GB free");
            text = hide_memory(&text, "\"memAvailableMB\": ", "");
            hide_signal_seconds(&text)
        };
        Shown {
            stdout: clean(shown.stdout),
            stderr: clean(shown.stderr),
            code: shown.code,
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_group(child.id(), Signal::KILL);
            let _ = child.wait();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Waits for `child` up to [`DEADLINE`]; kills its group and returns none when it outlives it.
fn wait_child(child: &mut Child) -> Option<i32> {
    let mut code = None;
    let ended = poll_until(|| match child.try_wait() {
        Ok(Some(status)) => {
            code = Some(status.code().unwrap_or(-1));
            true
        }
        _ => false,
    });
    if !ended {
        kill_group(child.id(), Signal::KILL);
        let _ = child.wait();
    }
    code
}

/// Replaces `from` with `to` wherever no digit follows it, so pid 12 leaves pid 123 alone.
fn replace_number(text: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(from) {
        let after = &rest[at + from.len()..];
        out.push_str(&rest[..at]);
        if after.starts_with(|c: char| c.is_ascii_digit()) {
            out.push_str(from);
        } else {
            out.push_str(to);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Shows the seconds in `by SIG<NAME> after <n> s` as `<n>`: how long a run took before a
/// signal ended it depends on how busy the machine is.
fn hide_signal_seconds(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(" after ") {
        let head = &rest[..at];
        let tail = &rest[at + " after ".len()..];
        let by_signal = head
            .rsplit_once(" by ")
            .is_some_and(|(_, name)| name.starts_with("SIG") && !name.contains(' '));
        let len = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        out.push_str(head);
        out.push_str(" after ");
        if by_signal && len > 0 && tail[len..].starts_with(" s") {
            out.push_str("<n>");
            rest = &tail[len..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Shows a number that follows `before` and precedes `after` as `<mem>`.
fn hide_memory(text: &str, before: &str, after: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(before) {
        let tail = &rest[at + before.len()..];
        let len = tail
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(tail.len());
        out.push_str(&rest[..at + before.len()]);
        if len > 0 && tail[len..].starts_with(after) {
            out.push_str("<mem>");
        } else {
            out.push_str(&tail[..len]);
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// A binary that waits forever; linked as `claude`, its process is an owner devlab recognises.
fn sleep_binary() -> PathBuf {
    ["/usr/bin/sleep", "/bin/sleep"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("a sleep binary")
}
