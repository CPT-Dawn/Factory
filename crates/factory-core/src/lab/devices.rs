//! Phones and emulators: what adb says is attached, which devices a request is granted, and how
//! the devices show in status. devlab's device planner, kept apart from adb and the store.

/// Which devices a request takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Phones first, then emulators.
    #[default]
    Any,
    Phone,
    Emulator,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Any => "any",
            Mode::Phone => "phone",
            Mode::Emulator => "emulator",
        }
    }

    /// The mode a wire word names; any other word is [Mode::Any].
    pub fn from_word(word: &str) -> Mode {
        match word {
            "phone" => Mode::Phone,
            "emulator" => Mode::Emulator,
            _ => Mode::Any,
        }
    }
}

/// `phone` or `emulator`, by its serial: adb names an emulator `emulator-<port>`.
pub fn kind_of(serial: &str) -> &'static str {
    match serial.strip_prefix("emulator-") {
        Some(port) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => "emulator",
        _ => "phone",
    }
}

/// A device adb lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub serial: String,
    /// adb's word: `device` (usable), `offline`, `unauthorized`, ...
    pub state: String,
    pub kind: &'static str,
    /// The `model:` property with its underscores as spaces; empty when adb gave none.
    pub model: String,
}

/// The devices in `adb devices -l` text, or in a frame of `adb track-devices -l`: one
/// `serial state prop:value...` line each.
pub fn parse_devices_l(text: &str) -> Vec<Attached> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(serial), Some(state)) = (words.next(), words.next()) else {
            continue;
        };
        let model = words
            .find_map(|w| w.strip_prefix("model:"))
            .map_or_else(String::new, |m| m.replace('_', " "));
        out.push(Attached {
            serial: serial.to_owned(),
            state: state.to_owned(),
            kind: kind_of(serial),
            model,
        });
    }
    out
}

/// The complete frames at the start of `buf`, as `adb track-devices` writes them (four hex
/// digits of length, then that many bytes of device lines), and how many bytes they took.
pub fn parse_track(buf: &[u8]) -> (Vec<Vec<Attached>>, usize) {
    let mut frames = Vec::new();
    let mut at = 0;
    while let Some(head) = buf.get(at..at + 4) {
        let Some(len) = std::str::from_utf8(head)
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
        else {
            break;
        };
        let Some(body) = buf.get(at + 4..at + 4 + len) else {
            break;
        };
        frames.push(parse_devices_l(&String::from_utf8_lossy(body)));
        at += 4 + len;
    }
    (frames, at)
}

/// How long to wait before restart number `restarts` (0 for the first) of the adb device
/// stream: `base` doubled for each restart before it, at most `cap`.
pub fn restart_delay(
    restarts: u32,
    base: std::time::Duration,
    cap: std::time::Duration,
) -> std::time::Duration {
    base.saturating_mul(1u32.checked_shl(restarts).unwrap_or(u32::MAX))
        .min(cap)
}

/// What the planner knows of the board for one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanInput {
    /// Devices still wanted, beyond those the lease holds.
    pub want: u32,
    pub mode: Mode,
    /// Phones attached as `device` and held by nobody, by serial.
    pub free_phones: Vec<String>,
    /// Emulators devlab booted, attached and held by nobody.
    pub idle_emulators: Vec<String>,
    /// Every devlab emulator: booting, idle or held.
    pub emulators_running: u32,
    /// The emulators still booting: their RAM is not taken yet, but it will be.
    pub emulators_booting: u32,
    pub max_emulators: u32,
    pub mem_available_mb: i64,
    pub host_floor_mb: i64,
    pub emulator_ram_mb: i64,
    /// Phones attached but not authorized, to explain a wait.
    pub unauthorized: u32,
    pub busy_phones: u32,
}

/// The devices a request is granted.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub phones: Vec<String>,
    pub reuse: Vec<String>,
    /// New emulators to boot.
    pub boot: u32,
}

/// All or nothing: free phones first, then idle emulators, then new emulators while the
/// emulator cap and the memory floor allow; else why the request waits.
pub fn plan_devices(p: &PlanInput) -> Result<Plan, String> {
    let take = |from: &[String], n: u32| -> Vec<String> {
        from.iter().take(n as usize).cloned().collect()
    };
    let phones = if p.mode == Mode::Emulator {
        Vec::new()
    } else {
        take(&p.free_phones, p.want)
    };
    let mut need = p.want - u32::try_from(phones.len()).unwrap_or(p.want);
    let why = || {
        let mut why = format!("{} phone(s) busy", p.busy_phones);
        if p.unauthorized > 0 {
            why.push_str(&format!(
                ", {} unauthorized (tap Allow on the phone)",
                p.unauthorized
            ));
        }
        why
    };
    if need > 0 && p.mode == Mode::Phone {
        return Err(format!("no free phone: {}", why()));
    }
    let reuse = take(&p.idle_emulators, need);
    need -= u32::try_from(reuse.len()).unwrap_or(need);
    if need > 0 {
        if p.emulators_running + need > p.max_emulators {
            return Err(format!(
                "no free phone ({}) and the emulator cap ({}) is reached",
                why(),
                p.max_emulators
            ));
        }
        let ram = i64::from(need + p.emulators_booting) * p.emulator_ram_mb;
        if p.mem_available_mb - ram < p.host_floor_mb {
            return Err(format!(
                "no free phone ({}) and too little memory for an emulator ({} MB free, need {})",
                why(),
                p.mem_available_mb,
                ram + p.host_floor_mb
            ));
        }
    }
    Ok(Plan {
        phones,
        reuse,
        boot: need,
    })
}

/// A device a lease holds, with the state adb shows it in now (none: not attached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub serial: String,
    pub kind: String,
    pub model: String,
    pub state: Option<String>,
}

/// Why a lease can't be used because a device it holds is gone or not usable; none when every
/// device is attached as `device`. Exit 76 says it.
pub fn lost(held: &[Held]) -> Option<String> {
    let gone: Vec<String> = held
        .iter()
        .filter(|d| d.state.as_deref() != Some("device"))
        .map(|d| {
            let model = if d.model.is_empty() {
                String::new()
            } else {
                format!(" ({})", d.model)
            };
            format!(
                "{} {}{model} is {}",
                d.kind,
                d.serial,
                d.state.as_deref().unwrap_or("unplugged")
            )
        })
        .collect();
    (!gone.is_empty()).then(|| format!("{}: run devlab acquire to replace it", gone.join("; ")))
}

/// The shell line that readies a device for unattended runs: the screen stays on while it has
/// power, animations off when asked, then it is woken and a non-secure keyguard lifted.
pub fn prep_command(stay_awake: bool, animations_off: bool) -> String {
    let mut cmds: Vec<String> = Vec::new();
    if stay_awake {
        cmds.push("settings put global stay_on_while_plugged_in 7".to_owned());
    }
    if animations_off {
        for key in [
            "window_animation_scale",
            "transition_animation_scale",
            "animator_duration_scale",
        ] {
            cmds.push(format!("settings put global {key} 0"));
        }
    }
    cmds.push(WAKE.to_owned());
    cmds.join("; ")
}

/// Wakes a device and lifts a non-secure keyguard.
pub const WAKE: &str = "input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard";

/// The shell line whose output is non-empty while the lock screen shows.
pub const LOCK_PROBE: &str = "dumpsys window | grep -E 'mDreamingLockscreen=true|isKeyguardShowing=true|mKeyguardShowing=true' | head -1";

/// The warning for a device whose [LOCK_PROBE] printed `probe`.
pub fn lock_warning(probe: &str) -> Option<&'static str> {
    (!probe.trim().is_empty())
        .then_some("lock screen still showing: set the phone's screen lock to None or Swipe")
}

/// The warning after a prep: whether adb ran its [prep_command], and what its [LOCK_PROBE]
/// printed (none: adb failed). A device adb reached for neither is not ready.
pub fn prep_warning(prepped: bool, probe: Option<&str>) -> Option<&'static str> {
    match probe {
        None if !prepped => {
            Some("adb could not reach it: plug it in and allow USB debugging, then devlab prep")
        }
        probe => lock_warning(probe.unwrap_or_default()),
    }
}

/// One line of status's device table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub serial: String,
    pub kind: String,
    pub model: String,
    /// `booting`, `ready`, adb's state, or `gone`.
    pub state: String,
    /// `lease N <worktree> (<owner>)`, `idle 3m` or `free`.
    pub holder: String,
}

/// A held device as status sees it: who holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldBy {
    pub serial: String,
    pub kind: String,
    pub model: String,
    /// `lease N <worktree> (<owner>)`.
    pub holder: String,
}

/// A devlab emulator as status sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmulatorView {
    pub serial: String,
    pub booting: bool,
    pub idle_since: Option<i64>,
}

/// Every device adb lists, a lease holds or devlab runs, in that order, once each.
pub fn device_rows(
    attached: &[Attached],
    held: &[HeldBy],
    emulators: &[EmulatorView],
    now: i64,
) -> Vec<DeviceRow> {
    let mut serials: Vec<&str> = Vec::new();
    let serial_lists = [
        attached
            .iter()
            .map(|a| a.serial.as_str())
            .collect::<Vec<_>>(),
        held.iter().map(|h| h.serial.as_str()).collect(),
        emulators.iter().map(|e| e.serial.as_str()).collect(),
    ];
    for s in serial_lists.iter().flatten() {
        if !serials.contains(s) {
            serials.push(s);
        }
    }
    serials
        .into_iter()
        .map(|s| {
            let a = attached.iter().find(|d| d.serial == s);
            let h = held.iter().find(|d| d.serial == s);
            let e = emulators.iter().find(|d| d.serial == s);
            let model = a
                .map(|a| a.model.clone())
                .filter(|m| !m.is_empty())
                .or_else(|| h.map(|h| h.model.clone()))
                .unwrap_or_default();
            let state = if e.is_some_and(|e| e.booting) {
                "booting".to_owned()
            } else {
                match a {
                    Some(a) if a.state == "device" => "ready".to_owned(),
                    Some(a) => a.state.clone(),
                    None => "gone".to_owned(),
                }
            };
            let holder = match (h, e.and_then(|e| e.idle_since)) {
                (Some(h), _) => h.holder.clone(),
                (None, Some(since)) => format!("idle {}", super::ago(now, since)),
                (None, None) => "free".to_owned(),
            };
            DeviceRow {
                serial: s.to_owned(),
                kind: a
                    .map(|a| a.kind.to_owned())
                    .or_else(|| h.map(|h| h.kind.clone()))
                    .unwrap_or_else(|| "emulator".to_owned()),
                model,
                state,
                holder,
            }
        })
        .collect()
}

/// The status line of one device row.
pub fn device_line(r: &DeviceRow) -> String {
    let model: String = if r.model.is_empty() {
        "-".to_owned()
    } else {
        r.model.chars().take(14).collect()
    };
    format!(
        "  {:<18} {:<8} {:<14} {:<12} {}",
        r.serial, r.kind, model, r.state, r.holder
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PlanInput {
        PlanInput {
            want: 1,
            max_emulators: 2,
            mem_available_mb: 32000,
            host_floor_mb: 5120,
            emulator_ram_mb: 3072,
            ..PlanInput::default()
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn phones_go_first_then_idle_emulators_then_a_boot() {
        let plan = |p: PlanInput| plan_devices(&p).unwrap();
        assert_eq!(
            plan(PlanInput {
                free_phones: s(&["P1"]),
                idle_emulators: s(&["emulator-5580"]),
                ..base()
            }),
            Plan {
                phones: s(&["P1"]),
                reuse: vec![],
                boot: 0
            }
        );
        assert_eq!(
            plan(PlanInput {
                idle_emulators: s(&["emulator-5580"]),
                emulators_running: 1,
                ..base()
            }),
            Plan {
                phones: vec![],
                reuse: s(&["emulator-5580"]),
                boot: 0
            }
        );
        assert_eq!(
            plan(base()),
            Plan {
                phones: vec![],
                reuse: vec![],
                boot: 1
            }
        );
        assert_eq!(
            plan(PlanInput {
                want: 2,
                free_phones: s(&["P1"]),
                ..base()
            }),
            Plan {
                phones: s(&["P1"]),
                reuse: vec![],
                boot: 1
            }
        );
        assert_eq!(plan(PlanInput { want: 0, ..base() }), Plan::default());
    }

    #[test]
    fn a_mode_keeps_to_its_kind() {
        assert_eq!(
            plan_devices(&PlanInput {
                mode: Mode::Emulator,
                free_phones: s(&["P1"]),
                ..base()
            })
            .unwrap()
            .boot,
            1
        );
        assert_eq!(
            plan_devices(&PlanInput {
                mode: Mode::Phone,
                busy_phones: 2,
                unauthorized: 1,
                ..base()
            }),
            Err("no free phone: 2 phone(s) busy, 1 unauthorized (tap Allow on the phone)".into())
        );
    }

    #[test]
    fn a_pair_is_all_or_nothing_within_the_cap_and_the_memory_floor() {
        assert_eq!(
            plan_devices(&PlanInput {
                want: 2,
                emulators_running: 1,
                busy_phones: 3,
                ..base()
            }),
            Err("no free phone (3 phone(s) busy) and the emulator cap (2) is reached".into())
        );
        assert_eq!(
            plan_devices(&PlanInput {
                mem_available_mb: 7000,
                ..base()
            }),
            Err("no free phone (0 phone(s) busy) and too little memory for an emulator (7000 MB free, need 8192)".into())
        );
    }

    #[test]
    fn a_device_adb_reaches_for_nothing_is_not_called_ready() {
        let locked = "isKeyguardShowing=true";
        assert_eq!(prep_warning(true, Some("")), None);
        assert_eq!(prep_warning(false, Some("")), None);
        assert_eq!(prep_warning(true, None), None);
        assert_eq!(prep_warning(true, Some(locked)), lock_warning(locked));
        assert_eq!(
            prep_warning(false, None),
            Some("adb could not reach it: plug it in and allow USB debugging, then devlab prep")
        );
    }

    #[test]
    fn the_memory_floor_counts_the_emulators_still_booting() {
        let ask = |emulators_booting| PlanInput {
            want: 1,
            mem_available_mb: 12000,
            emulators_running: emulators_booting,
            max_emulators: 4,
            emulators_booting,
            ..base()
        };
        assert_eq!(plan_devices(&ask(0)).map(|p| p.boot), Ok(1));
        assert_eq!(
            plan_devices(&ask(2)),
            Err("no free phone (0 phone(s) busy) and too little memory for an emulator (12000 MB free, need 14336)".into())
        );
    }

    #[test]
    fn device_lines_carry_their_model_and_state() {
        let text = "List of devices attached\nFAKEPHONE01            device usb:1-1 product:shiba model:Pixel_8 device:shiba\nemulator-5580          offline\n\n";
        assert_eq!(
            parse_devices_l(text),
            vec![
                Attached {
                    serial: "FAKEPHONE01".into(),
                    state: "device".into(),
                    kind: "phone",
                    model: "Pixel 8".into()
                },
                Attached {
                    serial: "emulator-5580".into(),
                    state: "offline".into(),
                    kind: "emulator",
                    model: String::new()
                },
            ]
        );
        assert_eq!(kind_of("emulator-x"), "phone");
    }

    #[test]
    fn track_frames_include_an_empty_list_and_leave_a_split_frame() {
        let buf = b"0013FAKEPHONE01\tdevice\n00000013FAKE";
        let (frames, used) = parse_track(buf);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0][0].serial, "FAKEPHONE01");
        assert!(frames[1].is_empty());
        assert_eq!(used, 27);
    }

    #[test]
    fn a_held_device_not_attached_as_device_is_lost() {
        let held = |state: Option<&str>| Held {
            serial: "FAKEPHONE01".into(),
            kind: "phone".into(),
            model: "Pixel 8".into(),
            state: state.map(str::to_owned),
        };
        assert_eq!(lost(&[held(Some("device"))]), None);
        assert_eq!(
            lost(&[held(None)]).unwrap(),
            "phone FAKEPHONE01 (Pixel 8) is unplugged: run devlab acquire to replace it"
        );
        assert_eq!(
            lost(&[held(Some("offline"))]).unwrap(),
            "phone FAKEPHONE01 (Pixel 8) is offline: run devlab acquire to replace it"
        );
    }

    #[test]
    fn rows_join_what_adb_lists_what_leases_hold_and_what_devlab_runs() {
        let attached = parse_devices_l(
            "FAKEPHONE01 offline usb:1-1\nemulator-5580 device model:sdk_gphone64_x86_64\n",
        );
        let held = vec![HeldBy {
            serial: "FAKEPHONE01".into(),
            kind: "phone".into(),
            model: "Pixel 8".into(),
            holder: "lease 1 plain-a (claude:9)".into(),
        }];
        let emus = vec![
            EmulatorView {
                serial: "emulator-5580".into(),
                booting: false,
                idle_since: Some(1000),
            },
            EmulatorView {
                serial: "emulator-5582".into(),
                booting: true,
                idle_since: None,
            },
        ];
        let lines: Vec<String> = device_rows(&attached, &held, &emus, 1000)
            .iter()
            .map(device_line)
            .collect();
        assert_eq!(
            lines,
            vec![
                "  FAKEPHONE01        phone    Pixel 8        offline      lease 1 plain-a (claude:9)",
                "  emulator-5580      emulator sdk gphone64 x ready        idle 0m",
                "  emulator-5582      emulator -              booting      free",
            ]
        );
    }

    #[test]
    fn prep_lifts_the_keyguard_and_reads_the_lock_screen() {
        assert_eq!(
            prep_command(true, false),
            "settings put global stay_on_while_plugged_in 7; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard"
        );
        assert_eq!(lock_warning("\n"), None);
        assert!(lock_warning("  mDreamingLockscreen=true").is_some());
    }
}
