//! The emulators devlab runs: AVDs made with avdmanager, each emulator run as a transient
//! systemd user unit (`systemd-run --user`) so it outlives whoever asked for it and can be
//! stopped cleanly.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::adb::{Adb, android_home, tool};
use crate::process;

/// How long `systemctl` and `systemd-run` may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long avdmanager may take to make an AVD.
const AVD_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `systemctl stop` may take on an emulator.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// One emulator to boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmulatorSpec {
    pub idx: u32,
    /// `devlab-<idx>`.
    pub avd: String,
    /// `devlab-emu-<idx>`.
    pub unit: String,
    pub port: u32,
    pub serial: String,
    /// The system image a new AVD is made from; empty picks the newest installed.
    pub image: String,
    pub device: String,
    pub gpu: String,
}

pub trait Emulators: Send + Sync {
    /// Makes the spec's AVD if it has none; else why not (exit 77).
    fn ensure_avd(&self, spec: &EmulatorSpec) -> Result<(), String>;
    /// Starts the spec's unit, unless it runs; else why not (exit 77).
    fn start(&self, spec: &EmulatorSpec) -> Result<(), String>;
    /// Whether the unit runs.
    fn unit_active(&self, unit: &str) -> bool;
    /// Stops the emulator: `emu kill` first, so it saves its snapshot, then the unit.
    fn stop(&self, unit: &str, serial: &str, adb: &dyn Adb);
}

/// The user's systemd and the Android SDK.
pub struct SystemdEmulators;

fn systemctl(args: &[&str], timeout: Duration) -> bool {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    process::run(cmd, timeout).is_ok_and(|out| out.status.success())
}

/// Where avdmanager writes AVDs: `ANDROID_AVD_HOME`, else `ANDROID_USER_HOME/avd`, else
/// (newer SDKs) `$XDG_CONFIG_HOME/.android/avd`, else `~/.android/avd`.
fn avd_homes() -> Vec<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let home = PathBuf::from(var("HOME").unwrap_or_default());
    let mut out = Vec::new();
    if let Some(dir) = var("ANDROID_AVD_HOME") {
        out.push(PathBuf::from(dir));
    }
    if let Some(dir) = var("ANDROID_USER_HOME") {
        out.push(PathBuf::from(dir).join("avd"));
    }
    out.push(
        var("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from)
            .join(".android/avd"),
    );
    out.push(home.join(".android/avd"));
    out
}

fn avd_ini(name: &str) -> Option<PathBuf> {
    avd_homes()
        .into_iter()
        .map(|h| h.join(format!("{name}.avd/config.ini")))
        .find(|f| f.is_file())
}

/// The newest x86_64 system image installed, google_apis first.
fn auto_image() -> Result<String, String> {
    let root = android_home().join("system-images");
    let Ok(apis) = fs::read_dir(&root) else {
        return Err(format!(
            "no system images under {}: sdkmanager \"system-images;android-35;google_apis;x86_64\"",
            root.display()
        ));
    };
    let mut found: Vec<(u32, String)> = Vec::new();
    for api in apis.flatten() {
        let api = api.file_name().to_string_lossy().into_owned();
        let n = api.trim_start_matches("android-").parse().unwrap_or(0);
        let Ok(tags) = fs::read_dir(root.join(&api)) else {
            continue;
        };
        for tag in tags.flatten() {
            let tag = tag.file_name().to_string_lossy().into_owned();
            if root.join(&api).join(&tag).join("x86_64").exists() {
                found.push((n, format!("system-images;{api};{tag};x86_64")));
            }
        }
    }
    found.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            b.1.contains("google_apis;")
                .cmp(&a.1.contains("google_apis;"))
        })
    });
    found
        .into_iter()
        .next()
        .map(|(_, image)| image)
        .ok_or_else(|| "no x86_64 system image installed".to_owned())
}

/// The AVD's config.ini, tuned for headless test runs.
fn tune(ini: &Path) {
    const TUNE: [(&str, &str); 5] = [
        ("hw.ramSize", "2048"),
        ("hw.keyboard", "yes"),
        ("disk.dataPartition.size", "6G"),
        ("hw.audioInput", "no"),
        ("hw.audioOutput", "no"),
    ];
    let text = fs::read_to_string(ini).unwrap_or_default();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let key = l.split('=').next().unwrap_or("").trim();
            !TUNE.iter().any(|(k, _)| *k == key)
        })
        .map(str::to_owned)
        .collect();
    lines.extend(TUNE.iter().map(|(k, v)| format!("{k}={v}")));
    let _ = fs::write(ini, lines.join("\n") + "\n");
}

impl Emulators for SystemdEmulators {
    fn ensure_avd(&self, spec: &EmulatorSpec) -> Result<(), String> {
        if avd_ini(&spec.avd).is_some() {
            return Ok(());
        }
        let image = if spec.image.is_empty() {
            auto_image()?
        } else {
            spec.image.clone()
        };
        let mut cmd = Command::new(tool("avdmanager"));
        cmd.args(["create", "avd", "-n", &spec.avd, "-k", &image, "-d"])
            .arg(&spec.device)
            .arg("--force")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // avdmanager asks whether to make a custom hardware profile: no.
        let out = spawn_answering(cmd, b"no\n");
        let ini = avd_ini(&spec.avd);
        match (out, ini) {
            (Ok((true, _)), Some(ini)) => {
                tune(&ini);
                Ok(())
            }
            (out, _) => {
                let said = match out {
                    Ok((_, text)) => text,
                    Err(e) => e,
                };
                Err(format!(
                    "avdmanager could not create {} from {image}: {}",
                    spec.avd,
                    said.trim().lines().last().unwrap_or("")
                ))
            }
        }
    }

    fn start(&self, spec: &EmulatorSpec) -> Result<(), String> {
        if self.unit_active(&spec.unit) {
            return Ok(());
        }
        systemctl(&["reset-failed", &spec.unit], TIMEOUT);
        let sdk = android_home().to_string_lossy().into_owned();
        // The emulator looks only in ANDROID_AVD_HOME and ~/.android/avd: name the folder
        // avdmanager used.
        let avd_home = avd_ini(&spec.avd)
            .and_then(|ini| ini.parent()?.parent().map(Path::to_path_buf))
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut cmd = Command::new("systemd-run");
        cmd.args([
            "--user".to_owned(),
            "--quiet".to_owned(),
            "--collect".to_owned(),
            format!("--unit={}", spec.unit),
            format!("--description=devlab emulator {}", spec.idx),
            format!("--setenv=ANDROID_HOME={sdk}"),
            format!("--setenv=ANDROID_SDK_ROOT={sdk}"),
            format!("--setenv=ANDROID_AVD_HOME={avd_home}"),
        ])
        .arg(tool("emulator"))
        .args([
            "-avd".to_owned(),
            spec.avd.clone(),
            "-port".to_owned(),
            spec.port.to_string(),
            "-no-window".to_owned(),
            "-no-audio".to_owned(),
            "-no-boot-anim".to_owned(),
            "-gpu".to_owned(),
            spec.gpu.clone(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        match process::run(cmd, TIMEOUT) {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "could not start {}: {}",
                spec.unit,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("could not start {}: systemd-run: {e}", spec.unit)),
        }
    }

    fn unit_active(&self, unit: &str) -> bool {
        systemctl(&["is-active", "--quiet", unit], TIMEOUT)
    }

    fn stop(&self, unit: &str, serial: &str, adb: &dyn Adb) {
        adb.emu_kill(serial);
        for _ in 0..10 {
            if !self.unit_active(unit) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        systemctl(&["stop", unit], STOP_TIMEOUT);
    }
}

/// Runs `cmd` with `input` on its stdin: whether it succeeded, and its stderr (else stdout).
fn spawn_answering(cmd: Command, input: &'static [u8]) -> Result<(bool, String), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut cmd = cmd;
    // process::run spawns; the answer is written by a thread that owns a pipe of its own.
    let (reader, mut writer) = std::io::pipe().map_err(|e| e.to_string())?;
    cmd.stdin(reader);
    std::thread::spawn(move || {
        let _ = writer.write_all(input);
        let _ = tx.send(());
    });
    let out = process::run(cmd, AVD_TIMEOUT).map_err(|e| e.to_string())?;
    let _ = rx.recv_timeout(Duration::from_secs(1));
    let text = if out.stderr.is_empty() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    Ok((out.status.success(), text))
}

/// Emulators whose units run while their names are in `active`; `start` adds one there
/// unless `refuse` says why it can't. Every call is recorded.
#[derive(Debug, Default)]
pub struct FakeEmulators {
    pub active: Mutex<BTreeSet<String>>,
    pub refuse: Option<String>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeEmulators {
    fn record(&self, call: String) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    fn units(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Emulators for FakeEmulators {
    fn ensure_avd(&self, spec: &EmulatorSpec) -> Result<(), String> {
        self.record(format!("avd {}", spec.avd));
        Ok(())
    }

    fn start(&self, spec: &EmulatorSpec) -> Result<(), String> {
        self.record(format!("start {}", spec.unit));
        if let Some(why) = &self.refuse {
            return Err(format!("could not start {}: {why}", spec.unit));
        }
        self.units().insert(spec.unit.clone());
        Ok(())
    }

    fn unit_active(&self, unit: &str) -> bool {
        self.units().contains(unit)
    }

    fn stop(&self, unit: &str, serial: &str, adb: &dyn Adb) {
        self.record(format!("stop {unit}"));
        adb.emu_kill(serial);
        self.units().remove(unit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::FakeAdb;

    fn spec() -> EmulatorSpec {
        EmulatorSpec {
            idx: 0,
            avd: "devlab-0".into(),
            unit: "devlab-emu-0".into(),
            port: 5580,
            serial: "emulator-5580".into(),
            image: String::new(),
            device: "pixel_6".into(),
            gpu: "swiftshader_indirect".into(),
        }
    }

    #[test]
    fn the_fake_runs_a_unit_from_its_start_to_its_stop() {
        let emus = FakeEmulators::default();
        emus.start(&spec()).unwrap();
        assert!(emus.unit_active("devlab-emu-0"));
        emus.stop("devlab-emu-0", "emulator-5580", &FakeAdb::default());
        assert!(!emus.unit_active("devlab-emu-0"));
        let refusing = FakeEmulators {
            refuse: Some("no".into()),
            ..FakeEmulators::default()
        };
        assert_eq!(
            refusing.start(&spec()),
            Err("could not start devlab-emu-0: no".into())
        );
    }

    #[test]
    fn a_config_ini_is_tuned_and_keeps_its_other_lines() {
        let dir = std::env::temp_dir().join(format!("factory-avd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("config.ini");
        fs::write(&ini, "avd.ini.encoding=UTF-8\nhw.ramSize=1536\n").unwrap();
        tune(&ini);
        let text = fs::read_to_string(&ini).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(text.starts_with("avd.ini.encoding=UTF-8\nhw.ramSize=2048\n"));
        assert!(text.ends_with("hw.audioOutput=no\n"));
    }
}
