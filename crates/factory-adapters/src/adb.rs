//! adb: the devices it lists, a device's shell, its reverse ports, and the stream of device
//! lists `adb track-devices -l` writes each time one changes.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use factory_core::lab::devices::parse_track;

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
    /// How long `adb track-devices` may take to show its first list.
    pub first_frame: Duration,
    /// How often adb is asked whether it still answers while the stream is quiet (adb writes
    /// only when the list changes, so silence alone is no fault). One question may take at most
    /// this long, or [`PROBE_TIMEOUT`] if that is less.
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
            first_frame: Duration::from_secs(15),
            probe_every: Duration::from_secs(60),
            restart_base: Duration::from_secs(3),
            restart_cap: Duration::from_secs(60),
        }
    }
}

impl Pace {
    /// The default pace, with the milliseconds `FACTORY_ADB_FIRST_FRAME_MS`,
    /// `FACTORY_ADB_PROBE_MS` and `FACTORY_ADB_BACKOFF_MS` give (the last also sets the cap, at
    /// twenty times it): the harness's way to make the watch quick.
    pub fn from_env() -> Pace {
        let ms = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .map(Duration::from_millis)
        };
        let mut pace = Pace::default();
        pace.first_frame = ms("FACTORY_ADB_FIRST_FRAME_MS").unwrap_or(pace.first_frame);
        pace.probe_every = ms("FACTORY_ADB_PROBE_MS").unwrap_or(pace.probe_every);
        if let Some(base) = ms("FACTORY_ADB_BACKOFF_MS") {
            pace.restart_base = base;
            pace.restart_cap = base.saturating_mul(20);
        }
        pace
    }
}

/// `adb track-devices -l`, in a process group of its own, watched. Its first list must come
/// within [`Pace::first_frame`]; after that it streams for as long as adb runs, and while it
/// is quiet adb is asked every [`Pace::probe_every`] whether it still answers (`adb
/// start-server`, which also starts a server that died). Either fails, and the stream's group
/// is killed, so the stream ends. Its group is killed too when the stream is dropped, and when
/// the process that started it dies, however it dies: the stream's stdin is a pipe only that
/// process holds the writing end of, and a small shell around adb kills the group when it
/// reads the end of it.
#[derive(Debug, Default)]
pub struct AdbTrack {
    pub pace: Pace,
}

/// The shell around adb: it keeps the pipe it got as stdin on descriptor 3, runs adb (given as
/// its arguments) with no stdin, and kills its own process group once the pipe reaches its end
/// or adb exits, so that nothing it started holds the stream's stdout open.
const WRAPPER: &str = r#"exec 3<&0 0</dev/null
"$@" 3<&- &
adb=$!
(cat <&3; kill -KILL 0) >/dev/null 2>&1 &
wait "$adb"
kill -KILL 0"#;

#[derive(Default)]
struct Watch {
    /// The stream has shown bytes.
    shown: bool,
    /// The stream is dropped: the watch stands down.
    dropped: bool,
    /// Why the watch killed the stream.
    cut_off: Option<String>,
}

type Shared = Arc<(Mutex<Watch>, Condvar)>;

fn watch(shared: &Shared) -> std::sync::MutexGuard<'_, Watch> {
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Stream {
    child: Child,
    /// What the stream showed before its first complete list; empty once that came.
    head: Vec<u8>,
    /// The writing end of the child's stdin; closing it, or the process ending, kills the group.
    stdin: Option<std::process::ChildStdin>,
    shared: Shared,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = match self.child.stdout.as_mut() {
            Some(out) => out.read(buf),
            None => Ok(0),
        };
        match read {
            Ok(n) if n > 0 => {
                if !watch(&self.shared).shown {
                    // A first list that stops halfway is as hung as none: the watch stands
                    // down only for a whole one.
                    self.head
                        .extend_from_slice(buf.get(..n).unwrap_or_default());
                    if !parse_track(&self.head).0.is_empty() {
                        self.head = Vec::new();
                        watch(&self.shared).shown = true;
                        self.shared.1.notify_all();
                    }
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
        watch(&self.shared).dropped = true;
        self.shared.1.notify_all();
        kill_group(self.child.id());
        drop(self.stdin.take());
        let _ = self.child.wait();
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

/// Whether adb answers: `adb start-server` finishes well within `limit`. It runs in the stream's
/// process group (`group`, the stream's leader), so that the group's end takes it along, even
/// the daemon's own end by SIGKILL.
fn adb_answers(group: u32, limit: Duration) -> bool {
    let Ok(group) = i32::try_from(group) else {
        return false;
    };
    let Ok(mut child) = Command::new(tool("adb"))
        .arg("start-server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(group)
        .spawn()
    else {
        return false;
    };
    let until = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Kills the stream's process group when it shows no list within [`Pace::first_frame`], or
/// when adb stops answering the questions asked while it is quiet; stands down when the stream
/// is dropped. The stream is not reaped before it is dropped, so the group is still its.
fn watch_stream(pid: u32, shared: Shared, pace: Pace) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("adb-track-watch".to_owned())
        .spawn(move || {
            let (_, cvar) = &*shared;
            let mut was_shown = false;
            loop {
                let limit = if was_shown {
                    pace.probe_every
                } else {
                    pace.first_frame
                };
                let guard = watch(&shared);
                let (mut guard, _) = cvar
                    .wait_timeout_while(guard, limit, |w| !w.dropped && (was_shown || !w.shown))
                    .unwrap_or_else(PoisonError::into_inner);
                if guard.dropped {
                    return;
                }
                if !was_shown {
                    if guard.shown {
                        was_shown = true;
                        continue;
                    }
                    guard.cut_off = Some(format!("no list in {}ms", pace.first_frame.as_millis()));
                    kill_group(pid);
                    return;
                }
                drop(guard);
                let asked = pace.probe_every.min(PROBE_TIMEOUT);
                if !adb_answers(pid, asked) {
                    let mut guard = watch(&shared);
                    if guard.dropped {
                        return;
                    }
                    guard.cut_off = Some(format!("adb did not answer in {}ms", asked.as_millis()));
                    kill_group(pid);
                    return;
                }
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
        let mut child = Command::new("sh")
            .args(["-c", WRAPPER, "adb-track", &tool("adb").to_string_lossy()])
            .args(["track-devices", "-l"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let shared: Shared = Arc::default();
        let pid = child.id();
        let stdin = child.stdin.take();
        // A stream with no watch is not handed out: dropping it kills its group.
        let stream = Stream {
            child,
            head: Vec::new(),
            stdin,
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
