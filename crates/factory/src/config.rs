//! The factory's config file, `${XDG_CONFIG_HOME:-$HOME/.config}/factory/config.toml`.
//!
//! Its `[[review.backend]]` entries are the review backends, tried in the order they are
//! written (spec #1, "Reviewer router"). Without the file, or without entries, the backends are
//! Antigravity's Gemini 4 Neon at high effort, then Codex on its own config, then Claude Opus at
//! high effort.
//!
//! Its `[ci]` table sets how often a wait on CI polls GitHub when it can't listen for events.
//!
//! ```toml
//! [ci]
//! poll = "30s"             # a duration: 90s, 9m, 2h
//!
//! [[review.backend]]
//! cli = "codex"            # which adapter runs it: agy, codex or claude
//! name = "codex"           # its quota's name in the ledger and the daemon (default: cli)
//! model = "gpt-6.2-sol"    # default: the CLI's own
//! effort = "high"          # default: the CLI's own
//! timeout = 1800           # seconds before its process group is killed
//! concurrency = 2          # reviews it runs at once
//! when_busy = "queue"      # at its limit: "queue" for it, or take the "next" backend
//!
//! [[review.backend]]
//! cli = "agy"
//! model = "gemini-4-neon"  # default: the model chosen with agy's /model (the label kept
//!                          # under "model" in ~/.gemini/antigravity-cli/settings.json),
//!                          # else agy's default, Gemini 4 Neon; either way its quota is
//!                          # that model's group in agy's /usage
//! auth = "subscription"    # agy only: its "subscription" login, or an "api-key" in
//!                          # GEMINI_API_KEY (default: subscription)
//! ```

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_adapters::reviewer::AgyAuth;
use factory_core::quota::WhenBusy;
use serde::Deserialize;

use crate::output::Failure;
use crate::paths::Paths;

const DEFAULT_TIMEOUT_S: u64 = 1800;
const DEFAULT_CONCURRENCY: u32 = 2;
const DEFAULT_POLL: Duration = Duration::from_secs(30);

/// The CLI a backend runs on, which picks its adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cli {
    Agy,
    Codex,
    Claude,
}

/// One review backend, as configured.
#[derive(Clone, Debug)]
pub struct Backend {
    /// Its quota's name; two entries on one CLI (with models of separate quotas) differ here.
    pub name: String,
    pub cli: Cli,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout: Duration,
    pub concurrency: u32,
    pub when_busy: WhenBusy,
    /// How agy signs in; the other CLIs take none.
    pub auth: AgyAuth,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    review: Review,
    #[serde(default)]
    ci: CiSection,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CiSection {
    poll: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Review {
    #[serde(default)]
    backend: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    cli: Cli,
    name: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    timeout: Option<u64>,
    concurrency: Option<u32>,
    when_busy: Option<Busy>,
    auth: Option<AgyAuth>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Busy {
    Queue,
    Next,
}

/// How often a wait on CI polls GitHub when it can't listen for events.
pub fn ci_poll(paths: &Paths) -> Result<Duration, Failure> {
    let (path, file) = load(paths)?;
    let Some(poll) = file.ci.poll else {
        return Ok(DEFAULT_POLL);
    };
    match factory_core::duration::parse(&poll) {
        Ok(poll) if !poll.is_zero() => Ok(poll),
        Ok(_) => Err(Failure::precondition(format!(
            "{}: ci.poll is 0s; give it 1s or more",
            path.display()
        ))),
        Err(e) => Err(Failure::precondition(format!(
            "{}: ci.poll: {e}; fix the config",
            path.display()
        ))),
    }
}

/// The config file's path and contents; an absent file is an empty config.
fn load(paths: &Paths) -> Result<(PathBuf, ConfigFile), Failure> {
    let path = paths.config_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Failure::precondition(format!(
                "cannot read {}: {e}; check its permissions",
                path.display()
            )));
        }
    };
    let file = toml::from_str(&text).map_err(|e| bad(&path, &text, &e))?;
    Ok((path, file))
}

/// The review backends in the order they are tried.
pub fn review_backends(paths: &Paths) -> Result<Vec<Backend>, Failure> {
    let (path, file) = load(paths)?;
    if file.review.backend.is_empty() {
        return Ok(defaults());
    }
    let mut names = HashSet::new();
    file.review
        .backend
        .into_iter()
        .map(|entry| {
            let auth = entry.auth.is_some();
            let backend = backend(entry);
            if auth && backend.cli != Cli::Agy {
                return Err(Failure::precondition(format!(
                    "{}: backend {} sets auth, which only agy takes; remove it",
                    path.display(),
                    backend.name
                )));
            }
            if backend.concurrency == 0 {
                return Err(Failure::precondition(format!(
                    "{}: backend {} has concurrency 0; give it 1 or more",
                    path.display(),
                    backend.name
                )));
            }
            if !names.insert(backend.name.clone()) {
                return Err(Failure::precondition(format!(
                    "{}: two backends are named {}; give each its own name",
                    path.display(),
                    backend.name
                )));
            }
            Ok(backend)
        })
        .collect()
}

fn backend(entry: Entry) -> Backend {
    Backend {
        name: entry.name.unwrap_or_else(|| cli_name(entry.cli).to_owned()),
        cli: entry.cli,
        model: entry.model,
        effort: entry.effort,
        timeout: Duration::from_secs(entry.timeout.unwrap_or(DEFAULT_TIMEOUT_S)),
        concurrency: entry.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        when_busy: match entry.when_busy {
            Some(Busy::Next) => WhenBusy::Next,
            Some(Busy::Queue) | None => WhenBusy::Queue,
        },
        auth: entry.auth.unwrap_or_default(),
    }
}

fn defaults() -> Vec<Backend> {
    let entry = |cli, model: Option<&str>, effort: Option<&str>| {
        backend(Entry {
            cli,
            name: None,
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
            timeout: None,
            concurrency: None,
            when_busy: None,
            auth: None,
        })
    };
    vec![
        entry(Cli::Agy, Some("gemini-4-neon"), Some("high")),
        entry(Cli::Codex, None, None),
        entry(Cli::Claude, Some("opus"), Some("high")),
    ]
}

pub fn cli_name(cli: Cli) -> &'static str {
    match cli {
        Cli::Agy => "agy",
        Cli::Codex => "codex",
        Cli::Claude => "claude",
    }
}

/// A config the file can't be read as: its path, line and toml's reason, in one line.
fn bad(path: &Path, text: &str, e: &toml::de::Error) -> Failure {
    let line = e
        .span()
        .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
    let at = line.map_or_else(String::new, |line| format!(" line {line}"));
    Failure::precondition(format!(
        "{}{at}: {}; fix the config",
        path.display(),
        e.message().trim()
    ))
}
