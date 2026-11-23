//! Review backends: each runs one review over a prompt and leaves its report in a file.
//!
//! Codex is the first; Claude and Antigravity join it behind the same [`Reviewer`] trait.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process::{self, RunError};

/// One review for a backend to run.
#[derive(Clone, Debug)]
pub struct Job {
    /// The repository under review; the backend runs read-only in it.
    pub cwd: PathBuf,
    /// The prompt, handed to the backend on stdin.
    pub prompt: PathBuf,
    /// Where the backend's final message, the report, goes.
    pub report: PathBuf,
    /// Where the backend's event stream goes.
    pub stream: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug)]
pub enum ReviewerError {
    /// The backend could not be started.
    Start(io::Error),
    /// The backend exited with this code (none when a signal ended it).
    Exited(Option<i32>),
    /// The backend outlived the job's timeout and was killed with its process group.
    TimedOut(Duration),
    /// The job's files could not be opened.
    Files(io::Error),
}

/// A review backend.
pub trait Reviewer {
    /// The backend's name in the ledger and in error lines: `codex`, `claude`, `agy`.
    fn name(&self) -> &str;
    /// The model it runs, when known.
    fn model(&self) -> Option<&str>;
    /// The reasoning effort it runs at, when known.
    fn effort(&self) -> Option<&str>;
    /// Runs the review to its end, leaving the report at `job.report`.
    fn review(&self, job: &Job) -> Result<(), ReviewerError>;
}

/// Codex, run read-only and ephemeral: `codex exec --json -s read-only`.
#[derive(Clone, Debug, Default)]
pub struct Codex {
    model: Option<String>,
    effort: Option<String>,
}

impl Codex {
    /// Codex as its config file sets it up: `model` and `model_reasoning_effort` from the
    /// top of `config.toml` in `codex_home` (`$CODEX_HOME`, else `~/.codex`).
    pub fn configured_in(codex_home: &Path) -> Codex {
        let config = fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
        Codex {
            model: top_level_string(&config, "model"),
            effort: top_level_string(&config, "model_reasoning_effort"),
        }
    }
}

impl Reviewer for Codex {
    fn name(&self) -> &str {
        "codex"
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    fn review(&self, job: &Job) -> Result<(), ReviewerError> {
        let prompt = File::open(&job.prompt).map_err(ReviewerError::Files)?;
        let stream = File::create(&job.stream).map_err(ReviewerError::Files)?;
        let stream_err = stream.try_clone().map_err(ReviewerError::Files)?;
        let mut cmd = Command::new("codex");
        cmd.arg("exec")
            .args(["--json", "-s", "read-only", "--ephemeral", "-C"])
            .arg(&job.cwd)
            .arg("-o")
            .arg(&job.report)
            .arg("-")
            .current_dir(&job.cwd)
            .stdin(Stdio::from(prompt))
            .stdout(Stdio::from(stream))
            .stderr(Stdio::from(stream_err));
        match process::run(cmd, job.timeout) {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(ReviewerError::Exited(output.status.code())),
            Err(RunError::Spawn(e) | RunError::Wait(e)) => Err(ReviewerError::Start(e)),
            Err(RunError::TimedOut(timeout)) => Err(ReviewerError::TimedOut(timeout)),
        }
    }
}

/// The string value of `key = "…"` before the first table header of a TOML file.
fn top_level_string(toml: &str, key: &str) -> Option<String> {
    toml.lines()
        .map(str::trim)
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            (k.trim() == key)
                .then(|| v.strip_prefix('"')?.strip_suffix('"'))
                .flatten()
                .map(str::to_owned)
        })
}

/// A backend that writes a scripted report, for tests of code that runs reviews.
#[derive(Clone, Debug, Default)]
pub struct FakeReviewer {
    pub report: String,
}

impl Reviewer for FakeReviewer {
    fn name(&self) -> &str {
        "fake"
    }

    fn model(&self) -> Option<&str> {
        None
    }

    fn effort(&self) -> Option<&str> {
        None
    }

    fn review(&self, job: &Job) -> Result<(), ReviewerError> {
        fs::write(&job.report, &self.report).map_err(ReviewerError::Files)
    }
}

impl fmt::Display for ReviewerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewerError::Start(e) => write!(f, "cannot start: {e}"),
            ReviewerError::Exited(Some(code)) => write!(f, "exited {code}"),
            ReviewerError::Exited(None) => write!(f, "was killed by a signal"),
            ReviewerError::TimedOut(timeout) => write!(f, "timed out after {}s", timeout.as_secs()),
            ReviewerError::Files(e) => write!(f, "cannot open its files: {e}"),
        }
    }
}

impl std::error::Error for ReviewerError {}
