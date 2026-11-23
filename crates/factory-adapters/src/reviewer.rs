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
use std::time::{Duration, SystemTime};

use factory_core::quota::{self, Reset};
use factory_core::review;
use factory_core::timestamp::Timestamp;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::git::{GitRepo, Repo};
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
    /// The backend can't take a review now, for this reason (its usage query failed, say);
    /// the review goes on to the next one.
    Unready(String),
    /// The backend's sandbox would not be active, for this reason, so it must not review.
    NoSandbox(String),
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
    /// Checks, before a review is prepared for it, that the backend can take one: quota left,
    /// its sandbox active. [`ReviewerError::OutOfQuota`], [`ReviewerError::Unready`] and
    /// [`ReviewerError::NoSandbox`] send the review on to the next backend.
    fn preflight(&self) -> Result<(), ReviewerError> {
        Ok(())
    }
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

/// The agy tools a review may not use: it writes no file, opens no URL and calls no MCP server.
pub const AGY_DENIED: [&str; 4] = ["write_file(*)", "read_url(*)", "execute_url(*)", "mcp(*)"];
/// How long agy's usage query is trusted before it is asked again.
const AGY_USAGE_FRESH: Duration = Duration::from_secs(120);
/// How long agy's usage query and the sandbox check may take.
const AGY_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long after its own print timeout agy is killed with its process group.
const AGY_KILL_GRACE: Duration = Duration::from_secs(30);

/// How agy signs in, as the factory's config names it: `subscription` or `api-key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgyAuth {
    /// The subscription login agy keeps, whose quota its usage query shows.
    #[default]
    Subscription,
    /// A Gemini API key, read from `GEMINI_API_KEY`.
    ApiKey,
}

/// Antigravity, headless: `agy -p` in plan mode inside its sandbox, with writes, URLs and MCP
/// denied, JSON output, a print timeout, and no paid credits.
#[derive(Clone, Debug)]
pub struct Agy {
    model: Option<String>,
    effort: Option<String>,
    auth: AgyAuth,
    /// Where the last answer of its usage query is kept for reviews close together.
    usage_cache: PathBuf,
}

impl Agy {
    pub fn new(
        model: Option<String>,
        effort: Option<String>,
        auth: AgyAuth,
        usage_cache: PathBuf,
    ) -> Agy {
        Agy {
            model,
            effort,
            auth,
            usage_cache,
        }
    }

    /// Its `--settings`: no paid credits, commands run in the sandbox without asking, and the
    /// denied tools. With an API key it runs on the Gemini API.
    fn settings(&self) -> String {
        let mut settings = json!({
            "useG1Credits": false,
            "toolPermission": "proceed-in-sandbox",
            "allowNonWorkspaceAccess": true,
            "permissions": {"deny": AGY_DENIED},
        });
        if self.auth == AgyAuth::ApiKey {
            settings["modelProvider"] = json!("gemini");
        }
        settings.to_string()
    }

    /// agy as its auth runs it: the subscription login never sees an API key.
    fn command(&self) -> Command {
        let mut cmd = Command::new("agy");
        if self.auth == AgyAuth::Subscription {
            cmd.env_remove("GEMINI_API_KEY");
        }
        cmd.stdin(Stdio::null());
        cmd
    }

    /// What its usage query says, and whether that came from the cache: from the cache while
    /// it is fresh, unless `ask` says to ask again.
    fn usage(&self, ask: bool) -> Result<(String, bool), ReviewerError> {
        let fresh = fs::metadata(&self.usage_cache)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < AGY_USAGE_FRESH);
        if fresh
            && !ask
            && let Ok(usage) = fs::read_to_string(&self.usage_cache)
        {
            return Ok((usage, true));
        }
        let mut cmd = self.command();
        cmd.args(["-p", "/usage", "--output-format", "json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let unready = |why: String| ReviewerError::Unready(format!("its usage query {why}"));
        let output = process::run(cmd, AGY_CHECK_TIMEOUT).map_err(|e| unready(e.to_string()))?;
        if !output.status.success() {
            let code = output
                .status
                .code()
                .map_or("by a signal".to_owned(), |code| code.to_string());
            return Err(unready(format!("exited {code}")));
        }
        let usage = String::from_utf8_lossy(&output.stdout).into_owned();
        // A cache that can't be written only costs the next review another query. It is
        // replaced whole, so a review reading it alongside never sees half an answer.
        let mut fresh = self.usage_cache.clone().into_os_string();
        fresh.push(format!(".{}", std::process::id()));
        if fs::write(&fresh, &usage).is_ok() && fs::rename(&fresh, &self.usage_cache).is_err() {
            let _ = fs::remove_file(&fresh);
        }
        Ok((usage, false))
    }

    /// Whether the quota of its model's group has some left, by its usage query.
    fn quota_left(&self) -> Result<(), ReviewerError> {
        let group = quota::agy_group(self.model.as_deref().unwrap_or_default());
        let (usage, cached) = self.usage(false)?;
        let mut said = quota::agy_usage(&usage, group);
        // A cached answer may name an empty bucket whose reset has come since: ask again.
        if cached && matches!(said, quota::AgyUsage::Empty { until } if until <= now()) {
            said = quota::agy_usage(&self.usage(true)?.0, group);
        }
        // Read after the answer, so a reset that came while it was asked counts as come.
        let now = now();
        match said {
            quota::AgyUsage::Empty { until } if until > now => {
                Err(ReviewerError::OutOfQuota(Reset::On(until)))
            }
            // An empty bucket whose reset has passed has its quota back.
            quota::AgyUsage::Left | quota::AgyUsage::Empty { .. } => Ok(()),
            quota::AgyUsage::Unreadable(why) => Err(ReviewerError::Unready(format!(
                "its usage query is unreadable: {why}"
            ))),
        }
    }
}

/// The moment it is, by the system clock.
fn now() -> Timestamp {
    let since = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp::from_unix_millis(i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

impl Reviewer for Agy {
    fn name(&self) -> &str {
        "agy"
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    fn opening(&self, base: &str) -> String {
        format!("/code-review since {base}.")
    }

    fn stream_extension(&self) -> &str {
        "agy.json"
    }

    fn preflight(&self) -> Result<(), ReviewerError> {
        match self.auth {
            AgyAuth::Subscription => self.quota_left()?,
            AgyAuth::ApiKey => {
                if env::var_os("GEMINI_API_KEY").is_none_or(|key| key.is_empty()) {
                    return Err(ReviewerError::Unready(
                        "GEMINI_API_KEY is not set, and its auth is api-key".to_owned(),
                    ));
                }
            }
        }
        sandbox_active()
    }

    fn review(&self, job: &Job) -> Result<(), ReviewerError> {
        let prompt = fs::read_to_string(&job.prompt).map_err(ReviewerError::Files)?;
        let shared = GitRepo::at(&job.cwd)
            .common_dir()
            .map_err(|e| ReviewerError::Files(io::Error::other(e.to_string())))?;
        let stream = File::create(&job.stream).map_err(ReviewerError::Files)?;
        let log_path = job.stream.with_extension("log");
        let log = File::create(&log_path).map_err(ReviewerError::Files)?;
        let mut cmd = self.command();
        cmd.arg("-p")
            .arg(prompt)
            .args(["--mode", "plan", "--sandbox", "--add-dir"])
            .arg(shared);
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &self.effort {
            cmd.args(["--effort", effort]);
        }
        cmd.args(["--output-format", "json", "--print-timeout"])
            .arg(format!("{}s", job.timeout.as_secs()))
            .arg("--settings")
            .arg(self.settings())
            .current_dir(&job.cwd)
            .stdout(Stdio::from(stream))
            .stderr(Stdio::from(log));
        // agy stops itself at its print timeout; the grace is for an agy that doesn't. Either
        // way, nothing it started outlives it.
        let kill_at = job.timeout.saturating_add(AGY_KILL_GRACE);
        let failed = exit_of(process::run_swept(cmd, kill_at))?;
        let result = fs::read_to_string(&job.stream).unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&result).ok();
        let Some(code) = failed else {
            let report = parsed
                .as_ref()
                .and_then(|result| result.get("response")?.as_str())
                .unwrap_or_default();
            return fs::write(&job.report, report).map_err(ReviewerError::Files);
        };
        let finished = parsed
            .as_ref()
            .and_then(|result| result.get("finished_reason")?.as_str());
        if finished == Some("print_timeout") {
            return Err(ReviewerError::TimedOut(job.timeout));
        }
        let stderr = fs::read_to_string(&log_path).unwrap_or_default();
        match quota::agy_limit(&result, &stderr) {
            Some(reset) if code == Some(3) => Err(ReviewerError::OutOfQuota(reset)),
            _ => Err(ReviewerError::Exited(code)),
        }
    }
}

/// Whether agy's sandbox would be active: it needs unprivileged user namespaces, and without
/// them `--sandbox` silently does nothing.
fn sandbox_active() -> Result<(), ReviewerError> {
    let mut cmd = Command::new("unshare");
    cmd.args(["--user", "--map-root-user", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let why = match process::run(cmd, AGY_CHECK_TIMEOUT) {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let said = stderr.lines().find(|line| !line.trim().is_empty());
            match (said, output.status.code()) {
                (Some(said), _) => said.trim().to_owned(),
                (None, Some(code)) => format!("unshare exited {code}"),
                (None, None) => "unshare was killed by a signal".to_owned(),
            }
        }
        Err(e) => format!("unshare: {e}"),
    };
    Err(ReviewerError::NoSandbox(format!(
        "its sandbox is not active (unprivileged user namespaces are unavailable: {why})"
    )))
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
    exit_of(process::run(cmd, timeout))
}

/// How a backend's command ended: none when it succeeded, else the code it exited with.
fn exit_of(
    ran: Result<std::process::Output, RunError>,
) -> Result<Option<Option<i32>>, ReviewerError> {
    match ran {
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
            ReviewerError::Unready(why) | ReviewerError::NoSandbox(why) => {
                write!(f, "skipped: {why}")
            }
        }
    }
}

impl std::error::Error for ReviewerError {}
