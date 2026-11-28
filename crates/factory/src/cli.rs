//! The command line: what each command takes, and `--help` kept short.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::builder::{NonEmptyStringValueParser, PossibleValuesParser};
use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use factory_core::exit::Exit;
use factory_core::lead::EFFORTS;

use crate::output::{self, Failure};

/// The agent software factory: one daemon does the waiting, polling and bookkeeping.
#[derive(Parser, Debug)]
#[command(name = "factory", version = crate::buildinfo::VERSION, disable_help_subcommand = true)]
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
    /// Watch every lead's and subagent's context; compact idle leads over the threshold
    Monitor {
        #[command(subcommand)]
        command: MonitorCommand,
    },
    /// One screen: leases and queue, reviewer quotas, waits, leads and their contexts
    Status,
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
        /// The spec: a file, an issue ref (#12, 12, owner/repo#12, an issue URL) or none
        spec: Option<String>,
        /// The previous round's report
        previous: Option<PathBuf>,
        /// How long a reviewer may take before its process group is killed: seconds, or 90s, 9m
        /// or 1h (default: the config's)
        #[arg(long, value_parser = duration)]
        timeout: Option<Duration>,
        /// The ticket's tier, which picks the chain of reviewers: hard, standard or trivial
        /// (a spec review is its own tier, spec; a small or docs-only diff is trivial unless tagged hard)
        #[arg(long, value_name = "TIER")]
        tier: Option<String>,
        /// The reviewer skill the prompt carries (default: the config's, else
        /// ~/.agents/skills/code-review/SKILL.md)
        #[arg(long, value_name = "FILE")]
        skill: Option<PathBuf>,
        /// An escalation's review, which may run rounds 3 and 4 (every ticket gets rounds 1 and 2,
        /// and one light review, <prefix>-r2-light, after the last); needs the lead's
        /// escalation event for the prefix in the ledger
        #[arg(long)]
        escalation: bool,
        /// A review outside a ticket's rounds, such as a retro's: its label names no -rN round
        #[arg(long, conflicts_with = "escalation")]
        standalone: bool,
        /// The follow-up issue a new round 1 reviews, when its work was reviewed under a prefix
        /// that used its cap: #12, 12, owner/repo#12 or an issue URL
        #[arg(long, value_name = "ISSUE")]
        follow_up: Option<String>,
    },
    /// Answer a Claude Code hook: its JSON on stdin, its hook protocol on stdout
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// The harness quotas: whether a lead may dispatch on Claude or a review on Codex (exit 0, or 75 naming the reset), and the usage-window history
    Quota {
        #[command(subcommand)]
        command: QuotaCommand,
    },
    /// The bar feed: one line of each harness's windows and the leads; --json, Zenith's cockpit v1 document
    Bar,
    /// Run the daemon; other commands start it when it isn't running
    Daemon,
    /// Install or upgrade factory in ~/.local/bin and its systemd units, and start the socket
    Install {
        /// Link the units from here (DotFiles' factory/.config/systemd/user); else they are in place
        #[arg(long, value_name = "DIR")]
        units: Option<PathBuf>,
    },
    /// Stop the daemon and remove the binary and units install put in place; state stays
    Uninstall,
    /// Hand the devlab name on PATH to factory lab and retire today's devlab, while it holds no lease
    Cutover {
        /// Give the name back to today's devlab and start its watcher again
        #[arg(long)]
        rollback: bool,
        /// With --rollback: skip the check that factory lab holds nothing, for a lab that no longer answers, and the need for a watcher unit to start again
        #[arg(long, requires = "rollback")]
        force: bool,
    },
    /// Lease this PC's local stack slots, in devlab's words: factory lab help (or run it as devlab)
    #[command(disable_help_flag = true)]
    Lab {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum HookCommand {
    /// Before a tool runs: refuse npm, pkill -f and killall, a leaking commit or push, a REPL script that can't run
    PreTool,
    /// After a tool ran: a note to the model when its result is over 20k characters
    PostTool,
    /// When a subagent stops: send back a report longer than its template
    SubagentStop,
    /// When a turn ends on an API error: a rate limit holds Claude dispatches until its reset
    StopFailure,
    /// The status line's JSON, handed on: records Claude's 5-hour and weekly windows, prints nothing, exits 0
    Statusline,
}

#[derive(Subcommand, Debug)]
pub enum QuotaCommand {
    /// The Claude plan's 5-hour and weekly windows: dispatch (exit 0) or hold until the holding one resets (exit 75)
    Claude,
    /// The Codex plan's 5-hour and weekly windows: dispatch (exit 0) or hold reviews until the holding one resets (exit 75)
    Codex,
    /// Every usage-window reading kept (90 days), oldest first; --json for the rows
    History {
        /// Only readings from this moment on: a UTC time (2026-11-20T00:00:00Z), a date (2026-11-20) or an age (6h, 7d)
        #[arg(long, value_name = "TIME")]
        since: Option<String>,
    },
    /// agy's quota groups and their 5-hour and weekly windows, or why its usage can't be read
    Agy,
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
        /// The model Claude starts with (default: [lead] model)
        #[arg(long, value_name = "MODEL", value_parser = NonEmptyStringValueParser::new())]
        model: Option<String>,
        /// The effort Claude starts with: low, medium, high, xhigh or max (default: [lead] effort)
        #[arg(long, value_name = "EFFORT", value_parser = PossibleValuesParser::new(EFFORTS))]
        effort: Option<String>,
        /// A person is at this lead's pane: its prompt drops "You run unattended"
        #[arg(long)]
        attended: bool,
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
pub enum MonitorCommand {
    /// Start the context steward in the daemon; starting it twice changes nothing
    Start,
    /// Stop it; a compaction under way still finishes
    Stop,
    /// Whether it runs, and the context of each lead and subagent it watches
    Status,
}

#[derive(Subcommand, Debug)]
pub enum WaitCommand {
    /// Until the lead finishes (exit 0), blocks, asks or dies (exit 10)
    Lead {
        /// The lead's slug, or lead-<slug>; several with --any
        #[arg(required = true, value_name = "LEAD")]
        leads: Vec<String>,
        /// Until the first of the leads finishes, blocks, asks or dies, with its line
        #[arg(long, conflicts_with = "notify")]
        any: bool,
        /// Give up after this long with exit 124: seconds, or 90s, 9m or 1h [default: 9m,
        /// or 2h with --notify]
        #[arg(long, value_parser = duration)]
        timeout: Option<Duration>,
        /// Return at once; the daemon posts the line into this Claude Code session when the
        /// wait ends (off unless the config switches event delivery on)
        #[arg(long)]
        notify: bool,
    },
    /// Until every check on a PR's head, or main's latest, finished: all passed (exit 0) or
    /// some failed (exit 10)
    Ci {
        /// The PR: its number (from its repo's checkout), URL or branch; `main` is main's latest
        /// head, as --branch main
        #[arg(required_unless_present = "branch", conflicts_with = "branch")]
        pr: Option<String>,
        /// A branch of the repo of this checkout, whose latest head it waits on instead
        #[arg(long)]
        branch: Option<String>,
        /// Give up after this long with exit 124: seconds, or 90s, 9m or 1h [default: 9m,
        /// or 2h with --notify]
        #[arg(long, value_parser = duration)]
        timeout: Option<Duration>,
        /// Return at once; the daemon posts the line into this Claude Code session when the
        /// wait ends (off unless the config switches event delivery on)
        #[arg(long)]
        notify: bool,
    },
    /// Until the review of a label has ended: clean (exit 0), with findings or failed (exit 10)
    Review {
        /// The review's label, as factory review was given it
        label: String,
        /// Give up after this long with exit 124: seconds, or 90s, 9m or 1h
        #[arg(long, default_value = "9m", value_parser = duration)]
        timeout: Duration,
    },
    /// Until a lead may dispatch on the Claude plan again: both windows have room (exit 0)
    Claude {
        /// Give up after this long with exit 124: seconds, or 90s, 9m or 1h
        #[arg(long, default_value = "9m", value_parser = duration)]
        timeout: Duration,
    },
    /// Until a lab lease is held no more: released, its owner exited, or expired (exit 0)
    Lease {
        /// The lease's id, as factory lab acquire and status name it
        lease: u32,
        /// Give up after this long with exit 124: seconds, or 90s, 9m or 1h
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
