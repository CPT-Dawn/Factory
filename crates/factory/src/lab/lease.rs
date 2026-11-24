//! The caller's side of a lease: what it asks the daemon for, and what it does once granted.

use std::time::Duration;

use factory_adapters::adb::{Adb, AdbCli};
use factory_core::exit::Exit;
use factory_core::lab::args::{Opts, wait_span};
use factory_core::lab::base_name;
use factory_core::lab::config::{MachineConfig, ProjectConfig, devices_per_owner, owner_cap};
use factory_core::lab::devices::Mode;
use factory_core::lab::emulator::BOOT_TIMEOUT_SEC;
use factory_core::lab::slots::{Env, slot_env};

use super::context::Ctx;
use super::host::proc;
use super::stack;
use super::wire::{
    ACQUIRE, AcquireParams, DeviceAsk, EXEC, EXEC_END, ExecEnd, ExecParams, GC, Granted, LEASE,
    Lease, Process, ProjectAsk, RECEIVED, RELEASE, ReleaseParams, Released, RunAsk, SLOT, STOPPING,
    SlotParams, SlotSchema, StackAsk, StopParams, Stopping,
};
use crate::client;
use crate::output::Failure;
use crate::paths::Paths;

/// How much longer than its wait the CLI listens for the daemon's answer.
pub const ANSWER_GRACE: Duration = Duration::from_secs(60);
/// How much longer an answer may take when the daemon may boot emulators for it: making their
/// AVDs, starting their units and booting them, then stopping them when that fails.
const BOOT_GRACE: Duration = Duration::from_secs(BOOT_TIMEOUT_SEC + 300);

pub fn paths() -> Result<Paths, Failure> {
    Paths::from_env()
}

/// Queues for a lease and takes it: the daemon grants its devices (booting emulators as needed)
/// and readies them, then the slot's workdir is prepared and, with `--stack`, its stack
/// started, and the project's ports reversed onto each device. A stack that fails to start keeps
/// the lease; an emulator that fails to boot (exit 77) gives the whole lease back. Each new reason it waits goes
/// to `waiting`, a warning about the slot to `warn`.
pub fn acquire(
    ctx: &Ctx,
    opts: &Opts,
    waiting: &mut dyn FnMut(&str),
    warn: &mut dyn FnMut(&str),
) -> Result<Lease, Failure> {
    let (params, listen) = ask(ctx, opts, None)?;
    let ask_at = |resume| AcquireParams {
        resume,
        ..params.clone()
    };
    // A daemon that comes back says why it waits again: say it once.
    let mut said = String::new();
    let granted: Granted =
        client::call_queued(&paths()?, ACQUIRE, RECEIVED, ask_at, listen, |why| {
            if why != said {
                waiting(why);
                said = why.to_owned();
            }
        })?;
    take(ctx, opts, granted, warn)
}

/// The acquire to send for `opts` (for `run`, if any) and how long to listen for its answer;
/// none listens as long as it takes.
pub fn ask(
    ctx: &Ctx,
    opts: &Opts,
    run: Option<RunAsk>,
) -> Result<(AcquireParams, Option<Duration>), Failure> {
    if opts.want == 0 && ctx.project.is_none() {
        return Err(Failure::usage(format!(
            "no devlab.json in {}: nothing to lease without a device",
            ctx.worktree
        )));
    }
    let project = match &ctx.project {
        Some(p) => Some(project_ask(ctx, p)?),
        None => None,
    };
    let wait_sec = opts.wait_sec.unwrap_or(ctx.cfg.wait_sec);
    let params = AcquireParams {
        worktree: ctx.worktree.clone(),
        owner: ctx.owner.clone(),
        lease: super::context::forced_lease(),
        project: project.clone(),
        wait_sec,
        devices: (opts.want > 0).then(|| device_ask(&ctx.cfg, ctx.project.as_ref(), opts)),
        resume: None,
        client: Some(me()),
        run,
        reservation: super::context::reserved(),
    };
    let listen = wait_span(wait_sec)
        .checked_add(ANSWER_GRACE)
        .and_then(|wait| {
            if opts.want > 0 && opts.mode != Mode::Phone {
                wait.checked_add(BOOT_GRACE)
            } else {
                Some(wait)
            }
        });
    Ok((params, listen))
}

/// Takes a granted lease: prepares the slot's workdir, with `--stack` starts its stack, and
/// reverses the project's ports onto each device. A warning about the slot goes to `warn`.
pub fn take(
    ctx: &Ctx,
    opts: &Opts,
    granted: Granted,
    warn: &mut dyn FnMut(&str),
) -> Result<Lease, Failure> {
    let lease = granted.lease;
    if let (Some(p), Some(slot)) = (&ctx.project, lease.slot) {
        stack::prepare_workdir(p, &ctx.worktree, slot)?;
        let info = stack::info(p, &ctx.worktree, slot)?;
        // A slot with no record of its schema (a stack devlab never ran, or one an earlier
        // daemon left) is taken to run its last worktree's while its stack runs.
        let schema = match (granted.schema, &info) {
            (Some(schema), Some(_)) => Some(schema),
            (None, Some(info)) => granted
                .previous
                .filter(|previous| *previous != ctx.worktree && stack::running(&info.project_id))
                .map(|worktree| SlotSchema {
                    worktree,
                    head: None,
                }),
            (_, None) => None,
        };
        if let Some(schema) = schema.filter(|s| s.worktree != ctx.worktree) {
            let head = schema
                .head
                .map_or_else(String::new, |head| format!(", up to {head}"));
            warn(&format!(
                "slot {slot}'s stack last ran {}'s schema{head}: reset it if your migrations differ",
                base_name(&schema.worktree)
            ));
        }
        if opts.stack && info.is_some() {
            let env = lease_env(&lease, Some(p))?;
            if stack::up(p, &ctx.worktree, slot, &env, &ctx.cfg)? == stack::Up::Started {
                stack_ours(&p.name, slot, true)?;
            }
        }
    }
    apply_reverses(&lease, ctx.project.as_ref(), warn)?;
    Ok(lease)
}

fn device_ask(cfg: &MachineConfig, project: Option<&ProjectConfig>, opts: &Opts) -> DeviceAsk {
    DeviceAsk {
        want: opts.want,
        mode: opts.mode.as_str().to_owned(),
        max_emulators: cfg.max_emulators,
        emulator_ram_mb: cfg.emulator_ram_mb,
        host_floor_mb: cfg.host_floor_mb,
        port_base: cfg.emulator_port_base,
        image: cfg.emulator_image.clone(),
        device: cfg.emulator_device.clone(),
        gpu: cfg.emulator_gpu.clone(),
        stay_awake: cfg.prep_stay_awake,
        animations_off: cfg.prep_animations_off,
        max_per_owner: devices_per_owner(cfg, project),
    }
}

/// Maps the project's `reverse` ports onto each device the lease holds: a device port to a
/// port, or to the port an export names.
pub fn apply_reverses(
    lease: &Lease,
    project: Option<&ProjectConfig>,
    warn: &mut dyn FnMut(&str),
) -> Result<(), Failure> {
    let Some(p) = project.filter(|p| !p.reverse.is_empty() && !lease.devices.is_empty()) else {
        return Ok(());
    };
    let ports = reverse_ports(p, &lease_env(lease, Some(p))?);
    for d in &lease.devices {
        for &(on_device, on_host) in &ports {
            if on_device > 0 && on_host > 0 && !AdbCli.reverse(&d.serial, on_device, on_host) {
                warn(&format!(
                    "adb reverse tcp:{on_device} -> {on_host} failed on {}",
                    d.serial
                ));
            }
        }
    }
    Ok(())
}

/// Wakes each device the lease holds and lifts a non-secure keyguard.
pub fn wake(lease: &Lease) {
    for d in &lease.devices {
        let _ = AdbCli.shell(&d.serial, factory_core::lab::devices::WAKE);
    }
}

/// The project as a grant needs it: its cap here, its ports and its stack's.
pub fn project_ask(ctx: &Ctx, p: &ProjectConfig) -> Result<ProjectAsk, Failure> {
    let stack = stack::info(p, &ctx.worktree, 0)?.map(|info| StackAsk {
        project_id: info.project_id,
        ports: info.ports.iter().map(|(_, port)| *port).collect(),
    });
    Ok(ProjectAsk {
        name: p.name.clone(),
        max_slots: p.max_slots,
        stride: p.slot_stride,
        cap: owner_cap(&ctx.cfg, p),
        ports: p.ports.iter().map(|(_, port)| *port).collect(),
        stack,
    })
}

/// The caller's lease, if it holds one.
pub fn find(ctx: &Ctx) -> Result<Option<Lease>, Failure> {
    client::call(&paths()?, LEASE, &ctx.find())
}

/// The caller's lease with every device it holds attached; else exit 4, or 76 naming the
/// devices that are gone.
pub fn live(ctx: &Ctx) -> Result<Lease, Failure> {
    usable(ctx, find(ctx)?)
}

/// The caller's lease for `devlab exec` of `argv`, as [`live`] finds it. The daemon counts
/// the exec as running on it, so no waiter takes its devices, until [`exec_ended`].
pub fn exec_lease(ctx: &Ctx, argv: &[String]) -> Result<Lease, Failure> {
    let params = ExecParams {
        find: ctx.find(),
        client: me(),
        command: argv.join(" "),
    };
    usable(ctx, client::call(&paths()?, EXEC, &params)?)
}

/// The exec [`exec_lease`] counted ended; its lease is touched. A daemon that went away
/// forgets the exec once this process exits.
pub fn exec_ended() {
    if let Ok(paths) = paths() {
        let _: Result<(), Failure> = client::call(&paths, EXEC_END, &ExecEnd { client: me() });
    }
}

/// `lease`, when it is there and every device it holds is attached; else exit 4, or 76
/// naming the devices that are gone.
fn usable(ctx: &Ctx, lease: Option<Lease>) -> Result<Lease, Failure> {
    let lease = lease.ok_or_else(|| {
        Failure::new(
            Exit::NoLease,
            format!(
                "no lease for {} ({}): run devlab acquire",
                ctx.worktree, ctx.owner.label
            ),
        )
    })?;
    match lease.lost() {
        Some(why) => Err(Failure::new(Exit::DeviceLost, why)),
        None => Ok(lease),
    }
}

/// Which leases a release ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// This worktree's lease, or the one `DEVLAB_LEASE` names.
    Here,
    /// The lease with this id, from any folder.
    Lease(i64),
    /// Every lease of the calling owner.
    AllMine,
}

/// Drops the leases `target` names, and lets go of any slot this call held while stopping its
/// stack; what it dropped and the caller's leases still held. A lease named by id that another
/// owner holds is refused, exit 4.
pub fn release(ctx: &Ctx, target: Target) -> Result<Released, Failure> {
    let mut find = ctx.find();
    if let Target::Lease(id) = target {
        find.lease = Some(id);
    }
    client::call(
        &paths()?,
        RELEASE,
        &ReleaseParams {
            find,
            stopper: Some(me()),
            all: target == Target::AllMine,
        },
    )
}

/// The caller's lease, its slot held for this call until its [`release`] while it stops
/// the slot's stack, and the stack it was granted.
pub fn stopping(ctx: &Ctx) -> Result<Option<Stopping>, Failure> {
    client::call(
        &paths()?,
        STOPPING,
        &StopParams {
            find: ctx.find(),
            stopper: me(),
        },
    )
}

/// This devlab call, as a stopper or a waiter.
pub(super) fn me() -> Process {
    let pid = i64::from(std::process::id());
    Process {
        pid,
        start: proc(pid).map(|p| p.start).unwrap_or_default(),
    }
}

/// Drops the leases of owners that exited; a line for each.
pub fn gc() -> Result<Vec<String>, Failure> {
    client::call(&paths()?, GC, &())
}

pub fn stack_ours(project: &str, slot: u32, ours: bool) -> Result<(), Failure> {
    client::call(
        &paths()?,
        SLOT,
        &SlotParams {
            project: project.to_owned(),
            slot,
            stack_ours: ours,
        },
    )
}

/// The project's reverses as (device port, host port), a port named by a variable read from
/// `env`, a lease's; 0 for a port that names nothing.
pub fn reverse_ports(p: &ProjectConfig, env: &Env) -> Vec<(u32, u32)> {
    let port = |x: &str| -> u32 {
        let named = env
            .iter()
            .find(|(k, _)| k == x)
            .map_or("", |(_, v)| v.as_str());
        let text = if !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()) {
            x
        } else {
            named
        };
        text.parse().unwrap_or(0)
    };
    p.reverse
        .iter()
        .map(|(on_device, on_host)| (port(on_device), port(on_host)))
        .collect()
}

/// The lease's exports: its slot's ports and stack, then `DEVLAB_LEASE`, then its devices:
/// `ANDROID_SERIAL` (the first) and the space-separated `DEVLAB_SERIALS`, `DEVLAB_KINDS` and
/// `DEVLAB_ABIS`.
pub fn lease_env(lease: &Lease, project: Option<&ProjectConfig>) -> Result<Env, Failure> {
    let mut env = Env::new();
    if let (Some(p), Some(slot)) = (project, lease.slot) {
        let info = stack::info(p, &lease.worktree, slot)?;
        env = slot_env(p, slot, info.as_ref());
    }
    env.push(("DEVLAB_LEASE".to_owned(), lease.id.to_string()));
    if let Some(first) = lease.devices.first() {
        let join = |part: &dyn Fn(&super::wire::LeaseDevice) -> String| {
            lease.devices.iter().map(part).collect::<Vec<_>>().join(" ")
        };
        env.push(("ANDROID_SERIAL".to_owned(), first.serial.clone()));
        env.push(("DEVLAB_SERIALS".to_owned(), join(&|d| d.serial.clone())));
        env.push(("DEVLAB_KINDS".to_owned(), join(&|d| d.kind.clone())));
        env.push((
            "DEVLAB_ABIS".to_owned(),
            join(&|d| d.abi.clone().unwrap_or_else(|| "unknown".to_owned())),
        ));
    }
    Ok(env)
}

/// `lease 3, slot 0 (stack http://127.0.0.1:27321, down)`.
pub fn summary(lease: &Lease, project: Option<&ProjectConfig>) -> Result<String, Failure> {
    let env = lease_env(lease, project)?;
    let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let mut parts = vec![format!("lease {}", lease.id)];
    if let Some(slot) = lease.slot {
        let stack = get("SUPABASE_URL").map_or_else(String::new, |url| {
            let up =
                get("SUPABASE_PROJECT_ID").is_some_and(|id| !id.is_empty() && stack::running(id));
            format!(" (stack {url}, {})", if up { "up" } else { "down" })
        });
        parts.push(format!("slot {slot}{stack}"));
    }
    for d in &lease.devices {
        let model = if d.kind == "phone" && !d.model.is_empty() {
            format!(" ({})", d.model)
        } else {
            String::new()
        };
        parts.push(format!("{} {}{model}", d.kind, d.serial));
    }
    Ok(parts.join(", "))
}
