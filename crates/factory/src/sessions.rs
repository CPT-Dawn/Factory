//! Claude Code's session logs: where a session's log is, reading the latest context from it as
//! lines are added, and the inotify watch that says when they are.
//!
//! A session run in `<cwd>` logs to `<claude>/projects/<cwd with every character but letters and
//! digits as `-`>/<session>.jsonl`; its subagents log to `<session>/subagents/agent-<id>.jsonl`
//! beside it, each with an `agent-<id>.meta.json` saying what it was started for and, once it
//! has, that it finished.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use factory_adapters::herdr::{AgentSession, SessionKind};
use factory_core::steward::{Reading, reading};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;
use serde_json::Value;
use tokio::io::unix::AsyncFd;

/// How much of a log's end is read back from at most to find its latest context.
const SCAN: u64 = 32 << 20;
const CHUNK: u64 = 1 << 20;

/// The log of a lead's own session, from what herdr reports of it: the path it gave, or where
/// Claude Code logs the session id it gave. A log not found under the cwd's project is looked
/// for in every project; one found nowhere is where it will appear, when the cwd is known.
pub fn lead_log(projects: &Path, cwd: Option<&str>, session: &AgentSession) -> Option<PathBuf> {
    let value = session.value.as_str();
    if session.kind == SessionKind::Path {
        return Some(PathBuf::from(value));
    }
    if value.is_empty() || value.contains('/') || value.starts_with('.') {
        return None;
    }
    let name = format!("{value}.jsonl");
    let expected = cwd.map(|cwd| projects.join(project(cwd)).join(&name));
    if expected.as_ref().is_some_and(|path| path.is_file()) {
        return expected;
    }
    let elsewhere = fs::read_dir(projects).ok().and_then(|dirs| {
        dirs.flatten()
            .map(|dir| dir.path().join(&name))
            .find(|path| path.is_file())
    });
    elsewhere.or(expected)
}

/// The project directory Claude Code names after `cwd`.
fn project(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The directory a session keeps beside its log, which holds its subagents' logs.
pub fn session_dir(log: &Path) -> PathBuf {
    log.with_extension("")
}

/// A file in a session's `subagents` directory, naming the subagent `agent-<id>` it belongs to.
#[derive(Debug, PartialEq, Eq)]
pub enum SubagentFile {
    /// The subagent's log.
    Log(String),
    /// The subagent's meta file.
    Meta(String),
}

/// What the file `name` in a session's `subagents` directory is, if it is a subagent's.
pub fn subagent_file(name: &OsStr) -> Option<SubagentFile> {
    let name = name.to_str()?;
    if !name.starts_with("agent-") {
        return None;
    }
    if let Some(agent) = name.strip_suffix(".jsonl") {
        return Some(SubagentFile::Log(agent.to_owned()));
    }
    name.strip_suffix(".meta.json")
        .map(|agent| SubagentFile::Meta(agent.to_owned()))
}

/// What a subagent's meta file says: what it was started to do, and whether it finished.
#[derive(Debug, Default)]
pub struct Meta {
    pub description: Option<String>,
    pub finished: bool,
}

/// Reads the meta file beside a subagent's log; a missing or unreadable one says nothing yet.
pub fn meta(log: &Path) -> Meta {
    let path = log.with_extension("meta.json");
    let Some(value) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return Meta::default();
    };
    Meta {
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        finished: value.get("finished").and_then(Value::as_bool) == Some(true),
    }
}

/// A log read up to some point: what has been read, and the start of a line still being
/// written.
#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    /// Which readings count: a lead's own log also carries its subagents' sidechain lines.
    sidechain: bool,
}

impl Tail {
    /// Opens the log at `path` at its end and returns its latest context, read back from its
    /// end; `sidechain` says whether subagents' lines count.
    pub fn open(path: &Path, sidechain: bool) -> io::Result<(Tail, Option<Reading>)> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        // Bytes [pos, len) are read; `head` holds the start of a line split at `pos`.
        let mut pos = len;
        let mut head: Vec<u8> = Vec::new();
        let mut lines_end = None;
        let mut latest = None;
        while pos > 0 && len - pos < SCAN && latest.is_none() {
            let start = pos.saturating_sub(CHUNK);
            let mut bytes = vec![0; usize::try_from(pos - start).unwrap_or(0)];
            file.read_exact_at(&mut bytes, start)?;
            bytes.append(&mut head);
            if lines_end.is_none() {
                // The bytes after the last newline are a line still being written.
                let Some(last) = bytes.iter().rposition(|b| *b == b'\n') else {
                    head = bytes;
                    pos = start;
                    continue;
                };
                lines_end = Some(start + last as u64 + 1);
                bytes.truncate(last);
            }
            let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
            let split = if start > 0 { lines.remove(0) } else { &[][..] };
            latest = lines
                .iter()
                .rev()
                .filter_map(|line| read_line(line))
                .find(|reading| counts(sidechain, reading));
            head = split.to_vec();
            pos = start;
        }
        let offset = lines_end.unwrap_or(pos);
        let tail = Tail {
            path: path.to_owned(),
            offset,
            partial: Vec::new(),
            sidechain,
        };
        Ok((tail, latest))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the lines added since, returning the latest context among them. A log that
    /// shrank was rewritten: it is read again from its start.
    pub fn read(&mut self) -> io::Result<Option<Reading>> {
        let file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Ok(None);
        }
        let mut bytes = vec![0; usize::try_from(len - self.offset).unwrap_or(0)];
        file.read_exact_at(&mut bytes, self.offset)?;
        self.offset = len;
        self.partial.append(&mut bytes);
        let Some(last) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Ok(None);
        };
        let rest = self.partial.split_off(last + 1);
        let lines = std::mem::replace(&mut self.partial, rest);
        let sidechain = self.sidechain;
        Ok(lines
            .split(|b| *b == b'\n')
            .rev()
            .filter_map(read_line)
            .find(|reading| counts(sidechain, reading)))
    }
}

/// Whether `reading` is one a log read with `sidechain` takes: a lead's own log also carries
/// its subagents' sidechain lines, which are not its context.
fn counts(sidechain: bool, reading: &Reading) -> bool {
    sidechain || !reading.sidechain
}

fn read_line(line: &[u8]) -> Option<Reading> {
    reading(std::str::from_utf8(line).ok()?)
}

/// One thing inotify reported.
#[derive(Debug)]
pub struct Change {
    /// The watch it came from; meaningless for an overflow.
    pub watch: i32,
    /// The file in the watched directory it is about.
    pub name: Option<OsString>,
    /// The file appeared, rather than grew.
    pub appeared: bool,
    /// Events were lost: everything must be read again.
    pub overflow: bool,
    /// The watch is gone, with its directory.
    pub gone: bool,
}

/// An inotify instance, read without blocking the runtime.
pub struct Inotify {
    fd: AsyncFd<OwnedFd>,
    buf: Vec<MaybeUninit<u8>>,
}

impl Inotify {
    pub fn new() -> io::Result<Inotify> {
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)?;
        // Deprecated for types that may swap their fd; an `OwnedFd` holds one fd for its life,
        // which is what the safe constructor needs, and `register` would need `unsafe`.
        #[allow(deprecated)]
        let fd = AsyncFd::new(fd)?;
        Ok(Inotify {
            fd,
            buf: vec![MaybeUninit::uninit(); 64 * 1024],
        })
    }

    /// Watches the directory `dir` for files appearing in it or growing.
    pub fn watch(&self, dir: &Path) -> io::Result<i32> {
        let flags =
            WatchFlags::CREATE | WatchFlags::MODIFY | WatchFlags::MOVED_TO | WatchFlags::ONLYDIR;
        Ok(inotify::add_watch(self.fd.get_ref(), dir, flags)?)
    }

    pub fn unwatch(&self, watch: i32) {
        // A watch whose directory went is gone already.
        let _ = inotify::remove_watch(self.fd.get_ref(), watch);
    }

    /// Waits for changes and returns every one there is.
    pub async fn next(&mut self) -> io::Result<Vec<Change>> {
        loop {
            let mut ready = self.fd.readable().await?;
            let mut changes = Vec::new();
            let buf = &mut self.buf;
            let read: Result<io::Result<()>, _> = ready.try_io(|fd| {
                let mut reader = inotify::Reader::new(fd.get_ref(), buf);
                loop {
                    match reader.next() {
                        Ok(event) => changes.push(change(&event)),
                        Err(Errno::AGAIN) => {
                            return Err(io::Error::from(io::ErrorKind::WouldBlock));
                        }
                        Err(e) => return Err(io::Error::from(e)),
                    }
                }
            });
            if let Ok(Err(e)) = read {
                return Err(e);
            }
            if !changes.is_empty() {
                return Ok(changes);
            }
        }
    }
}

fn change(event: &inotify::Event<'_>) -> Change {
    let flags = event.events();
    Change {
        watch: event.wd(),
        name: event
            .file_name()
            .map(|name| OsStr::from_bytes(name.to_bytes()).to_owned())
            .filter(|name| !name.is_empty()),
        appeared: flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO),
        overflow: flags.contains(ReadFlags::QUEUE_OVERFLOW),
        gone: flags.contains(ReadFlags::IGNORED),
    }
}
