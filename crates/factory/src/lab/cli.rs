//! devlab's command line: its commands and options, its help and how it fails.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use factory_core::lab::slots::export_lines;

use super::context::{Ctx, context, project_config, release_context};
use factory_core::lab::args::parse_opts;

use super::lease::{self, Target};
use super::wire::{Lease, Released};
use super::{android, reserve, run, stack, status};
use crate::output::Failure;
use factory_adapters::adb::AdbCli;
use factory_core::exit::Exit;

pub(super) const HELP: &str = include_str!("help.txt");

/// Runs one devlab command; `args` come after the name (`devlab` or `factory lab`).
pub fn main(args: &[OsString]) -> ExitCode {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    match dispatch(&args) {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            note(&failure.message);
            ExitCode::from(failure.exit.code())
        }
    }
}

/// One line on stderr, as devlab says everything that isn't an export line.
pub fn note(message: &str) {
    let _ = writeln!(std::io::stderr(), "devlab: {message}");
}

fn dispatch(argv: &[String]) -> Result<u8, Failure> {
    let (cmd, args) = match argv.split_first() {
        Some((cmd, args)) => (cmd.as_str(), args),
        None => ("help", &[][..]),
    };
    match cmd {
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(0)
        }
        "status" => {
            println!("{}", status::render(args.iter().any(|a| a == "--json"))?);
            Ok(0)
        }
        "acquire" | "run" => {
            let (mut opts, rest, flags) = parse_opts(args).map_err(Failure::usage)?;
            let ctx = context()?;
            if opts.wait_sec.is_none() {
                opts.wait_sec = Some(ctx.cfg.wait_sec);
            }
            let quiet = flags.iter().any(|f| f == "--quiet");
            if cmd == "run" {
                let keep = flags.iter().any(|f| f == "--keep");
                return run::run(&ctx, &opts, quiet, keep, rest);
            }
            let granted = lease::acquire(&ctx, &opts, &mut |why| note(why), &mut |w| note(w))?;
            if !quiet {
                note(&lease::summary(&granted, ctx.project.as_ref())?);
            }
            println!(
                "{}",
                export_lines(&lease::lease_env(&granted, ctx.project.as_ref())?)
            );
            Ok(0)
        }
        "reserve" => reserve::main(args),
        "exec" => {
            let ctx = context()?;
            let rest = match args.split_first() {
                Some((dashes, rest)) if dashes == "--" => rest,
                _ => args,
            };
            if rest.is_empty() {
                return Err(Failure::usage("devlab exec -- <cmd...>"));
            }
            let held = lease::live(&ctx)?;
            run::exec_in(&ctx, &held, rest, None)
        }
        "env" => {
            let ctx = context()?;
            let held = lease::live(&ctx)?;
            println!(
                "{}",
                export_lines(&lease::lease_env(&held, ctx.project.as_ref())?)
            );
            Ok(0)
        }
        "release" => {
            let (target, stop_stack) = release_args(args)?;
            let (released, stop_failed) = release(&release_context()?, stop_stack, target)?;
            for line in release_lines(&released, target)? {
                note(&line);
            }
            stop_failed.map_or(Ok(0), Err)
        }
        "stack" => {
            let done = stack(&context()?, args.first().map_or("status", String::as_str))?;
            if let Stack::State { .. } = done {
                println!("{}", done.devlab_line());
            } else {
                note(&done.devlab_line());
            }
            Ok(0)
        }
        "gc" => {
            for freed in lease::gc()? {
                println!("released {freed}");
            }
            Ok(0)
        }
        "prep" => {
            for (serial, warning) in prep(&context()?, args)? {
                println!("{serial}: {}", warning.unwrap_or("ready"));
            }
            Ok(0)
        }
        "watch" => {
            println!("devlab watch: {WATCH}");
            Ok(0)
        }
        other => Err(Failure::usage(format!(
            "unknown command {other}; devlab help"
        ))),
    }
}

/// What `watch` says: there is nothing left for it to start.
pub(super) const WATCH: &str =
    "the factory daemon frees leases and stops idle emulators and stacks now; nothing to start";

/// Readies the phones named in `serials`, or every ready device when none is named: each serial,
/// and the warning it left, if any.
pub(super) fn prep(
    ctx: &Ctx,
    serials: &[String],
) -> Result<Vec<(String, Option<&'static str>)>, Failure> {
    let serials = if serials.is_empty() {
        status::ready_serials()?
    } else {
        serials.to_vec()
    };
    Ok(serials
        .into_iter()
        .map(|serial| {
            let warning = android::prep(
                &AdbCli,
                &serial,
                ctx.cfg.prep_stay_awake,
                ctx.cfg.prep_animations_off,
            );
            (serial, warning)
        })
        .collect())
}

/// What `release` ends, from its arguments: `--lease <id>`, `--all-mine`, else this worktree's
/// lease; and whether `--stop-stack` was given, which only goes with the lease here.
pub(super) fn release_args(args: &[String]) -> Result<(Target, bool), Failure> {
    let mut target = Target::Here;
    let mut stop_stack = false;
    let mut at = 0;
    while at < args.len() {
        let arg = args[at].as_str();
        let named = match arg {
            "--stop-stack" => {
                stop_stack = true;
                None
            }
            "--all-mine" => Some(Target::AllMine),
            "--lease" => {
                at += 1;
                Some(lease_id(args.get(at).map_or("", String::as_str))?)
            }
            _ => arg.strip_prefix("--lease=").map(lease_id).transpose()?,
        };
        if let Some(named) = named {
            if target != Target::Here {
                return Err(Failure::usage(
                    "devlab release takes one of --lease <id> and --all-mine",
                ));
            }
            target = named;
        }
        at += 1;
    }
    if stop_stack && target != Target::Here {
        return Err(Failure::usage(
            "--stop-stack stops this worktree's stack: run it in the worktree, without --lease or --all-mine",
        ));
    }
    Ok((target, stop_stack))
}

fn lease_id(text: &str) -> Result<Target, Failure> {
    text.trim()
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .map(Target::Lease)
        .ok_or_else(|| Failure::usage(format!("--lease takes a lease id, not \"{text}\"")))
}

/// What a release says, a line each: the leases it ended, or that there were none (a lease named
/// by id that isn't held is exit 4), then each lease of the caller still held with the command
/// that ends it.
pub(super) fn release_lines(released: &Released, target: Target) -> Result<Vec<String>, Failure> {
    let mut lines: Vec<String> = released
        .dropped
        .iter()
        .map(|l| format!("released lease {} ({})", l.id, l.worktree))
        .collect();
    if lines.is_empty() {
        lines.push(
            match target {
                Target::Here => "nothing held here",
                Target::AllMine => "nothing held",
                Target::Lease(id) => {
                    return Err(Failure::new(
                        Exit::NoLease,
                        format!("no lease {id}: see devlab status for the leases held"),
                    ));
                }
            }
            .to_owned(),
        );
    }
    lines.extend(released.still_held.iter().map(still_held));
    Ok(lines)
}

/// `still held: lease 1, /path, slot 0, phone SERIAL (Pixel 8): end it with devlab release --lease 1`.
pub(super) fn still_held(lease: &Lease) -> String {
    let mut parts = vec![format!("lease {}", lease.id), lease.worktree.clone()];
    if let Some(slot) = lease.slot {
        parts.push(format!("slot {slot}"));
    }
    for d in &lease.devices {
        let model = if d.model.is_empty() {
            String::new()
        } else {
            format!(" ({})", d.model)
        };
        parts.push(format!("{} {}{model}", d.kind, d.serial));
    }
    format!(
        "still held: {}: end it with devlab release --lease {}",
        parts.join(", "),
        lease.id
    )
}

/// Gives the leases `target` names back. With `--stop-stack` the
/// stack stops first, its slot held for this call meanwhile, whatever ends the lease, so the next
/// in the queue never gets a stack being torn down; a stop that fails still gives the lease back.
///
/// A stack that fails to stop does not undo the release: the failure comes back beside what was
/// released, for the caller to say after it. The stack stopped is the held lease's own, by its
/// worktree, even when `DEVLAB_LEASE` named a lease of another worktree.
pub(super) fn release(
    ctx: &Ctx,
    stop_stack: bool,
    target: Target,
) -> Result<(Released, Option<Failure>), Failure> {
    let held = if stop_stack {
        lease::stopping(ctx)?
    } else {
        None
    };
    let stopped = match &held {
        Some(Lease {
            project: Some(project),
            slot: Some(slot),
            worktree,
            ..
        }) => Some(stop(project, worktree, *slot)),
        _ => None,
    };
    let released = lease::release(ctx, target)?;
    let stop_failed = stopped
        .filter(|_| !released.dropped.is_empty())
        .and_then(Result::err);
    Ok((released, stop_failed))
}

/// Stops the slot's stack of `held`, the lease's project, at `worktree` if it runs, and marks it
/// no longer devlab's to stop. A `devlab.json` there that now names another project is refused:
/// its stack on that slot is not the lease's to stop.
fn stop(held: &str, worktree: &str, slot: u32) -> Result<(), Failure> {
    let Some(project) = project_config(worktree)? else {
        return Ok(());
    };
    if project.name != held {
        return Err(Failure::precondition(format!(
            "{worktree}/devlab.json names project {}, not {held} the lease held slot {slot} for: \
             no stack stopped; stop {held}'s with its own devlab.json",
            project.name
        )));
    }
    if let Some(info) = stack::info(&project, worktree, slot)?
        && stack::running(&info.project_id)
    {
        stack::down(&project, &info.project_id, worktree)?;
    }
    lease::stack_ours(&project.name, slot, false)
}

/// What `stack up`, `down` or `status` did to the lease's stack, by its project id.
pub(super) enum Stack {
    Up {
        id: String,
        slot: u32,
        how: stack::Up,
    },
    Down {
        id: String,
        slot: u32,
    },
    State {
        id: String,
        slot: u32,
        up: bool,
    },
}

impl Stack {
    /// How devlab says it.
    pub(super) fn devlab_line(&self) -> String {
        match self {
            Stack::Up { id, slot, how } => format!("stack {id} {} on slot {slot}", how.word()),
            Stack::Down { id, .. } => format!("stack {id} stopped"),
            Stack::State { id, up, .. } => format!("{id} {}", if *up { "up" } else { "down" }),
        }
    }
}

pub(super) fn stack(ctx: &Ctx, sub: &str) -> Result<Stack, Failure> {
    let held = lease::live(ctx)?;
    let (Some(project), Some(slot)) = (
        ctx.project.as_ref().filter(|p| p.supabase.is_some()),
        held.slot,
    ) else {
        return Err(Failure::precondition(
            "this lease has no stack slot (devlab.json has no supabase)",
        ));
    };
    let Some(info) = stack::info(project, &ctx.worktree, slot)? else {
        return Err(Failure::precondition(
            "this lease has no stack slot (devlab.json has no supabase)",
        ));
    };
    let id = info.project_id;
    Ok(match sub {
        "up" => {
            let env = lease::lease_env(&held, Some(project))?;
            let how = stack::up(project, &ctx.worktree, slot, &env, &ctx.cfg)?;
            if how == stack::Up::Started {
                lease::stack_ours(&project.name, slot, true)?;
            }
            Stack::Up { id, slot, how }
        }
        "down" => {
            stack::down(project, &id, &ctx.worktree)?;
            lease::stack_ours(&project.name, slot, false)?;
            Stack::Down { id, slot }
        }
        _ => {
            let up = stack::running(&id);
            Stack::State { id, slot, up }
        }
    })
}
