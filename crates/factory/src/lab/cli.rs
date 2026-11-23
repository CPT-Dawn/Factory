//! devlab's command line: its commands and options, its help and how it fails.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use factory_core::lab::slots::export_lines;

use super::context::{Ctx, context};
use super::lease::{self, Opts};
use super::{run, stack, status};
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
            let ctx = context()?;
            let (mut opts, rest, flags) = parse_opts(args)?;
            if opts.wait_sec.is_none() {
                opts.wait_sec = Some(ctx.cfg.wait_sec);
            }
            if cmd == "run" && rest.is_empty() {
                return Err(Failure::usage("devlab run [opts] -- <cmd...>"));
            }
            let granted = lease::acquire(&ctx, &opts)?;
            if !flags.iter().any(|f| f == "--quiet") {
                note(&lease::summary(&granted, ctx.project.as_ref())?);
            }
            if cmd == "acquire" {
                println!(
                    "{}",
                    export_lines(&lease::lease_env(&granted, ctx.project.as_ref())?)
                );
                return Ok(0);
            }
            run::exec_in(&ctx, &granted, rest)
        }
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

fn release(ctx: &Ctx, stop_stack: bool) -> Result<u8, Failure> {
    let Some(dropped) = lease::release(ctx)? else {
        note("nothing held here");
        return Ok(0);
    };
    if let (true, Some(project), Some(slot)) = (stop_stack, &ctx.project, dropped.slot) {
        if let Some(info) = stack::info(project, &ctx.worktree, slot)?
            && stack::running(&info.project_id)
        {
            stack::down(project, &info.project_id, &ctx.worktree)?;
        }
        lease::stack_ours(&project.name, slot, false)?;
    }
    note(&format!("released lease {}", dropped.id));
    Ok(0)
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

/// Options for run and acquire, the command after them, and the other `--flags` seen.
fn parse_opts(args: &[String]) -> Result<(Opts, &[String], Vec<String>), Failure> {
    let mut want = 1.0;
    let mut opts = Opts::default();
    let mut flags = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "--devices" => {
                i += 1;
                want = js_number(args.get(i));
            }
            "--pair" => want = 2.0,
            "--no-device" => want = 0.0,
            "--phone" => opts.mode = "phone",
            "--emulator" => opts.mode = "emulator",
            "--stack" => opts.stack = true,
            "--wait" => {
                i += 1;
                let sec = js_number(args.get(i));
                if sec.is_nan() || sec < 0.0 {
                    return Err(Failure::usage(
                        "--wait takes a number of seconds (0 = do not wait)",
                    ));
                }
                opts.wait_sec = Some(sec);
            }
            flag if flag.starts_with("--") => flags.push(flag.to_owned()),
            _ => break,
        }
        i += 1;
    }
    if want.fract() != 0.0 || !want.is_finite() || want < 0.0 {
        return Err(Failure::usage("--devices takes a whole number"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        opts.want = want as u32;
    }
    Ok((opts, args.get(i..).unwrap_or_default(), flags))
}

/// JavaScript's `Number(text)`, near enough: blank is 0, junk (or nothing) is NaN.
fn js_number(text: Option<&String>) -> f64 {
    let Some(text) = text else { return f64::NAN };
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    if text
        .chars()
        .any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E'))
    {
        return if text.trim_start_matches(['+', '-']) == "Infinity" {
            text.parse().unwrap_or(f64::NAN)
        } else {
            f64::NAN
        };
    }
    text.parse().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Result<(Opts, Vec<String>, Vec<String>), String> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        parse_opts(&args)
            .map(|(o, rest, flags)| (o, rest.to_vec(), flags))
            .map_err(|f| f.message)
    }

    #[test]
    fn options_stop_at_the_command_and_keep_unknown_flags() {
        let (o, rest, flags) =
            opts(&["--no-device", "--stack", "--quiet", "--", "echo", "--x"]).unwrap();
        assert_eq!(
            (o.want, o.stack, rest, flags),
            (
                0,
                true,
                vec!["echo".into(), "--x".into()],
                vec!["--quiet".to_owned()]
            )
        );
        let (o, rest, _) = opts(&["--pair", "--wait", "2.5", "true"]).unwrap();
        assert_eq!(
            (o.want, o.wait_sec, rest),
            (2, Some(2.5), vec!["true".to_owned()])
        );
    }

    #[test]
    fn a_bad_count_or_wait_is_refused() {
        assert_eq!(
            opts(&["--devices", "two"]).unwrap_err(),
            "--devices takes a whole number"
        );
        assert_eq!(
            opts(&["--devices", "1.5"]).unwrap_err(),
            "--devices takes a whole number"
        );
        assert_eq!(
            opts(&["--devices"]).unwrap_err(),
            "--devices takes a whole number"
        );
        for bad in ["soon", "-1", "nan"] {
            assert!(
                opts(&["--wait", bad])
                    .unwrap_err()
                    .starts_with("--wait takes"),
                "{bad}"
            );
        }
        assert_eq!(opts(&["--wait", " 7 "]).unwrap().0.wait_sec, Some(7.0));
    }
}
