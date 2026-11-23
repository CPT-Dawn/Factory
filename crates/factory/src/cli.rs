//! The command line: what each command takes, and `--help` kept short.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

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
    /// Say what the leads in herdr are doing
    Lead {
        #[command(subcommand)]
        command: LeadCommand,
    },
    /// Block, silent, until something happens
    Wait {
        #[command(subcommand)]
        command: WaitCommand,
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
        /// Seconds a reviewer may take before its process group is killed (default: the config's)
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
    /// Run the daemon; other commands start it when it isn't running
    Daemon,
}

#[derive(Subcommand, Debug)]
pub enum LeadCommand {
    /// Start a lead on a spec in a new herdr workspace, and say where it is
    Spawn {
        /// The lead's slug: lowercase letters, digits and -, at most 27
        slug: String,
        /// The spec the lead works: an issue ref such as #12
        spec: String,
        /// The branch the lead's worktree is cut from
        #[arg(default_value = "main")]
        base: String,
        /// The skill the lead runs
        #[arg(default_value = "implement-spec")]
        skill: String,
        /// The repository to work in (default: the current directory's)
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// One line per lead: working, waiting (on what), blocked, asking, finished or dead
    List,
    /// One lead's line, and its pane's last lines when it is blocked or asking
    Status {
        /// The lead's slug, or lead-<slug>
        lead: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum WaitCommand {
    /// Until the lead finishes (exit 0), blocks, asks or dies (exit 10)
    Lead {
        /// The lead's slug, or lead-<slug>
        lead: String,
        /// Give up after this long with exit 124, such as 90s, 9m or 2h
        #[arg(long, default_value = "9m", value_parser = duration)]
        timeout: Duration,
    },
}

fn duration(text: &str) -> Result<Duration, String> {
    factory_core::duration::parse(text).map_err(|e| e.to_string())
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
