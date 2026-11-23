//! `factory`: the CLI and the daemon of the agent software factory (spec #1).

mod cli;
mod client;
mod daemon;
mod log;
mod output;
mod paths;
mod protocol;
mod review;

use std::ffi::OsString;
use std::process::ExitCode;

use cli::Command;
use output::{Done, Failure};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let json = args.iter().skip(1).any(|arg| arg == "--json");
    match cli::parse(args) {
        Ok(cli) => output::finish(run(cli.command, cli.json), cli.json),
        Err(refusal) => cli::refuse(refusal, json),
    }
}

fn run(command: Command, json: bool) -> Result<Done, Failure> {
    match command {
        Command::Log { kind, pairs } => log::run(&kind, &pairs),
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
            timeout: std::time::Duration::from_secs(timeout),
        }),
        Command::Daemon => daemon::run(json),
    }
}
