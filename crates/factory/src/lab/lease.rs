//! The caller's side of a lease: what it asks the daemon for, and what it does once granted.

use std::path::Path;
use std::time::Duration;

use factory_core::exit::Exit;
use factory_core::lab::args::Opts;
use factory_core::lab::config::{ProjectConfig, owner_cap};
use factory_core::lab::slots::{Env, slot_env};

use super::cli::note;
use super::context::Ctx;
use super::host::proc;
use super::stack;
use super::wire::{
    ACQUIRE, AcquireParams, GC, Granted, LEASE, Lease, Process, ProjectAsk, RELEASE, ReleaseParams,
    SLOT, STOPPING, SlotParams, StackAsk, StopParams,
};
use crate::client;
use crate::output::Failure;
use crate::paths::Paths;

/// How much longer than its wait the CLI listens for the daemon's answer.
const ANSWER_GRACE: Duration = Duration::from_secs(60);

fn paths() -> Result<Paths, Failure> {
    Paths::from_env()
}

pub fn base_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Queues for a lease and takes it: the slot's workdir is prepared and, with `--stack`, its
/// stack started. A stack that fails to start keeps the lease.
pub fn acquire(ctx: &Ctx, opts: &Opts) -> Result<Lease, Failure> {
    if opts.want == 0 && ctx.project.is_none() {
        return Err(Failure::usage(format!(
            "no devlab.json in {}: nothing to lease without a device",
            ctx.worktree
        )));
    }
    if opts.want > 0 {
        return Err(Failure::usage(
            "factory lab leases no devices yet (#16): pass --no-device for a stack slot",
        ));
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
        project,
        wait_sec,
    };
    let listen = Duration::try_from_secs_f64(wait_sec)
        .ok()
        .and_then(|wait| wait.checked_add(ANSWER_GRACE));
    let granted: Granted = client::call_streaming(&paths()?, ACQUIRE, &params, listen, note)?;
    let lease = granted.lease;
    if let (Some(p), Some(slot)) = (&ctx.project, lease.slot) {
        stack::prepare_workdir(p, &ctx.worktree, slot)?;
        let info = stack::info(p, &ctx.worktree, slot)?;
        if let (Some(previous), Some(info)) = (&granted.previous, &info)
            && *previous != ctx.worktree
            && stack::running(&info.project_id)
        {
            note(&format!(
                "slot {slot}'s stack last ran {}'s schema: reset it if your migrations differ",
                base_name(previous)
            ));
        }
        if opts.stack && info.is_some() {
            let env = lease_env(&lease, Some(p))?;
            if stack::up(p, &ctx.worktree, slot, &env, &ctx.cfg)? == stack::Up::Started {
                stack_ours(&p.name, slot, true)?;
            }
        }
    }
    Ok(lease)
}

/// The project as a grant needs it: its cap here, its ports and its stack's.
fn project_ask(ctx: &Ctx, p: &ProjectConfig) -> Result<ProjectAsk, Failure> {
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

/// The caller's lease, or exit 4.
pub fn live(ctx: &Ctx) -> Result<Lease, Failure> {
    find(ctx)?.ok_or_else(|| {
        Failure::new(
            Exit::NoLease,
            format!(
                "no lease for {} ({}): run devlab acquire",
                ctx.worktree, ctx.owner.label
            ),
        )
    })
}

/// Drops the caller's lease, and lets go of any slot this call held while stopping its stack.
pub fn release(ctx: &Ctx) -> Result<Option<Lease>, Failure> {
    client::call(
        &paths()?,
        RELEASE,
        &ReleaseParams {
            find: ctx.find(),
            stopper: Some(me()),
        },
    )
}

/// The caller's lease, its slot held for this call until its [`release`] while it stops
/// the slot's stack.
pub fn stopping(ctx: &Ctx) -> Result<Option<Lease>, Failure> {
    client::call(
        &paths()?,
        STOPPING,
        &StopParams {
            find: ctx.find(),
            stopper: me(),
        },
    )
}

/// This devlab call, as a stopper.
fn me() -> Process {
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

/// The lease's exports: its slot's ports and stack, then `DEVLAB_LEASE`.
pub fn lease_env(lease: &Lease, project: Option<&ProjectConfig>) -> Result<Env, Failure> {
    let mut env = Env::new();
    if let (Some(p), Some(slot)) = (project, lease.slot) {
        let info = stack::info(p, &lease.worktree, slot)?;
        env = slot_env(p, slot, info.as_ref());
    }
    env.push(("DEVLAB_LEASE".to_owned(), lease.id.to_string()));
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
    Ok(parts.join(", "))
}
