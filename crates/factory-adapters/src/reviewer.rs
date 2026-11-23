//! Review backends: each runs one review over a prompt and leaves its report in a file.
//!
//! Codex and Claude sit behind the same [`Reviewer`] trait; Antigravity joins them. Each
//! backend knows its own out-of-quota signature and reads the reset time from it, so the
//! router can cool it down and re-run the review on the next one.

use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use factory_core::quota::{self, Reset};
use factory_core::review;
use serde_json::{Map, Value, json};

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
    /// The backend ran out of quota; it gets it back at this reset.
    OutOfQuota(Reset),
}

/// A review backend.
pub trait Reviewer {
    /// The backend's name in the ledger and in error lines: `codex`, `claude`, `agy`.
    fn name(&self) -> &str;
    /// The model it runs, when known.
    fn model(&self) -> Option<&str>;
    /// The reasoning effort it runs at, when known.
    fn effort(&self) -> Option<&str>;
    /// The prompt's first line, which sets it reviewing the changes since `base`.
    fn opening(&self, base: &str) -> String;
    /// The extension of its event stream's file, after the review's label.
    fn stream_extension(&self) -> &str;
    /// Runs the review to its end, leaving the report at `job.report`.
    fn review(&self, job: &Job) -> Result<(), ReviewerError>;
}

/// Codex, run read-only and ephemeral: `codex exec --json -s read-only`.
#[derive(Clone, Debug, Default)]
pub struct Codex {
    model: Option<String>,
    effort: Option<String>,
    /// Whether the model is passed on its command line, or is its config file's.
    pinned_model: bool,
    /// Whether the effort is passed on its command line, or is its config file's.
    pinned_effort: bool,
}

impl Codex {
    /// Codex on this `model` at this `effort`, as the factory's config sets them. What the
    /// config leaves unset is read from Codex's own `config.toml` in `codex_home`
    /// (`$CODEX_HOME`, else `~/.codex`), and left to Codex.
    pub fn new(model: Option<String>, effort: Option<String>, codex_home: &Path) -> Codex {
        let config = fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
        Codex {
            pinned_model: model.is_some(),
            pinned_effort: effort.is_some(),
            model: model.or_else(|| top_level_string(&config, "model")),
            effort: effort.or_else(|| top_level_string(&config, "model_reasoning_effort")),
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

    fn opening(&self, base: &str) -> String {
        format!("$code-review since {base}.")
    }

    fn stream_extension(&self) -> &str {
        "jsonl"
    }

    fn review(&self, job: &Job) -> Result<(), ReviewerError> {
        let prompt = File::open(&job.prompt).map_err(ReviewerError::Files)?;
        let stream = File::create(&job.stream).map_err(ReviewerError::Files)?;
        let stream_err = stream.try_clone().map_err(ReviewerError::Files)?;
        let mut cmd = Command::new("codex");
        cmd.arg("exec")
            .args(["--json", "-s", "read-only", "--ephemeral"]);
        if let Some(model) = self.model.as_ref().filter(|_| self.pinned_model) {
            cmd.args(["-m", model]);
        }
        if let Some(effort) = self.effort.as_ref().filter(|_| self.pinned_effort) {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort={}", json!(effort)));
        }
        cmd.arg("-C")
            .arg(&job.cwd)
            .arg("-o")
            .arg(&job.report)
            .arg("-")
            .current_dir(&job.cwd)
            .stdin(Stdio::from(prompt))
            .stdout(Stdio::from(stream))
            .stderr(Stdio::from(stream_err));
        match run(cmd, job.timeout)? {
            None => Ok(()),
            Some(code) => {
                let stream = fs::read_to_string(&job.stream).unwrap_or_default();
                Err(quota::codex_limit(&stream)
                    .map_or(ReviewerError::Exited(code), ReviewerError::OutOfQuota))
            }
        }
    }
}

/// The tools a headless Claude reviewer may not use: it reads and reports, and runs no skill
/// of its own (the review skill is a file it reads).
const CLAUDE_DISALLOWED: [&str; 4] = ["Edit", "Write", "NotebookEdit", "Skill"];

/// Claude Code, headless: `claude -p` with the user's settings only, no plugins and no MCP
/// servers, and no tool that edits.
#[derive(Clone, Debug, Default)]
pub struct Claude {
    model: Option<String>,
    effort: Option<String>,
    /// The plugins the user's settings enable, each turned off for the review.
    plugins: Vec<String>,
}

impl Claude {
    /// Claude on this `model` at this `effort`, with `claude_home` (`$CLAUDE_CONFIG_DIR`,
    /// else `~/.claude`) read for the plugins its user settings enable.
    pub fn new(model: Option<String>, effort: Option<String>, claude_home: &Path) -> Claude {
        let settings = fs::read_to_string(claude_home.join("settings.json")).unwrap_or_default();
        let plugins = serde_json::from_str::<Value>(&settings)
            .ok()
            .and_then(|settings| settings.get("enabledPlugins")?.as_object().cloned())
            .map(|plugins| plugins.keys().cloned().collect())
            .unwrap_or_default();
        Claude {
            model,
            effort,
            plugins,
        }
    }

    /// The `--settings` that turn off every plugin: each one the user's settings name, and
    /// those synced from claude.ai. Flag settings outrank user settings plugin by plugin.
    fn plugins_off(&self) -> String {
        let off: Map<String, Value> = self
            .plugins
            .iter()
            .map(|plugin| (plugin.clone(), Value::Bool(false)))
            .collect();
        json!({"enabledPlugins": off, "syncClaudeAiPlugins": false}).to_string()
    }
}

impl Reviewer for Claude {
    fn name(&self) -> &str {
        "claude"
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    fn opening(&self, base: &str) -> String {
        format!("Review the changes since {base}.")
    }

    fn stream_extension(&self) -> &str {
        "claude.json"
    }

    fn review(&self, job: &Job) -> Result<(), ReviewerError> {
        let prompt = File::open(&job.prompt).map_err(ReviewerError::Files)?;
        let stream = File::create(&job.stream).map_err(ReviewerError::Files)?;
        let log = File::create(job.stream.with_extension("log")).map_err(ReviewerError::Files)?;
        let mut cmd = Command::new("claude");
        cmd.arg("-p");
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &self.effort {
            cmd.args(["--effort", effort]);
        }
        cmd.args(["--output-format", "json", "--disallowedTools"])
            .args(CLAUDE_DISALLOWED)
            .args([
                "--setting-sources",
                "user",
                "--strict-mcp-config",
                "--settings",
            ])
            .arg(self.plugins_off())
            .arg("--append-system-prompt")
            .arg(review::STANDIN_REVIEWER)
            .env_remove("CLAUDE_CODE_PLUGIN_DIRS")
            .current_dir(&job.cwd)
            .stdin(Stdio::from(prompt))
            .stdout(Stdio::from(stream))
            .stderr(Stdio::from(log));
        if let Some(code) = run(cmd, job.timeout)? {
            return Err(ReviewerError::Exited(code));
        }
        let result = fs::read_to_string(&job.stream).unwrap_or_default();
        let report = serde_json::from_str::<Value>(&result)
            .ok()
            .and_then(|result| Some(result.get("result")?.as_str()?.to_owned()))
            .unwrap_or_default();
        fs::write(&job.report, report).map_err(ReviewerError::Files)
    }
}

/// Runs a backend's command: none when it succeeded, else the code it exited with.
fn run(cmd: Command, timeout: Duration) -> Result<Option<Option<i32>>, ReviewerError> {
    match process::run(cmd, timeout) {
        Ok(output) if output.status.success() => Ok(None),
        Ok(output) => Ok(Some(output.status.code())),
        Err(RunError::Spawn(e) | RunError::Wait(e)) => Err(ReviewerError::Start(e)),
        Err(RunError::TimedOut(timeout)) => Err(ReviewerError::TimedOut(timeout)),
    }
}

/// A tool's own dir: `$<var>`, else `~/<dir>`.
pub fn home_of(var: &str, dir: &str) -> PathBuf {
    let set = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
    set(var).map_or_else(
        || Path::new(&set("HOME").unwrap_or_default()).join(dir),
        PathBuf::from,
    )
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

    fn opening(&self, base: &str) -> String {
        format!("Review the changes since {base}.")
    }

    fn stream_extension(&self) -> &str {
        "fake.jsonl"
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
            ReviewerError::OutOfQuota(_) => write!(f, "is out of quota"),
        }
    }
}

impl std::error::Error for ReviewerError {}
