//! The command line: what each command takes, and `--help` kept short.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use factory_core::exit::Exit;

use crate::output::{self, Failure};

/// The agent software factory: one daemon does the waiting, polling and bookkeeping.
#[derive(Parser, Debug)]
#[command(name = "factory", version, disable_help_subcommand = true)]
pub struct Cli {
    /// Print one JSON object instead of one line
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Append one event to the ledger
    Log {
        /// Event type: lowercase letters and dashes
        #[arg(value_name = "TYPE")]
        kind: String,
        /// Fields; numbers stay numbers, empty values are dropped
        #[arg(value_name = "KEY=VALUE")]
        pairs: Vec<String>,
    },
    /// Review the commits since BASE on a second vendor's model
    Review {
        /// The fixed point the work started from
        base: String,
        /// Names the report: letters, digits, _ and -; a -rN suffix is the round
        label: String,
        /// The spec: a file, an issue ref (#12, 12, an issue URL) or none
        spec: Option<String>,
        /// The previous round's report
        previous: Option<PathBuf>,
        /// Seconds the reviewer may take before its process group is killed
        #[arg(long, value_name = "SECONDS", default_value_t = 1800)]
        timeout: u64,
    },
    /// Run the daemon; other commands start it when it isn't running
    Daemon,
    /// Lease this PC's local stack slots, in devlab's words: factory lab help (or run it as devlab)
    #[command(disable_help_flag = true)]
    Lab {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

pub fn parse(args: Vec<OsString>) -> Result<Cli, clap::Error> {
    let matches = Cli::command()
        .after_help(exit_codes())
        .try_get_matches_from(args)?;
    Cli::from_arg_matches(&matches)
}

/// Prints help or the version when asked, and otherwise refuses in one line with exit 2.
pub fn refuse(refusal: clap::Error, json: bool) -> ExitCode {
    if matches!(
        refusal.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        let _ = refusal.print();
        return ExitCode::SUCCESS;
    }
    let rendered = refusal.render().to_string();
    let reason = rendered.lines().next().unwrap_or_default();
    let reason = reason.strip_prefix("error: ").unwrap_or(reason);
    output::finish(
        Err(Failure::usage(format!("{reason}; see factory --help"))),
        json,
    )
}

fn exit_codes() -> String {
    let codes: Vec<String> = Exit::ALL
        .iter()
        .map(|exit| format!("{} {}", exit.code(), exit.meaning()))
        .collect();
    format!("Exit codes: {}.", codes.join(", "))
}
