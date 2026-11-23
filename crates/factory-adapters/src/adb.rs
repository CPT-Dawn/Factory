//! adb: the devices it lists, a device's shell, its reverse ports, and the stream of device
//! lists `adb track-devices -l` writes each time one changes.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

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

    fn emu_kill(&self, serial: &str) {
        let _ = adb(&["-s", serial, "emu", "kill"], Duration::from_secs(10));
    }
}

/// A source of `adb track-devices -l` streams.
pub trait TrackDevices: Send + Sync {
    /// A new stream: four hex digits of length, then that many bytes of `adb devices -l`
    /// lines, once now and once each time the list changes.
    fn start(&self) -> std::io::Result<Box<dyn Read + Send>>;
}

/// How long `adb track-devices` may take to show its first list; past it the stream's process
/// group is killed, and the stream ends.
const FIRST_FRAME: Duration = Duration::from_secs(15);

/// `adb track-devices -l`, in a process group of its own. Its first list must come within
/// [`FIRST_FRAME`], or its group is killed; after that it streams for as long as adb runs (adb
/// writes only when the list changes, so silence is no fault), and its group is killed when the
/// stream is dropped.
pub struct AdbTrack;

struct Stream {
    child: Child,
    /// Set once the stream has shown bytes or is dropped: the first-frame watchdog stands down.
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Stream {
    fn stand_down(&self) {
        let (done, cvar) = &*self.done;
        *done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = match self.child.stdout.as_mut() {
            Some(out) => out.read(buf),
            None => Ok(0),
        };
        if matches!(read, Ok(n) if n > 0) {
            self.stand_down();
        }
        read
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stand_down();
        kill_group(self.child.id());
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

/// Kills the stream's process group unless it shows bytes, or is dropped, within
/// [`FIRST_FRAME`]. The stream is not reaped before it is dropped, so the group is still its.
fn watch_first_frame(pid: u32, done: Arc<(Mutex<bool>, Condvar)>) {
    let _ = std::thread::Builder::new()
        .name("adb-track-first".to_owned())
        .spawn(move || {
            let (flag, cvar) = &*done;
            let guard = flag.lock().unwrap_or_else(PoisonError::into_inner);
            let (guard, _) = cvar
                .wait_timeout_while(guard, FIRST_FRAME, |done| !*done)
                .unwrap_or_else(PoisonError::into_inner);
            if !*guard {
                kill_group(pid);
            }
        });
}

impl TrackDevices for AdbTrack {
    fn start(&self) -> std::io::Result<Box<dyn Read + Send>> {
        let child = Command::new(tool("adb"))
            .args(["track-devices", "-l"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        watch_first_frame(child.id(), Arc::clone(&done));
        Ok(Box::new(Stream { child, done }))
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
