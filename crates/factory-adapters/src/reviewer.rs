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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use factory_core::agy::{self, Sandbox};
use factory_core::claude;
use factory_core::quota::{self, Reset};
use factory_core::review;
use factory_core::timestamp::Timestamp;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::git::{Checkout, Git, Repo, Repos};
use crate::guard;
use crate::process::{self, RunError};

/// One review for a backend to run.
#[derive(Clone, Debug)]
pub struct Job {
    /// The repository under review; the backend runs read-only in it, or in a checkout of it.
    pub cwd: PathBuf,
    /// The commit under review, the head the bundle was made from.
    pub head: String,
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
    /// The backend can't take a review now, for this reason (its quota is kept for hard
    /// reviews, say); the review goes on to the next one.
    Unready(String),
    /// The backend's usage answer, which says whether it has quota, could not be had or read,
    /// for this reason; the review goes on to the next one, and says so.
    NoUsage(String),
    /// The backend's sandbox would not be active, or was not, for this reason, so it must
    /// not review, or its review is not kept.
    NoSandbox(String),
    /// The head under review could not be checked out for the backend, for this reason.
    Checkout(String),
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
    /// its sandbox active. [`ReviewerError::OutOfQuota`], [`ReviewerError::Unready`],
    /// [`ReviewerError::NoUsage`] and [`ReviewerError::NoSandbox`] send the review on to the
    /// next backend.
    fn preflight(&self) -> Result<(), ReviewerError> {
        Ok(())
    }
    /// Runs the review to its end, leaving the report at `job.report`.
    fn review(&self, job: &Job) -> Result<(), ReviewerError>;
}

/// The Codex features a review never uses.
const CODEX_UNUSED: [&str; 8] = [
    "image_generation",
    "browser_use",
    "computer_use",
    "in_app_browser",
    "plugins",
    "goals",
    "tool_suggest",
    "multi_agent",
];

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
        // A review reads one bundle, so the tools it never uses are off: about 3.7k tokens of
        // tool definitions per request.
        cmd.args(["-c", "web_search=\"disabled\""]);
        for feature in CODEX_UNUSED {
            cmd.args(["--disable", feature]);
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

/// agy's free usage query: its JSON answer as it printed it, or why there is none (`exited
/// 127`, `timed out after 60s`).
pub trait AgyUsage: Send + Sync {
    fn query(&self) -> Result<String, String>;
}

/// `agy -p /usage --output-format json` from PATH, as `auth` runs agy.
pub struct AgyUsageCli(pub AgyAuth);

impl AgyUsage for AgyUsageCli {
    fn query(&self) -> Result<String, String> {
        agy_usage_query(self.0)
    }
}

/// A usage query that answers as it was told, and counts how often it was asked.
pub struct FakeAgyUsage {
    pub answer: Result<String, String>,
    pub asked: std::sync::atomic::AtomicUsize,
}

impl AgyUsage for FakeAgyUsage {
    fn query(&self) -> Result<String, String> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answer.clone()
    }
}

fn agy_usage_query(auth: AgyAuth) -> Result<String, String> {
    let mut cmd = Command::new("agy");
    if auth == AgyAuth::Subscription {
        cmd.env_remove("GEMINI_API_KEY");
    }
    cmd.args(["-p", "/usage", "--output-format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = process::run(cmd, AGY_CHECK_TIMEOUT).map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "exited {}",
            output
                .status
                .code()
                .map_or("by a signal".to_owned(), |code| code.to_string())
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

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
/// denied, JSON output, a print timeout, and no paid credits. It reviews a throwaway detached
/// checkout of the head, so a terminal command that writes changes nothing of the caller's,
/// under a write guard ([`guard`]) that leaves it nothing else to write but its own log and
/// agy's state; its review is kept only when its own log says its sandbox started.
///
/// With no model configured it runs the model chosen in agy, and its quota is that model's
/// group: the label agy's `/model` saved under `model` in
/// `~/.gemini/antigravity-cli/settings.json`, else agy's default, Gemini 4 Neon, whose group is
/// "Gemini Models".
#[derive(Clone, Debug)]
pub struct Agy {
    model: Option<String>,
    effort: Option<String>,
    auth: AgyAuth,
    /// Where the last answer of its usage query is kept for reviews close together.
    usage_cache: PathBuf,
    /// Where its throwaway checkouts go, one per review process: `<repo>-<pid>`.
    checkouts: PathBuf,
    /// The repositories it checks the head out of.
    repos: Arc<dyn Repos>,
    /// Whether its sandbox can start.
    namespaces: Arc<dyn Namespaces>,
    /// The share of its group's five-hour bucket this review leaves alone: below it, the
    /// review is not its to take.
    reserve: Option<f64>,
}

impl Agy {
    pub fn new(
        model: Option<String>,
        effort: Option<String>,
        auth: AgyAuth,
        usage_cache: PathBuf,
        checkouts: PathBuf,
    ) -> Agy {
        Agy {
            model,
            effort,
            auth,
            usage_cache,
            checkouts,
            repos: Arc::new(Git),
            namespaces: Arc::new(Unshare),
            reserve: None,
        }
    }

    /// The same backend, keeping `reserve` of its group's five-hour bucket for other reviews.
    pub fn reserving(mut self, reserve: Option<f64>) -> Agy {
        self.reserve = reserve;
        self
    }

    /// The same backend, reaching repositories through `repos` and asking `namespaces` whether
    /// its sandbox can start: how a test hands it fakes.
    pub fn with(mut self, repos: Arc<dyn Repos>, namespaces: Arc<dyn Namespaces>) -> Agy {
        self.repos = repos;
        self.namespaces = namespaces;
        self
    }

    /// The model whose quota a review draws on: the configured one, else the one chosen in
    /// agy's settings, else agy's default.
    fn quota_model(&self) -> String {
        self.model.clone().unwrap_or_else(|| {
            let home = env::var_os("HOME").unwrap_or_default();
            let settings = Path::new(&home).join(".gemini/antigravity-cli/settings.json");
            quota::agy_chosen_model(fs::read_to_string(settings).ok().as_deref())
        })
    }

    /// A detached checkout of the head under review, outside the repo, and the directory in it
    /// that mirrors the caller's. What a review that died before removing its own left here is
    /// removed first.
    fn checkout(&self, job: &Job) -> Result<(Checkout, PathBuf), ReviewerError> {
        let failed = |e: crate::git::GitError| ReviewerError::Checkout(e.to_string());
        let repo = self.repos.at(&job.cwd);
        let name = repo.name().map_err(failed)?;
        let top = repo.toplevel().map_err(failed)?;
        sweep_checkouts(&self.checkouts, repo.as_ref());
        let here = self
            .checkouts
            .join(format!("{name}-{}", std::process::id()));
        let checkout = repo.checkout(&job.head, &here).map_err(failed)?;
        let within = job
            .cwd
            .strip_prefix(&top)
            .map(|sub| checkout.dir().join(sub))
            .ok()
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| checkout.dir().to_owned());
        Ok((checkout, within))
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

    /// How a review on its auth reaches the session bus: the subscription login reads its
    /// token from the keyring over it; an API key needs none of it.
    fn bus(&self) -> guard::Bus {
        match self.auth {
            AgyAuth::Subscription => guard::Bus::Keyring,
            AgyAuth::ApiKey => guard::Bus::Closed,
        }
    }

    /// `cmd`, which runs agy, as its auth runs it.
    fn as_its_auth(&self, mut cmd: Command) -> Command {
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
        let usage = agy_usage_query(self.auth)
            .map_err(|why| ReviewerError::NoUsage(format!("its usage query {why}")))?;
        // A cache that can't be written only costs the next review another query. It is
        // replaced whole, so a review reading it alongside never sees half an answer.
        let mut fresh = self.usage_cache.clone().into_os_string();
        fresh.push(format!(".{}", std::process::id()));
        if fs::write(&fresh, &usage).is_ok() && fs::rename(&fresh, &self.usage_cache).is_err() {
            let _ = fs::remove_file(&fresh);
        }
        Ok((usage, false))
    }

    /// Whether the quota of its model's group has some left, by its usage query, and more
    /// than the reserve this review must leave.
    fn quota_left(&self) -> Result<(), ReviewerError> {
        let model = self.quota_model();
        let (mut usage, cached) = self.usage(false)?;
        let mut group = quota::agy_group(&usage, &model);
        let mut said = quota::agy_usage(&usage, &group);
        // A cached answer may name an empty bucket whose reset has come since: ask again.
        if cached && matches!(said, quota::AgyUsage::Empty { until } if until <= now()) {
            usage = self.usage(true)?.0;
            group = quota::agy_group(&usage, &model);
            said = quota::agy_usage(&usage, &group);
        }
        if let Some(reserve) = self.reserve
            && matches!(said, quota::AgyUsage::Left)
            && quota::agy_below(&usage, &group, "5h", reserve)
        {
            return Err(ReviewerError::Unready(format!(
                "its {group} 5-hour bucket is under {:.0}%, kept for hard reviews",
                reserve * 100.0
            )));
        }
        // Read after the answer, so a reset that came while it was asked counts as come.
        let now = now();
        match said {
            quota::AgyUsage::Empty { until } if until > now => {
                Err(ReviewerError::OutOfQuota(Reset::On(until)))
            }
            // An empty bucket whose reset has passed has its quota back.
            quota::AgyUsage::Left | quota::AgyUsage::Empty { .. } => Ok(()),
            quota::AgyUsage::Unreadable(why) => Err(ReviewerError::NoUsage(format!(
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
        sandbox_active(self.namespaces.as_ref(), self.bus())
    }

    fn review(&self, job: &Job) -> Result<(), ReviewerError> {
        let prompt = fs::read_to_string(&job.prompt).map_err(ReviewerError::Files)?;
        let shared = self
            .repos
            .at(&job.cwd)
            .common_dir()
            .map_err(|e| ReviewerError::Files(io::Error::other(e.to_string())))?;
        let stream = File::create(&job.stream).map_err(ReviewerError::Files)?;
        let log_path = job.stream.with_extension("log");
        let log = File::create(&log_path).map_err(ReviewerError::Files)?;
        // agy's own log, which says whether its sandbox started. It is emptied first, as agy
        // may add to it.
        let own_log = job.stream.with_extension("run.log");
        File::create(&own_log).map_err(ReviewerError::Files)?;
        // Dropped, so removed, however the review ends.
        let (checkout, within) = self.checkout(job)?;
        // Under the write guard, which leaves it its checkout, its own log and agy's state, and
        // which stays up until the review is done.
        let guard = guard::Guard::start(&[checkout.dir(), own_log.as_path()], self.bus())
            .map_err(guard_down)?;
        let mut cmd = self.as_its_auth(guard.command("agy"));
        cmd.arg("-p")
            .arg(prompt)
            .args(["--mode", "plan", "--sandbox", "--add-dir"])
            .arg(shared)
            .arg("--log-file")
            .arg(&own_log);
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
            .current_dir(&within)
            .stdout(Stdio::from(stream))
            .stderr(Stdio::from(log));
        // agy stops itself at its print timeout; the grace is for an agy that doesn't. Either
        // way, nothing it started outlives it.
        let kill_at = job.timeout.saturating_add(AGY_KILL_GRACE);
        let failed = exit_of(process::run_swept(cmd, kill_at))?;
        let result = fs::read_to_string(&job.stream).unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&result).ok();
        let Some(code) = failed else {
            sandbox_started(&own_log)?;
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

/// Whether agy's own log, at `path`, says its sandbox started: a review it ran without one is
/// not kept.
fn sandbox_started(path: &Path) -> Result<(), ReviewerError> {
    let log = fs::read_to_string(path).unwrap_or_default();
    let why = match agy::sandbox(&log) {
        Sandbox::Started => return Ok(()),
        Sandbox::Unavailable(why) => why,
        Sandbox::Unknown => "its log never says it started".to_owned(),
    };
    Err(ReviewerError::NoSandbox(format!(
        "its sandbox did not start ({why}); see {}",
        path.display()
    )))
}

/// Removes the checkouts in `dir` whose review process has died, which never removed its own,
/// and has `repo` forget them. A checkout is named `<repo>-<pid>`.
fn sweep_checkouts(dir: &Path, repo: &dyn Repo) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut swept = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|name| name.rsplit_once('-'))
            .and_then(|(_, pid)| pid.parse::<u32>().ok());
        let Some(pid) = pid else { continue };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            swept |= fs::remove_dir_all(entry.path()).is_ok();
        }
    }
    if swept {
        let _ = repo.prune_worktrees();
    }
}

/// Whether unprivileged user namespaces are available, which agy's sandbox needs: without
/// them `--sandbox` silently does nothing. And whether the write guard ([`guard`]) a review
/// runs under starts, with agy's sandbox able to start inside it.
pub trait Namespaces: fmt::Debug + Send + Sync {
    /// `Ok`, or why they are unavailable, in the probe's own words.
    fn user_namespaces(&self) -> Result<(), String>;
    /// `Ok`, or why the guard, reaching the bus as `bus` says, can't start, in the probe's
    /// own words.
    fn guard(&self, bus: guard::Bus) -> Result<(), String>;
}

/// The probes through `unshare --user --map-root-user true`, bare and under the guard.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unshare;

/// The arguments that make `unshare` start a user namespace and do nothing in it.
const UNSHARE_PROBE: [&str; 3] = ["--user", "--map-root-user", "true"];

impl Unshare {
    /// Runs `cmd`, a probe whose program is `program`: `Ok`, or what it said.
    fn probe(mut cmd: Command, program: &str) -> Result<(), String> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match process::run(cmd, AGY_CHECK_TIMEOUT) {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let said = stderr.lines().find(|line| !line.trim().is_empty());
                Err(match (said, output.status.code()) {
                    (Some(said), _) => said.trim().to_owned(),
                    (None, Some(code)) => format!("{program} exited {code}"),
                    (None, None) => format!("{program} was killed by a signal"),
                })
            }
            Err(e) => Err(format!("{program}: {e}")),
        }
    }
}

impl Namespaces for Unshare {
    fn user_namespaces(&self) -> Result<(), String> {
        let mut cmd = Command::new("unshare");
        cmd.args(UNSHARE_PROBE);
        Unshare::probe(cmd, "unshare")
    }

    fn guard(&self, bus: guard::Bus) -> Result<(), String> {
        let guard = guard::Guard::start(&[], bus)?;
        let mut cmd = guard.command("unshare");
        cmd.args(UNSHARE_PROBE);
        Unshare::probe(cmd, guard::BWRAP)
    }
}

/// A probe that answers as it was told: `None` for available, else the reason they are not;
/// the same for the guard.
#[derive(Clone, Debug, Default)]
pub struct FakeNamespaces {
    pub namespaces: Option<String>,
    pub guard: Option<String>,
}

impl Namespaces for FakeNamespaces {
    fn user_namespaces(&self) -> Result<(), String> {
        self.namespaces.clone().map_or(Ok(()), Err)
    }

    fn guard(&self, _bus: guard::Bus) -> Result<(), String> {
        self.guard.clone().map_or(Ok(()), Err)
    }
}

/// Whether agy's sandbox would be active, and its write guard start, as `namespaces` says.
fn sandbox_active(namespaces: &dyn Namespaces, bus: guard::Bus) -> Result<(), ReviewerError> {
    namespaces.user_namespaces().map_err(|why| {
        ReviewerError::NoSandbox(format!(
            "its sandbox is not active (unprivileged user namespaces are unavailable: {why})"
        ))
    })?;
    namespaces.guard(bus).map_err(guard_down)
}

/// A review's sandbox, when its write guard can't start, for `why`.
fn guard_down(why: String) -> ReviewerError {
    ReviewerError::NoSandbox(format!(
        "its sandbox is not active (its write guard can't start: {why})"
    ))
}

/// The tools a headless Claude reviewer may not use: it reads and reports, and runs no skill
/// of its own (the review skill is a file it reads).
const CLAUDE_DISALLOWED: [&str; 8] = [
    "Edit",
    "Write",
    "NotebookEdit",
    "Skill",
    "Agent",
    "Task",
    "WebFetch",
    "WebSearch",
];

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
        let outcome = run(cmd, job.timeout)?;
        let result = fs::read_to_string(&job.stream).unwrap_or_default();
        let report = serde_json::from_str::<Value>(&result)
            .ok()
            .and_then(|result| Some(result.get("result")?.as_str()?.to_owned()))
            .unwrap_or_default();
        if let Some(code) = outcome {
            // Its failure's own words are its `result` and its stderr, not what it read.
            let log = fs::read_to_string(job.stream.with_extension("log")).unwrap_or_default();
            let said = format!("{report}\n{log}");
            return Err(claude::failure_limit(&said)
                .map_or(ReviewerError::Exited(code), ReviewerError::OutOfQuota));
        }
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
            ReviewerError::Unready(why)
            | ReviewerError::NoUsage(why)
            | ReviewerError::NoSandbox(why) => {
                write!(f, "skipped: {why}")
            }
            ReviewerError::Checkout(why) => write!(f, "has no checkout to review in: {why}"),
        }
    }
}

impl std::error::Error for ReviewerError {}

#[cfg(test)]
mod seams {
    use super::*;
    use crate::git::FakeRepo;

    fn agy(checkouts: &Path, repo: &FakeRepo, namespaces: Option<&str>) -> Agy {
        Agy::new(
            None,
            None,
            AgyAuth::ApiKey,
            checkouts.join("usage.json"),
            checkouts.to_owned(),
        )
        .with(
            Arc::new(repo.clone()),
            Arc::new(FakeNamespaces {
                namespaces: namespaces.map(str::to_owned),
                guard: None,
            }),
        )
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("factory-seams-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_checkout_mirrors_the_callers_directory_and_goes_when_dropped() {
        let dir = scratch("checkout");
        let top = dir.join("repo");
        fs::create_dir_all(top.join("app")).unwrap();
        let repo = FakeRepo {
            name: "repo".to_owned(),
            toplevel: top.clone(),
            ..FakeRepo::default()
        };
        // What a review process that died left behind is swept.
        let stale = dir.join("repo-999999999");
        fs::create_dir_all(&stale).unwrap();
        let agy = agy(&dir, &repo, None);
        let job = Job {
            cwd: top.join("app"),
            head: "abc".to_owned(),
            prompt: dir.join("prompt"),
            report: dir.join("report"),
            stream: dir.join("stream"),
            timeout: Duration::from_secs(1),
        };
        fs::create_dir_all(dir.join(format!("repo-{}", std::process::id())).join("app")).unwrap();
        let (checkout, within) = agy.checkout(&job).unwrap();
        let here = dir.join(format!("repo-{}", std::process::id()));
        assert_eq!(checkout.dir(), here);
        assert_eq!(within, here.join("app"));
        assert!(!stale.exists());
        assert_eq!(*repo.checkouts.lock().unwrap(), vec![here.clone()]);
        drop(checkout);
        assert!(repo.checkouts.lock().unwrap().is_empty());
        assert!(!here.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_user_namespaces_sends_the_review_on_in_the_probes_words() {
        let off = FakeNamespaces {
            namespaces: Some("unshare: Operation not permitted".to_owned()),
            guard: None,
        };
        match sandbox_active(&off, guard::Bus::Keyring) {
            Err(ReviewerError::NoSandbox(why)) => assert_eq!(
                why,
                "its sandbox is not active (unprivileged user namespaces are unavailable: \
                 unshare: Operation not permitted)"
            ),
            other => panic!("expected NoSandbox, got {other:?}"),
        }
        assert!(sandbox_active(&FakeNamespaces::default(), guard::Bus::Keyring).is_ok());
    }

    #[test]
    fn a_guard_that_cannot_start_sends_the_review_on_in_the_probes_words() {
        let off = FakeNamespaces {
            namespaces: None,
            guard: Some("bwrap: No such file or directory".to_owned()),
        };
        match sandbox_active(&off, guard::Bus::Keyring) {
            Err(ReviewerError::NoSandbox(why)) => assert_eq!(
                why,
                "its sandbox is not active (its write guard can't start: \
                 bwrap: No such file or directory)"
            ),
            other => panic!("expected NoSandbox, got {other:?}"),
        }
    }
}
