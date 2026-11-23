//! `factory lab` under its own name: devlab's verbs under the output contract (spec #1,
//! "Output contract"). One line on stdout, naming a file when there is more; nothing while it
//! waits; one JSON object instead with `--json`, which every verb takes; a failure as
//! `factory: …`. `run` and `exec` put their command's output in a file the line names, and
//! exit with its code. Under the `devlab` name the lab keeps devlab's dialect ([`super::cli`]),
//! the command's output its caller's.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use factory_core::lab::args::parse_opts;
use factory_core::lab::slots::export_lines;
use serde_json::{Map, Value, json};

use super::cli::{self, HELP, Stack};
use super::context::{Ctx, context};
use super::wire::{Board, Lease};
use super::{lease, run, status};
use crate::output::{self, Done, Failure};
use crate::paths::Paths;

/// Runs one `factory lab` command; `args` come after `lab`, and `json` is the global flag.
pub fn main(args: &[OsString], json: bool) -> ExitCode {
    let mut args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let json = strip_json(&mut args) || json;
    match dispatch(&args) {
        Ok(Verb::Done(done)) => output::finish(Ok(done), json),
        Ok(Verb::Ran(done, code)) => {
            output::print(&done, json);
            ExitCode::from(code)
        }
        Err(failure) => output::finish(Err(failure), json),
    }
}

enum Verb {
    /// What the verb shows under the output contract.
    Done(Done),
    /// What `run` or `exec` shows, and its command's exit code, which it exits with.
    Ran(Done, u8),
}

/// Takes every `--json` before a `--` out of `args`; whether there was one.
fn strip_json(args: &mut Vec<String>) -> bool {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let before = args.len();
    let mut at = 0;
    args.retain(|arg| {
        at += 1;
        at > end || arg != "--json"
    });
    args.len() != before
}

fn dispatch(argv: &[String]) -> Result<Verb, Failure> {
    let (cmd, args) = match argv.split_first() {
        Some((cmd, args)) => (cmd.as_str(), args),
        None => ("help", &[][..]),
    };
    let done = match cmd {
        "help" | "-h" | "--help" => {
            let file = save("help.txt", HELP)?;
            Done::new(
                format!(
                    "factory lab: acquire, run, exec, env, release, stack, status, gc; usage in {}",
                    file.display()
                ),
                json!({ "help": HELP, "helpFile": file }),
            )
        }
        "status" => status_done(&status::board()?)?,
        "acquire" | "run" => {
            let ctx = context()?;
            let (mut opts, rest, _flags) = parse_opts(args).map_err(Failure::usage)?;
            if opts.wait_sec.is_none() {
                opts.wait_sec = Some(ctx.cfg.wait_sec);
            }
            if cmd == "run" && rest.is_empty() {
                return Err(Failure::usage("factory lab run [opts] -- <cmd...>"));
            }
            let mut warnings = Vec::new();
            let granted = lease::acquire(&ctx, &opts, &mut |_| {}, &mut |warning| {
                warnings.push(warning.to_owned());
            })?;
            if cmd == "run" {
                return ran(&ctx, &granted, rest);
            }
            lease_done(&ctx, &granted, &warnings)?
        }
        "exec" => {
            let ctx = context()?;
            let rest = match args.split_first() {
                Some((dashes, rest)) if dashes == "--" => rest,
                _ => args,
            };
            if rest.is_empty() {
                return Err(Failure::usage("factory lab exec -- <cmd...>"));
            }
            return ran(&ctx, &lease::live(&ctx)?, rest);
        }
        "env" => {
            let ctx = context()?;
            lease_done(&ctx, &lease::live(&ctx)?, &[])?
        }
        "release" => {
            let dropped = cli::release(&context()?, args.iter().any(|a| a == "--stop-stack"))?;
            let line = dropped.as_ref().map_or_else(
                || "nothing held here".to_owned(),
                |lease| format!("released lease {}", lease.id),
            );
            Done::new(line, json!({ "released": dropped }))
        }
        "stack" => stack_done(&cli::stack(
            &context()?,
            args.first().map_or("status", String::as_str),
        )?),
        "gc" => {
            let freed = lease::gc()?;
            match freed.as_slice() {
                [] => Done::new("no lease of an exited owner", json!({ "released": freed })),
                [one] => Done::new(format!("released {one}"), json!({ "released": freed })),
                many => {
                    let lines: String = many.iter().map(|f| format!("released {f}\n")).collect();
                    let file = save("gc.txt", &lines)?;
                    Done::new(
                        format!(
                            "released {} leases of exited owners; list in {}",
                            many.len(),
                            file.display()
                        ),
                        json!({ "released": freed, "releasedFile": file }),
                    )
                }
            }
        }
        "prep" | "watch" => {
            return Err(Failure::usage(format!(
                "factory lab {cmd} is not here yet; run today's devlab for it"
            )));
        }
        other => {
            return Err(Failure::usage(format!(
                "unknown command {other}; factory lab help"
            )));
        }
    };
    Ok(Verb::Done(done))
}

/// `2 leases, 1 waiting; status in <file>`, the file holding devlab's status screen; the board
/// as JSON.
fn status_done(board: &Board) -> Result<Done, Failure> {
    let file = save("status.txt", &(status::screen(board)? + "\n"))?;
    let leases = board.leases.len();
    let line = format!(
        "{leases} lease{}, {} waiting; status in {}",
        if leases == 1 { "" } else { "s" },
        board.waiters.len(),
        file.display()
    );
    let mut json = serde_json::to_value(board).unwrap_or(Value::Null);
    if let Value::Object(fields) = &mut json {
        fields.insert("statusFile".to_owned(), json!(file));
    }
    Ok(Done::new(line, json))
}

/// `lease 3, slot 0 (stack …, up); env in <file>`, the file holding the lease's export lines
/// (after a comment for each warning); the lease, its env and the file as JSON.
fn lease_done(ctx: &Ctx, lease: &Lease, warnings: &[String]) -> Result<Done, Failure> {
    let project = ctx.project.as_ref();
    let env = lease::lease_env(lease, project)?;
    let comments: String = warnings
        .iter()
        .map(|w| format!("# warning: {w}\n"))
        .collect();
    let file = save(
        &format!("lease-{}.env", lease.id),
        &format!("{comments}{}\n", export_lines(&env)),
    )?;
    let warned = match warnings.len() {
        0 => String::new(),
        1 => ", 1 warning".to_owned(),
        n => format!(", {n} warnings"),
    };
    let line = format!(
        "{}{warned}; env in {}",
        lease::summary(lease, project)?,
        file.display()
    );
    let vars: Map<String, Value> = env.into_iter().map(|(k, v)| (k, json!(v))).collect();
    Ok(Done::new(
        line,
        json!({ "lease": lease, "env": vars, "envFile": file, "warnings": warnings }),
    ))
}

/// Runs `argv` in the lease, its output in `<runtime>/lab/lease-N.out`: the line
/// `sh exited 3 in lease 1; output in <file>`, and the command's code to exit with.
fn ran(ctx: &Ctx, lease: &Lease, argv: &[String]) -> Result<Verb, Failure> {
    let file = save(&format!("lease-{}.out", lease.id), "")?;
    let code = run::exec_in(ctx, lease, argv, Some(&file))?;
    let program = argv.first().map_or("", String::as_str);
    let line = format!(
        "{program} exited {code} in lease {}; output in {}",
        lease.id,
        file.display()
    );
    let json = json!({ "lease": lease.id, "exit": code, "outputFile": file });
    Ok(Verb::Ran(Done::new(line, json), code))
}

fn stack_done(done: &Stack) -> Done {
    let (id, slot, state) = match done {
        Stack::Up { id, slot, .. } => (id, slot, "up"),
        Stack::Down { id, slot } => (id, slot, "down"),
        Stack::State { id, slot, up } => (id, slot, if *up { "up" } else { "down" }),
    };
    let line = match done {
        Stack::Up { how, .. } => format!("stack {id} {} on slot {slot}", how.word()),
        Stack::Down { .. } => format!("stack {id} on slot {slot} stopped"),
        Stack::State { .. } => format!("stack {id} on slot {slot} is {state}"),
    };
    Done::new(line, json!({ "stack": id, "slot": slot, "state": state }))
}

/// Writes `text` to `name` in the lab's runtime directory; its path.
fn save(name: &str, text: &str) -> Result<PathBuf, Failure> {
    let paths = Paths::from_env()?;
    paths.create_runtime()?;
    let dir = paths.runtime.join("lab");
    let path = dir.join(name);
    fs::create_dir_all(&dir)
        .and_then(|()| fs::write(&path, text))
        .map_err(|e| {
            Failure::precondition(format!(
                "cannot write {}: {e}; check XDG_RUNTIME_DIR",
                path.display()
            ))
        })?;
    Ok(path)
}
