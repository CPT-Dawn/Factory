//! `factory`: the CLI and the daemon of the agent software factory (spec #1).

mod cli;
mod client;
mod clock;
mod config;
mod daemon;
mod install;
mod lead;
mod leads;
mod log;
mod output;
mod paths;
mod protocol;
mod review;
mod router;
mod store;

use std::ffi::OsString;
use std::process::ExitCode;

use cli::{Command, LeadCommand, WaitCommand};
use output::{Done, Failure};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let json = args.iter().skip(1).any(|arg| arg == "--json");
    match cli::parse(args) {
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
            command: LeadCommand::List,
        } => lead::list(),
        Command::Lead {
            command: LeadCommand::Status { lead },
        } => lead::status(&lead),
        Command::Wait {
            command: WaitCommand::Lead { lead, timeout },
        } => lead::wait(&lead, timeout),
        Command::Review {
            base,
            label,
            spec,
            previous,
            timeout,
        } => review::run(&review::Args {
            base,
            label,
            spec,
            previous,
            timeout: timeout.map(std::time::Duration::from_secs),
        }),
        Command::Install { units } => install::install(units.as_deref()),
        Command::Uninstall => install::uninstall(),
    };
    output::finish(outcome, json)
}
