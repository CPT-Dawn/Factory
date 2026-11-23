//! The output contract (spec #1, "Output contract").
//!
//! Every command prints one line on stdout (at most [`LINE_LIMIT`] characters), or one JSON
//! object with `--json`, and exits with a
//! code from the one table in [`factory_core::exit`]. A failure is one line on stderr starting
//! `factory: `, or one JSON object on stdout with `--json`.

use std::io::{self, Write};
use std::process::ExitCode;

use factory_core::exit::Exit;
use serde_json::{Value, json};

/// The most characters a default line shows; a longer one is cut and ends in `…`.
pub const LINE_LIMIT: usize = 200;

/// What a command that succeeded shows.
#[derive(Debug)]
pub struct Done {
    pub line: String,
    pub json: Value,
    /// Lines after the first, only where the spec names them (a review's blocking findings).
    pub more: Vec<String>,
    /// Notices on stderr, only where the spec names them (a backend skipped because its
    /// sandbox is not active); `--json` carries them in its object instead.
    pub notes: Vec<String>,
    /// [`Exit::Ok`], or [`Exit::NeedsAttention`] for a result that asks for work.
    pub exit: Exit,
    /// Shows the line whole even past [`LINE_LIMIT`]: for a line whose point is the file it
    /// names, which a cut would leave naming nothing.
    pub whole: bool,
}

impl Done {
    pub fn new(line: impl Into<String>, json: Value) -> Done {
        Done {
            line: line.into(),
            json,
            more: Vec::new(),
            notes: Vec::new(),
            exit: Exit::Ok,
            whole: false,
        }
    }
}

/// Why a command failed: its exit code and one line the agent can act on.
#[derive(Debug)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn new(exit: Exit, message: impl Into<String>) -> Failure {
        Failure {
            exit,
            message: one_line(&message.into()),
        }
    }

    pub fn usage(message: impl Into<String>) -> Failure {
        Failure::new(Exit::Usage, message)
    }

    pub fn precondition(message: impl Into<String>) -> Failure {
        Failure::new(Exit::Precondition, message)
    }
}

/// Prints what a command that succeeded shows. Output nobody reads is not an error.
pub fn print(done: &Done, json: bool) {
    let mut out = io::stdout().lock();
    if json {
        let _ = writeln!(out, "{}", done.json);
        return;
    }
    let first = if done.whole {
        one_line(&done.line)
    } else {
        bounded(&one_line(&done.line))
    };
    let _ = writeln!(out, "{first}");
    for line in &done.more {
        let _ = writeln!(out, "{}", bounded(&one_line(line)));
    }
    let mut err = io::stderr().lock();
    for note in &done.notes {
        let _ = writeln!(err, "factory: {}", one_line(note));
    }
}

/// Shows a command's outcome and turns it into its exit code.
pub fn finish(outcome: Result<Done, Failure>, json: bool) -> ExitCode {
    match outcome {
        Ok(done) => {
            print(&done, json);
            ExitCode::from(done.exit.code())
        }
        Err(failure) => {
            if json {
                let body = json!({"exit": failure.exit.code(), "error": failure.message});
                let _ = writeln!(io::stdout(), "{body}");
            } else {
                let _ = writeln!(io::stderr(), "factory: {}", failure.message);
            }
            ExitCode::from(failure.exit.code())
        }
    }
}

fn bounded(line: &str) -> String {
    if line.chars().count() <= LINE_LIMIT {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(LINE_LIMIT - 1).collect();
    cut.push('…');
    cut
}

fn one_line(text: &str) -> String {
    text.trim_end().replace('\n', " ")
}
