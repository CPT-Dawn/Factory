//! adb: the devices it lists, a device's shell, its reverse ports, and the stream of device
//! lists `adb track-devices -l` writes each time one changes.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use factory_core::lab::devices::{ListFraming, same_devices};

use crate::process;

/// How long one adb call may take.
const TIMEOUT: Duration = Duration::from_secs(15);

/// An Android SDK tool: from `$ANDROID_HOME` (systemd units have no login PATH), else from
/// PATH.
pub fn tool(name: &str) -> PathBuf {
    let rel = match name {
        "adb" => "platform-tools/adb",
        "emulator" => "emulator/emulator",
        "avdmanager" => "cmdline-tools/latest/bin/avdmanager",
        _ => "",
    };
    let sdk = android_home();
    if !rel.is_empty() && sdk.join(rel).is_file() {
        return sdk.join(rel);
    }
    PathBuf::from(name)
}

/// `$ANDROID_HOME`, else `$ANDROID_SDK_ROOT`, else `~/Android/Sdk`.
pub fn android_home() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    var("ANDROID_HOME")
        .or_else(|| var("ANDROID_SDK_ROOT"))
        .map_or_else(
            || {
                PathBuf::from(var("HOME").unwrap_or_default())
                    .join("Android")
                    .join("Sdk")
            },
            PathBuf::from,
        )
}

pub trait Adb: Send + Sync {
    /// `adb -s <serial> shell <cmd>`: what it printed, or none when adb failed.
    fn shell(&self, serial: &str, cmd: &str) -> Option<String>;
    /// `adb -s <serial> reverse tcp:<device> tcp:<host>`; whether it worked.
    fn reverse(&self, serial: &str, device_port: u32, host_port: u32) -> bool;
    /// `adb -s <serial> reverse --remove tcp:<device>`; whether it worked.
    fn unreverse(&self, serial: &str, device_port: u32) -> bool;
    /// `adb -s <serial> emu kill`: an emulator saves its quick-boot snapshot and exits.
    fn emu_kill(&self, serial: &str);
}

/// The adb of the Android SDK, else of PATH.
pub struct AdbCli;

fn adb(args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(tool("adb"));
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = process::run(cmd, timeout).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Adb for AdbCli {
    fn shell(&self, serial: &str, cmd: &str) -> Option<String> {
        adb(&["-s", serial, "shell", cmd], TIMEOUT)
    }

    fn reverse(&self, serial: &str, device_port: u32, host_port: u32) -> bool {
        adb(
            &[
                "-s",
                serial,
                "reverse",
                &format!("tcp:{device_port}"),
                &format!("tcp:{host_port}"),
            ],
            TIMEOUT,
        )
        .is_some()
    }

    fn unreverse(&self, serial: &str, device_port: u32) -> bool {
        let port = format!("tcp:{device_port}");
        adb(
            &["-s", serial, "reverse", "--remove", &port],
            Duration::from_secs(5),
        )
        .is_some()
    }

    fn emu_kill(&self, serial: &str) {
        let _ = adb(&["-s", serial, "emu", "kill"], Duration::from_secs(10));
    }
}

/// A source of `adb track-devices -l` streams.
pub trait TrackDevices: Send + Sync {
    /// A new stream: four hex digits of length, then that many bytes of `adb devices -l`
    /// lines, once now and once each time the list changes. A stream that was cut off because
    /// adb stopped answering ends with an error that says so.
    fn start(&self) -> std::io::Result<Box<dyn Read + Send>>;
}

/// How long the device stream's watch lets things take, and how long it waits to restart one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    /// How long a list may take to come whole: the first from the stream's start, each later
    /// one from its first byte.
    pub list_within: Duration,
    /// How often adb is asked for its device list while the stream is quiet (adb writes only
    /// when the list changes, so silence alone is no fault). One question may take at most this
    /// long, or [`PROBE_TIMEOUT`] if that is less.
    pub probe_every: Duration,
    /// The wait before the first restart of a stream; it doubles with each next one.
    pub restart_base: Duration,
    /// The longest wait before a restart.
    pub restart_cap: Duration,
}

/// How long one question to adb may take at most.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

impl Default for Pace {
    fn default() -> Pace {
        Pace {
            list_within: Duration::from_secs(15),
            probe_every: Duration::from_secs(60),
            restart_base: Duration::from_secs(3),
            restart_cap: Duration::from_secs(60),
        }
    }
}

impl Pace {
    /// The default pace, with the milliseconds `FACTORY_ADB_LIST_MS`, `FACTORY_ADB_PROBE_MS`
    /// and `FACTORY_ADB_BACKOFF_MS` give (the last also sets the cap, at twenty times it): the
    /// harness's way to make the watch quick.
    pub fn from_env() -> Pace {
        let ms = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .map(Duration::from_millis)
        };
        let mut pace = Pace::default();
        pace.list_within = ms("FACTORY_ADB_LIST_MS").unwrap_or(pace.list_within);
        pace.probe_every = ms("FACTORY_ADB_PROBE_MS").unwrap_or(pace.probe_every);
        if let Some(base) = ms("FACTORY_ADB_BACKOFF_MS") {
            pace.restart_base = base;
            pace.restart_cap = base.saturating_mul(20);
        }
        pace
    }
}

/// `adb track-devices -l`, tethered (see [`Tethered`]) and watched. Each list must come whole
/// within [`Pace::list_within`]: the first from the start, a later one from its first byte.
/// While the stream is quiet between lists, adb is asked for its device list every
/// [`Pace::probe_every`] (`adb devices -l`, which also starts a server that died): it must
/// answer, and a list it gives that the stream has not shown must come on the stream, whole,
/// within [`Pace::list_within`]. Any of these fails, and the stream's group is killed, so the
/// stream ends with an error that says why.
#[derive(Debug, Default)]
pub struct AdbTrack {
    pub pace: Pace,
}

/// The shell around an adb command: it keeps the pipe it got as stdin on descriptor 3, runs adb
/// (given as its arguments) with no stdin and no stderr, writes adb's exit status to its own
/// stderr, and kills its own process group once the pipe reaches its end or adb exits, so that
/// nothing adb started lives on or holds the command's stdout open.
const WRAPPER: &str = r#"exec 3<&0 0</dev/null
"$@" 3<&- 2>/dev/null &
adb=$!
(cat <&3; kill -KILL 0) >/dev/null 2>&1 &
wait "$adb"
echo "$?" >&2
kill -KILL 0"#;

/// An adb command under [`WRAPPER`], in a process group of its own, whose stdin is a pipe only
/// this process holds the writing end of: when this process dies, however it dies, the pipe
/// ends and the wrapper kills the group. Dropped, it kills the group itself.
struct Tethered {
    child: Child,
    lifeline: Option<std::process::ChildStdin>,
    /// The wrapper is reaped, so its pid no longer names the group.
    reaped: bool,
}

impl Tethered {
    fn start(args: &[&str], stdout: Stdio, stderr: Stdio) -> std::io::Result<Tethered> {
        let mut child = Command::new("sh")
            .args(["-c", WRAPPER, "adb", &tool("adb").to_string_lossy()])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0)
            .spawn()?;
        let lifeline = child.stdin.take();
        Ok(Tethered {
            child,
            lifeline,
            reaped: false,
        })
    }

    /// Kills the group, while its leader is still unreaped so that its pid still names it.
    fn kill(&self) {
        if !self.reaped {
            kill_group(self.child.id());
        }
    }

    /// Whether the wrapper has ended (and is reaped).
    fn ended(&mut self) -> bool {
        self.reaped = self.reaped || matches!(self.child.try_wait(), Ok(Some(_)));
        self.reaped
    }
}

impl Drop for Tethered {
    fn drop(&mut self) {
        self.kill();
        // A wrapper already reaped killed its group as it ended; should anything be left, the
        // pipe's end tells the group's watcher, which is in it, to kill the rest.
        drop(self.lifeline.take());
        if !self.reaped {
            let _ = self.child.wait();
        }
    }
}

#[derive(Default)]
struct Watch {
    /// Since when, and why, the stream has stood waiting for a list to come whole: from its
    /// start until its first list, from the first byte of each next one, and from when adb
    /// listed devices the stream had not shown; `None` while it is quiet and up to date.
    awaiting: Option<(Instant, &'static str)>,
    /// The whole lists the stream has shown, and the last of them.
    lists: u64,
    last_list: Option<Vec<u8>>,
    /// What adb said when last asked for its device list (`None` inside: it did not answer),
    /// until the watch reads it.
    answer: Option<Option<String>>,
    /// The stream is dropped: the watch stands down.
    dropped: bool,
    /// Why the watch killed the stream.
    cut_off: Option<String>,
}

/// Why a list is awaited.
const NO_WHOLE_LIST: &str = "no whole list";
const BEHIND_ADB: &str = "no list with the devices adb lists";

type Shared = Arc<(Mutex<Watch>, Condvar)>;

fn watch(shared: &Shared) -> std::sync::MutexGuard<'_, Watch> {
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Stream {
    adb: Tethered,
    framing: ListFraming,
    shared: Shared,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = match self.adb.child.stdout.as_mut() {
            Some(out) => out.read(buf),
            None => Ok(0),
        };
        match read {
            Ok(n) if n > 0 => {
                let finished = self.framing.feed(buf.get(..n).unwrap_or_default());
                let mid_list = self.framing.mid_list();
                let mut watch = watch(&self.shared);
                let was = watch.awaiting;
                if finished > 0 {
                    watch.lists += finished as u64;
                    watch.last_list = self.framing.last_list().map(<[u8]>::to_vec);
                    watch.awaiting = mid_list.then(|| (Instant::now(), NO_WHOLE_LIST));
                } else if was.is_none() && mid_list {
                    watch.awaiting = Some((Instant::now(), NO_WHOLE_LIST));
                }
                if watch.awaiting != was {
                    self.shared.1.notify_all();
                }
                Ok(n)
            }
            ended => match watch(&self.shared).cut_off.clone() {
                Some(why) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, why)),
                None => ended,
            },
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Under the lock, so the watch kills no group once the stream is on its way out; the
        // tethered adb is killed and reaped next, as the fields drop.
        watch(&self.shared).dropped = true;
        self.shared.1.notify_all();
    }
}

fn kill_group(pid: u32) {
    if let Some(group) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}

/// adb's device list: `adb devices -l` (which also starts a server that died), tethered in a
/// group of its own; `None` when it fails or outlives `limit`, and then its group is killed.
fn ask_adb(limit: Duration) -> Option<String> {
    let mut probe = Tethered::start(&["devices", "-l"], Stdio::piped(), Stdio::piped()).ok()?;
    let until = Instant::now() + limit;
    while !probe.ended() {
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut status = String::new();
    probe
        .child
        .stderr
        .take()?
        .read_to_string(&mut status)
        .ok()?;
    let mut said = String::new();
    probe.child.stdout.take()?.read_to_string(&mut said).ok()?;
    (status.trim() == "0").then_some(said)
}

/// Kills the stream's process group when a list it awaits does not come whole within
/// [`Pace::list_within`], or when adb does not answer the question asked every
/// [`Pace::probe_every`] while the stream is quiet; stands down when the stream is dropped.
/// adb's answer, its device list, must match the stream's last list: when it does not, and no
/// list came while adb was asked, the stream awaits one. The question runs on a thread of its
/// own, so no deadline waits on it. The stream is not reaped before it is dropped, so the group
/// is still its.
fn watch_stream(pid: u32, shared: Shared, pace: Pace) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("adb-track-watch".to_owned())
        .spawn(move || {
            let (_, cvar) = &*shared;
            let asked = pace.probe_every.min(PROBE_TIMEOUT);
            let mut next_question = Instant::now() + pace.probe_every;
            // The lists shown when adb was asked, while a question is out.
            let mut asking: Option<u64> = None;
            let mut guard = watch(&shared);
            loop {
                if guard.dropped {
                    return;
                }
                if let Some(answer) = guard.answer.take() {
                    let lists_then = asking.take();
                    let Some(said) = answer else {
                        guard.cut_off =
                            Some(format!("adb did not answer in {}ms", asked.as_millis()));
                        kill_group(pid);
                        return;
                    };
                    let behind = !guard
                        .last_list
                        .as_deref()
                        .is_some_and(|list| same_devices(list, &said));
                    if behind && guard.awaiting.is_none() && lists_then == Some(guard.lists) {
                        guard.awaiting = Some((Instant::now(), BEHIND_ADB));
                    }
                }
                let now = Instant::now();
                if let Some((since, why)) = guard.awaiting {
                    let deadline = since + pace.list_within;
                    if now >= deadline {
                        guard.cut_off =
                            Some(format!("{why} in {}ms", pace.list_within.as_millis()));
                        kill_group(pid);
                        return;
                    }
                    guard = cvar
                        .wait_timeout(guard, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                    continue;
                }
                if asking.is_none() && now >= next_question {
                    next_question = now + pace.probe_every;
                    let lists = guard.lists;
                    let answer_to = Arc::clone(&shared);
                    let sent = std::thread::Builder::new()
                        .name("adb-track-ask".to_owned())
                        .spawn(move || {
                            let said = ask_adb(asked);
                            watch(&answer_to).answer = Some(said);
                            answer_to.1.notify_all();
                        });
                    if sent.is_ok() {
                        asking = Some(lists);
                    }
                }
                // A question out is waited for by its answer's notice; the next one is due at
                // `next_question`.
                let wait = if asking.is_some() {
                    pace.probe_every
                } else {
                    next_question.saturating_duration_since(now)
                };
                guard = cvar
                    .wait_timeout(guard, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        })
        .map(drop)
}

impl AdbTrack {
    pub fn from_env() -> AdbTrack {
        AdbTrack {
            pace: Pace::from_env(),
        }
    }
}

impl TrackDevices for AdbTrack {
    fn start(&self) -> std::io::Result<Box<dyn Read + Send>> {
        let adb = Tethered::start(&["track-devices", "-l"], Stdio::piped(), Stdio::null())?;
        let pid = adb.child.id();
        let shared: Shared = Arc::new((
            Mutex::new(Watch {
                awaiting: Some((Instant::now(), NO_WHOLE_LIST)),
                ..Watch::default()
            }),
            Condvar::new(),
        ));
        // A stream with no watch is not handed out: dropping it kills its group.
        let stream = Stream {
            adb,
            framing: ListFraming::default(),
            shared: Arc::clone(&shared),
        };
        watch_stream(pid, shared, self.pace)?;
        Ok(Box::new(stream))
    }
}

/// An adb whose shells answer from a map of command to output (a command not in it fails), and
/// that records every call.
#[derive(Debug, Default)]
pub struct FakeAdb {
    pub answers: BTreeMap<String, String>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeAdb {
    fn record(&self, call: String) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }
}

impl Adb for FakeAdb {
    fn shell(&self, serial: &str, cmd: &str) -> Option<String> {
        self.record(format!("-s {serial} shell {cmd}"));
        self.answers.get(cmd).cloned()
    }

    fn reverse(&self, serial: &str, device_port: u32, host_port: u32) -> bool {
        self.record(format!(
            "-s {serial} reverse tcp:{device_port} tcp:{host_port}"
        ));
        true
    }

    fn unreverse(&self, serial: &str, device_port: u32) -> bool {
        self.record(format!("-s {serial} reverse --remove tcp:{device_port}"));
        true
    }

    fn emu_kill(&self, serial: &str) {
        self.record(format!("-s {serial} emu kill"));
    }
}

/// A stream that plays back the bytes it was given, then ends.
#[derive(Debug, Default, Clone)]
pub struct FakeTrack {
    pub bytes: Vec<u8>,
}

impl TrackDevices for FakeTrack {
    fn start(&self) -> std::io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
    }
}
