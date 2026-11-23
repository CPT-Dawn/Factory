//! The machine config (`~/.config/devlab/config.json`, optional) and a project's `devlab.json`,
//! read from their text, and the per-owner slot cap they set.

use serde_json::{Map, Value};

/// A config file that can't be used: the line devlab prints, exit 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// What the lab reads from the machine config.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineConfig {
    /// Process names that own a lease: the nearest such ancestor of the caller.
    pub owner_names: Vec<String>,
    /// Default seconds acquire waits for a free slot.
    pub wait_sec: f64,
    /// Stack slots of a project one owner may hold, for projects whose devlab.json sets none.
    pub max_slots_per_owner: Option<u32>,
    /// RAM a stack is expected to take, and what the host must keep free after it.
    pub stack_ram_mb: i64,
    pub host_floor_mb: i64,
    /// Emulators devlab runs at once.
    pub max_emulators: u32,
    /// The system image a new AVD is made from; empty picks the newest installed.
    pub emulator_image: String,
    pub emulator_device: String,
    pub emulator_gpu: String,
    /// Console port of emulator 0; emulator i uses base + 2i.
    pub emulator_port_base: u32,
    /// RAM an emulator is expected to take.
    pub emulator_ram_mb: i64,
    /// Keep the screen on while plugged in.
    pub prep_stay_awake: bool,
    /// Turn animations off, for steadier UI tests.
    pub prep_animations_off: bool,
}

impl Default for MachineConfig {
    fn default() -> MachineConfig {
        MachineConfig {
            owner_names: vec!["claude".to_owned(), "codex".to_owned()],
            wait_sec: 480.0,
            max_slots_per_owner: None,
            stack_ram_mb: 2048,
            host_floor_mb: 5120,
            max_emulators: 2,
            emulator_image: String::new(),
            emulator_device: "pixel_6".to_owned(),
            emulator_gpu: "swiftshader_indirect".to_owned(),
            emulator_port_base: 5580,
            emulator_ram_mb: 3072,
            prep_stay_awake: true,
            prep_animations_off: true,
        }
    }
}

/// A project's local Supabase stack: its directory in the worktree and the CLI that runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supabase {
    pub dir: String,
    pub cli: String,
}

/// A project's `devlab.json`. Maps keep the order the file wrote them in, which is the order
/// their exports are printed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    /// How many stack slots the project may run at once.
    pub max_slots: u32,
    /// Overrides the machine config's cap.
    pub max_slots_per_owner: Option<u32>,
    /// Slot n adds n * slot_stride to every port.
    pub slot_stride: u32,
    /// Host ports the project's own servers use in slot 0, exported under these names.
    pub ports: Vec<(String, u32)>,
    /// Device port -> exported port name (or a port), for `adb reverse`.
    pub reverse: Vec<(String, String)>,
    pub supabase: Option<Supabase>,
    /// Extra exports; `${NAME}` expands to any other exported value.
    pub env: Vec<(String, String)>,
}

/// The machine config in `text`, read from `file` (named in errors).
pub fn machine_config(file: &str, text: &str) -> Result<MachineConfig, ConfigError> {
    let raw = object(file, text)?;
    let mut cfg = MachineConfig::default();
    if let Some(v) = raw.get("ownerNames") {
        cfg.owner_names = v
            .as_array()
            .and_then(|names| {
                names
                    .iter()
                    .map(|n| n.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                ConfigError(format!("{file}: \"ownerNames\" must be a list of names"))
            })?;
    }
    if let Some(v) = raw.get("waitSec") {
        cfg.wait_sec = v.as_f64().filter(|n| *n >= 0.0).ok_or_else(|| {
            ConfigError(format!("{file}: \"waitSec\" must be a number of seconds"))
        })?;
    }
    for (key, at) in [
        ("stackRamMB", &mut cfg.stack_ram_mb),
        ("hostFloorMB", &mut cfg.host_floor_mb),
        ("emulatorRamMB", &mut cfg.emulator_ram_mb),
    ] {
        if let Some(v) = raw.get(key) {
            *at = v
                .as_f64()
                .map(|n| n as i64)
                .ok_or_else(|| ConfigError(format!("{file}: \"{key}\" must be a number")))?;
        }
    }
    for (key, at) in [
        ("maxEmulators", &mut cfg.max_emulators),
        ("emulatorPortBase", &mut cfg.emulator_port_base),
    ] {
        if let Some(v) = raw.get(key) {
            *at = v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| ConfigError(format!("{file}: \"{key}\" must be a whole number")))?;
        }
    }
    for (key, at) in [
        ("emulatorImage", &mut cfg.emulator_image),
        ("emulatorDevice", &mut cfg.emulator_device),
        ("emulatorGpu", &mut cfg.emulator_gpu),
    ] {
        if let Some(v) = raw.get(key) {
            *at = v
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ConfigError(format!("{file}: \"{key}\" must be a string")))?;
        }
    }
    if let Some(prep) = raw.get("prep") {
        let prep = prep
            .as_object()
            .ok_or_else(|| ConfigError(format!("{file}: \"prep\" must be an object")))?;
        for (key, at) in [
            ("stayAwake", &mut cfg.prep_stay_awake),
            ("animationsOff", &mut cfg.prep_animations_off),
        ] {
            if let Some(v) = prep.get(key) {
                *at = v.as_bool().ok_or_else(|| {
                    ConfigError(format!("{file}: \"prep.{key}\" must be true or false"))
                })?;
            }
        }
    }
    cfg.max_slots_per_owner = cap(&raw, file)?;
    Ok(cfg)
}

/// The project config in `text`, read from `file` (named in errors).
pub fn project_config(file: &str, text: &str) -> Result<ProjectConfig, ConfigError> {
    let raw = object(file, text)?;
    let name = raw.get("name").and_then(Value::as_str).unwrap_or_default();
    if !valid_name(name) {
        return Err(ConfigError(format!(
            "{file}: \"name\" must be lowercase letters, digits and -"
        )));
    }
    let max_slots_per_owner = cap(&raw, file)?;
    let whole = |key: &str, default: u32, least: u64| -> Result<u32, ConfigError> {
        match raw.get(key) {
            None => Ok(default),
            Some(v) => v
                .as_u64()
                .filter(|n| *n >= least)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    ConfigError(format!(
                        "{file}: \"{key}\" must be a whole number, {least} or more"
                    ))
                }),
        }
    };
    let max_slots = whole("maxSlots", 3, 1)?;
    let slot_stride = whole("slotStride", 100, 0)?;
    let ports = entries(&raw, "ports", file, |v| {
        v.as_u64().and_then(|n| u32::try_from(n).ok())
    })?;
    let reverse = entries(&raw, "reverse", file, |v| v.as_str().map(str::to_owned))?;
    let env = entries(&raw, "env", file, |v| v.as_str().map(str::to_owned))?;
    let supabase = match raw.get("supabase") {
        None => None,
        Some(Value::String(dir)) => Some(Supabase {
            dir: dir.clone(),
            cli: "bunx supabase".to_owned(),
        }),
        Some(Value::Object(o)) => {
            let text = |key: &str, default: &str| -> Result<String, ConfigError> {
                match o.get(key) {
                    None => Ok(default.to_owned()),
                    Some(Value::String(s)) => Ok(s.clone()),
                    Some(_) => Err(ConfigError(format!(
                        "{file}: \"supabase.{key}\" must be a string"
                    ))),
                }
            };
            Some(Supabase {
                dir: text("dir", "supabase")?,
                cli: text("cli", "bunx supabase")?,
            })
        }
        Some(_) => {
            return Err(ConfigError(format!(
                "{file}: \"supabase\" must be a directory or an object"
            )));
        }
    };
    let last = max_slots - 1;
    for (key, port) in &ports {
        let highest = u64::from(*port) + u64::from(slot_stride) * u64::from(last);
        if *port == 0 || highest > 65535 {
            return Err(ConfigError(format!(
                "{file}: \"ports.{key}\" must stay within 1-65535 in every slot (slot {last} would use {highest})"
            )));
        }
    }
    Ok(ProjectConfig {
        name: name.to_owned(),
        max_slots,
        max_slots_per_owner,
        slot_stride,
        ports,
        reverse,
        supabase,
        env,
    })
}

/// How many stack slots of a project one owner (a lead and every subagent in its process) may
/// hold at once. devlab.json's cap wins over the machine's; the default leaves room for one
/// other owner. Always within 1..=max_slots.
pub fn slot_cap(max_slots: u32, project: Option<u32>, machine: Option<u32>) -> u32 {
    let n = project.or(machine).unwrap_or(if max_slots > 1 {
        max_slots - 1
    } else {
        max_slots
    });
    n.max(1).min(max_slots)
}

/// The cap a project's owners are held to on this machine.
pub fn owner_cap(machine: &MachineConfig, project: &ProjectConfig) -> u32 {
    slot_cap(
        project.max_slots,
        project.max_slots_per_owner,
        machine.max_slots_per_owner,
    )
}

fn object(file: &str, text: &str) -> Result<Map<String, Value>, ConfigError> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(ConfigError(format!("{file}: must be a JSON object"))),
        Err(e) => Err(ConfigError(format!("{file}: {e}"))),
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn cap(raw: &Map<String, Value>, file: &str) -> Result<Option<u32>, ConfigError> {
    match raw.get("maxSlotsPerOwner") {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .or_else(|| {
                v.as_f64()
                    .filter(|n| n.fract() == 0.0 && *n >= 1.0)
                    .map(|n| n as u64)
            })
            .filter(|n| *n >= 1)
            .map(|n| Some(u32::try_from(n).unwrap_or(u32::MAX)))
            .ok_or_else(|| {
                ConfigError(format!(
                    "{file}: \"maxSlotsPerOwner\" must be a whole number, 1 or more"
                ))
            }),
    }
}

fn entries<T>(
    raw: &Map<String, Value>,
    key: &str,
    file: &str,
    read: impl Fn(&Value) -> Option<T>,
) -> Result<Vec<(String, T)>, ConfigError> {
    let Some(v) = raw.get(key) else {
        return Ok(Vec::new());
    };
    let bad = || ConfigError(format!("{file}: \"{key}\" has a value of the wrong kind"));
    v.as_object()
        .ok_or_else(bad)?
        .iter()
        .map(|(k, v)| read(v).map(|t| (k.clone(), t)).ok_or_else(bad))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_cap_leaves_room_for_one_other_owner() {
        assert_eq!(slot_cap(3, None, None), 2);
        assert_eq!(slot_cap(2, None, None), 1);
        assert_eq!(slot_cap(1, None, None), 1);
    }

    #[test]
    fn devlab_json_wins_over_the_machine_and_the_cap_stays_within_one_and_max() {
        assert_eq!(slot_cap(3, Some(3), Some(1)), 3);
        assert_eq!(slot_cap(3, None, Some(1)), 1);
        assert_eq!(slot_cap(3, Some(9), None), 3);
        assert_eq!(slot_cap(4, Some(0), None), 1);
    }

    #[test]
    fn a_project_config_keeps_its_order_and_takes_devlabs_defaults() {
        let p = project_config(
            "/w/devlab.json",
            r#"{"name":"demo","ports":{"B_PORT":2,"A_PORT":1},"supabase":"db","env":{"Z":"${A_PORT}"}}"#,
        )
        .unwrap();
        assert_eq!(p.max_slots, 3);
        assert_eq!(p.slot_stride, 100);
        assert_eq!(
            p.ports,
            vec![("B_PORT".to_owned(), 2), ("A_PORT".to_owned(), 1)]
        );
        assert_eq!(
            p.supabase,
            Some(Supabase {
                dir: "db".to_owned(),
                cli: "bunx supabase".to_owned()
            })
        );
        let object =
            project_config("f", r#"{"name":"demo","supabase":{"cli":"supabase"}}"#).unwrap();
        assert_eq!(
            object.supabase,
            Some(Supabase {
                dir: "supabase".to_owned(),
                cli: "supabase".to_owned()
            })
        );
    }

    #[test]
    fn a_port_that_leaves_the_port_range_in_some_slot_is_refused() {
        let at = |port: u32, stride: u32| {
            project_config(
                "f",
                &format!(
                    r#"{{"name":"demo","maxSlots":2,"slotStride":{stride},"ports":{{"API_PORT":{port}}}}}"#
                ),
            )
        };
        assert_eq!(
            at(65535, 100),
            Err(ConfigError(
                "f: \"ports.API_PORT\" must stay within 1-65535 in every slot (slot 1 would use 65635)"
                    .to_owned()
            ))
        );
        assert!(at(0, 100).is_err());
        assert!(at(65435, 100).is_ok());
        assert!(at(80, 4_294_967_295).is_err());
    }

    #[test]
    fn a_bad_name_or_cap_is_named_as_devlab_named_it() {
        assert_eq!(
            project_config("/w/devlab.json", r#"{"name":"Demo"}"#),
            Err(ConfigError(
                "/w/devlab.json: \"name\" must be lowercase letters, digits and -".to_owned()
            ))
        );
        for cap in ["0", "1.5", "\"2\"", "-1"] {
            let text = format!(r#"{{"name":"demo","maxSlotsPerOwner":{cap}}}"#);
            assert_eq!(
                project_config("f", &text),
                Err(ConfigError(
                    "f: \"maxSlotsPerOwner\" must be a whole number, 1 or more".to_owned()
                )),
                "{cap}"
            );
        }
        assert_eq!(
            machine_config("m", r#"{"maxSlotsPerOwner":0}"#),
            Err(ConfigError(
                "m: \"maxSlotsPerOwner\" must be a whole number, 1 or more".to_owned()
            ))
        );
    }

    #[test]
    fn the_machine_config_overrides_only_what_it_names() {
        let cfg =
            machine_config("m", r#"{"waitSec":30,"ownerNames":["pi"],"hostFloorMB":0}"#).unwrap();
        assert_eq!(cfg.wait_sec, 30.0);
        assert_eq!(cfg.owner_names, vec!["pi".to_owned()]);
        assert_eq!(cfg.host_floor_mb, 0);
        assert_eq!(cfg.stack_ram_mb, 2048);
        assert_eq!(cfg.max_slots_per_owner, None);
    }

    #[test]
    fn the_emulator_and_prep_settings_are_read_with_their_defaults() {
        let cfg = machine_config(
            "m",
            r#"{"maxEmulators":1,"emulatorImage":"img","emulatorRamMB":0,"prep":{"animationsOff":false}}"#,
        )
        .unwrap();
        assert_eq!(
            (
                cfg.max_emulators,
                cfg.emulator_image.as_str(),
                cfg.emulator_ram_mb
            ),
            (1, "img", 0)
        );
        assert_eq!(
            (cfg.emulator_port_base, cfg.emulator_gpu.as_str()),
            (5580, "swiftshader_indirect")
        );
        assert!(cfg.prep_stay_awake && !cfg.prep_animations_off);
        assert_eq!(
            machine_config("m", r#"{"maxEmulators":"two"}"#),
            Err(ConfigError(
                "m: \"maxEmulators\" must be a whole number".to_owned()
            ))
        );
    }
}
