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
use super::{reserve, run, stack, status};
use crate::output::Failure;

const HELP: &str = include_str!("help.txt");

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
            let granted = lease::acquire(&ctx, &opts)?;
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
            run::exec_in(&ctx, &held, rest)
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
        "release" => release(&context()?, args.iter().any(|a| a == "--stop-stack")),
        "stack" => stack_command(&context()?, args.first().map_or("status", String::as_str)),
        "gc" => {
            for freed in lease::gc()? {
                println!("released {freed}");
            }
            Ok(0)
        }
        "prep" | "watch" => Err(Failure::usage(format!(
            "devlab {cmd} is not in factory lab yet; run today's devlab for it"
        ))),
        other => Err(Failure::usage(format!(
            "unknown command {other}; devlab help"
        ))),
    }
}

/// Gives the lease back. With `--stop-stack` the stack stops first, while the lease still keeps
/// the slot from the next in the queue; a stop that fails still gives the lease back.
fn release(ctx: &Ctx, stop_stack: bool) -> Result<u8, Failure> {
    let held = if stop_stack { lease::find(ctx)? } else { None };
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
        note("nothing held here");
        return Ok(0);
    };
    if let Some(stopped) = stopped {
        stopped?;
    }
    note(&format!("released lease {}", dropped.id));
    Ok(0)
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

fn stack_command(ctx: &Ctx, sub: &str) -> Result<u8, Failure> {
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
    match sub {
        "up" => {
            let env = lease::lease_env(&held, Some(project))?;
            let how = stack::up(project, &ctx.worktree, slot, &env, &ctx.cfg)?;
            if how == stack::Up::Started {
                lease::stack_ours(&project.name, slot, true)?;
            }
            note(&format!(
                "stack {} {} on slot {slot}",
                info.project_id,
                how.word()
            ));
        }
        "down" => {
            stack::down(project, &info.project_id, &ctx.worktree)?;
            lease::stack_ours(&project.name, slot, false)?;
            note(&format!("stack {} stopped", info.project_id));
        }
        _ => {
            let state = if stack::running(&info.project_id) {
                "up"
            } else {
                "down"
            };
            println!("{} {state}", info.project_id);
        }
    }
    Ok(0)
}
