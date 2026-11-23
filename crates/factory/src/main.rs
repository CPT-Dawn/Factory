//! `factory`: the CLI and the daemon of the agent software factory (spec #1).

mod ci;
mod cli;
mod client;
mod clock;
mod config;
mod cutover;
mod daemon;
mod hook;
mod install;
mod lab;
mod lead;
mod leads;
mod ledger;
mod log;
mod monitor;
mod notifier;
mod notify;
mod output;
mod paths;
mod pollers;
mod protocol;
mod review;
mod reviews;
mod router;
mod sessions;
mod spawn;
mod status;
mod steward;
mod store;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use cli::{Cli, Command, LeadCommand, MonitorCommand, WaitCommand};
use output::{Done, Failure};

/// How long a `factory wait` blocks without `--timeout`.
const WAIT_TIMEOUT: Duration = Duration::from_secs(9 * 60);

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // The multicall: as `devlab` the lab speaks devlab's dialect; as `factory lab` it keeps
    // the output contract.
    let name = args.first().and_then(|arg| Path::new(arg).file_name());
    if name.is_some_and(|name| name == "devlab") {
        return lab::main(args.get(1..).unwrap_or_default());
    }
    if args.get(1).is_some_and(|arg| arg == "lab") {
        return lab::native(args.get(2..).unwrap_or_default(), false);
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
        }) => lab::native(
            &args.into_iter().map(OsString::from).collect::<Vec<_>>(),
            json,
        ),
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
        Command::Status => status::run(),
        Command::Monitor { command } => match command {
            MonitorCommand::Start => monitor::start(),
            MonitorCommand::Stop => monitor::stop(),
            MonitorCommand::Status => monitor::status(),
        },
        Command::Wait {
            command:
                WaitCommand::Lead {
                    lead,
                    timeout,
                    notify,
                },
        } => {
            if notify {
                lead::notify(&lead, timeout.unwrap_or(notify::TIMEOUT))
            } else {
                lead::wait(&lead, timeout.unwrap_or(WAIT_TIMEOUT))
            }
        }
        Command::Wait {
            command:
                WaitCommand::Ci {
                    pr,
                    timeout,
                    notify,
                },
        } => {
            if notify {
                ci::notify(&pr, timeout.unwrap_or(notify::TIMEOUT))
            } else {
                ci::wait(&pr, timeout.unwrap_or(WAIT_TIMEOUT))
            }
        }
        Command::Wait {
            command: WaitCommand::Review { label, timeout },
        } => reviews::wait(&label, timeout),
        Command::Wait {
            command: WaitCommand::Lease { lease, timeout },
        } => lab::wait_lease(lease, timeout),
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
        Command::Cutover {
            rollback: false, ..
        } => cutover::cutover(),
        Command::Cutover {
            rollback: true,
            force,
        } => cutover::rollback(force),
        // main hands every `factory lab` to the lab; this arm is never reached.
        Command::Lab { .. } => Err(Failure::usage("run factory lab help")),
    };
    output::finish(outcome, json)
}
