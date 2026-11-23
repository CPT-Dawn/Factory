//! `factory`: the CLI and the daemon of the agent software factory (spec #1).

mod ci;
mod cli;
mod client;
mod clock;
mod config;
mod daemon;
mod hook;
mod install;
mod lab;
mod lead;
mod leads;
mod ledger;
mod log;
mod output;
mod paths;
mod pollers;
mod protocol;
mod review;
mod reviews;
mod router;
mod spawn;
mod store;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use cli::{Cli, Command, LeadCommand, WaitCommand};
use output::{Done, Failure};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // The multicall: as `devlab`, or as `factory lab`, the lab speaks devlab's dialect.
    let name = args.first().and_then(|arg| Path::new(arg).file_name());
    if name.is_some_and(|name| name == "devlab") {
        return lab::main(args.get(1..).unwrap_or_default());
    }
    if args.get(1).is_some_and(|arg| arg == "lab") {
        return lab::main(args.get(2..).unwrap_or_default());
    }
    let json = args.iter().skip(1).any(|arg| arg == "--json");
    match cli::parse(args) {
        Ok(Cli {
            command: Command::Hook { command },
            json,
        }) => hook::run(&command, json),
        Ok(cli::Cli {
            command: Command::Lab { args },
            json,
        }) => lab::main(&lab_args(args, json)),
        Ok(cli) => run(cli.command, cli.json),
        Err(refusal) => cli::refuse(refusal, json),
    }
}

fn run(command: Command, json: bool) -> ExitCode {
    let outcome: Result<Done, Failure> = match command {
        // The daemon prints its ready line itself, and nothing when it stops.
        Command::Daemon => {
            return daemon::run(json).map_or_else(
                |failure| output::finish(Err(failure), json),
                |()| ExitCode::SUCCESS,
            );
        }
        Command::Log { kind, pairs } => log::run(&kind, &pairs),
        Command::Lead {
            command:
                LeadCommand::Spawn {
                    slug,
                    spec,
                    base,
                    skill,
                    repo,
                },
        } => lead::spawn(&lead::SpawnArgs {
            slug,
            spec,
            base,
            skill,
            repo,
        })
        // lead.sh's failures said so, and factory-stats' classifier still matches the text.
        .map_err(|failure| Failure::new(failure.exit, format!("lead.sh: {}", failure.message))),
        Command::Lead {
            command: LeadCommand::List,
        } => lead::list(),
        Command::Lead {
            command: LeadCommand::Status { lead },
        } => lead::status(&lead),
        Command::Wait {
            command: WaitCommand::Lead { lead, timeout },
        } => lead::wait(&lead, timeout),
        Command::Wait {
            command: WaitCommand::Ci { pr, timeout },
        } => ci::wait(&pr, timeout),
        Command::Wait {
            command: WaitCommand::Review { label, timeout },
        } => reviews::wait(&label, timeout),
        Command::Review {
            base,
            label,
            spec,
            previous,
            timeout,
            tier,
        } => review::run(&review::Args {
            base,
            label,
            spec,
            previous,
            timeout: timeout.map(std::time::Duration::from_secs),
            tier,
        })
        // review.sh's failures said so, and factory-stats' classifier still matches the text.
        .map_err(|failure| Failure::new(failure.exit, format!("code-review: {}", failure.message))),
        // Answered in `main`: a hook prints its own protocol, not the output contract's line.
        Command::Hook { .. } => unreachable!("main answers hooks before it gets here"),
        Command::Install { units } => install::install(units.as_deref()),
        Command::Uninstall => install::uninstall(),
        // main hands every `factory lab` to the lab; this arm is never reached.
        Command::Lab { .. } => Err(Failure::usage("run factory lab help")),
    };
    output::finish(outcome, json)
}

/// The lab's arguments when global flags came before `lab` (`factory --json lab status`): the
/// lab speaks devlab's dialect, where only `status` takes `--json`.
fn lab_args(args: Vec<String>, json: bool) -> Vec<OsString> {
    let mut out: Vec<OsString> = args.into_iter().map(OsString::from).collect();
    if json && out.first().is_some_and(|cmd| cmd == "status") {
        out.push(OsString::from("--json"));
    }
    out
}
