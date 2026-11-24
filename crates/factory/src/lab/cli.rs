//! devlab's command line: its commands and options, its help and how it fails.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use factory_core::lab::slots::export_lines;

use super::context::{Ctx, context};
use factory_core::lab::args::parse_opts;
use factory_core::lab::config::ProjectConfig;

use super::lease;
use super::wire::Lease;
use super::{android, check, reserve, run, stack, status};
use crate::output::Failure;
use factory_adapters::adb::AdbCli;

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
                return run::run(&ctx, &opts, quiet, rest);
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
            match release(&context()?, args.iter().any(|a| a == "--stop-stack"))? {
                Some(dropped) => note(&format!("released lease {}", dropped.id)),
                None => note("nothing held here"),
            }
            Ok(0)
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
        "check" => {
            let phones = check::phones(args)?;
            if args.iter().any(|a| a == "--json") {
                println!("{}", check::to_json(&phones));
            } else {
                for line in check::lines(&phones) {
                    println!("{line}");
                }
            }
            Ok(check::code(&phones))
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

/// Gives the lease back; the lease it gave back, if one was held. With `--stop-stack` the
/// stack stops first, its slot held for this call meanwhile, whatever ends the lease, so the next
/// in the queue never gets a stack being torn down; a stop that fails still gives the lease back.
pub(super) fn release(ctx: &Ctx, stop_stack: bool) -> Result<Option<Lease>, Failure> {
    let held = if stop_stack {
        lease::stopping(ctx)?
    } else {
        None
    };
    let stopped = match (&held, &ctx.project) {
        (
            Some(Lease {
                slot: Some(slot), ..
            }),
            Some(project),
        ) => Some(stop(ctx, project, *slot)),
        _ => None,
    };
    let Some(dropped) = lease::release(ctx)? else {
        return Ok(None);
    };
    if let Some(stopped) = stopped {
        stopped?;
    }
    Ok(Some(dropped))
}

/// Stops the slot's stack if it runs, and marks it no longer devlab's to stop.
fn stop(ctx: &Ctx, project: &ProjectConfig, slot: u32) -> Result<(), Failure> {
    if let Some(info) = stack::info(project, &ctx.worktree, slot)?
        && stack::running(&info.project_id)
    {
        stack::down(project, &info.project_id, &ctx.worktree)?;
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
