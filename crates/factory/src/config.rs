//! The factory's config file, `${XDG_CONFIG_HOME:-$HOME/.config}/factory/config.toml`.
//!
//! Its `[[review.backend]]` entries are the review backends, each a pool of quota with its own
//! slots and cooldown (spec #1, "Reviewer router"), and `[review.chains]` names, per tier, the
//! pool, model and effort each step of that tier's chain runs (spec #1, story 31). A tier the
//! chains leave out tries the configured backends in their order, on their own model and
//! effort. Without backends, the pools and chains are review.sh's:
//!
//! - pools `agy-gemini` (with a 20% reserve) and `agy-claude` (agy's two usage groups: every
//!   Gemini model, Argon too, draws on the first), `codex` and `claude`, each with 2 slots;
//! - hard: Codex high, Gemini 4 Neon high, agy's Opus high, Claude Opus high;
//! - spec (reviews labelled `spec-*`): Gemini 4 Neon high, agy's Opus high, Codex high, Claude
//!   Opus high;
//! - standard: Codex medium, Neon medium, agy's Opus high, agy's Sonnet high, Claude Sonnet
//!   high;
//! - trivial: Flash high, Neon low, Codex low, Claude Sonnet medium.
//!
//! Argon is in no default chain, but a chain may name it (`agy-gemini:gemini-4-argon-high:high`).
//! An old config's `agy-argon` pool still works: configured, it is a pool of its own that reads
//! the shared Gemini group; named in a chain over the default pools, it is `agy-gemini`.
//!
//! A pool's `when_busy` left unset passes a busy pool to the next one in the chain, and waits
//! for a slot on the chain's last.
//!
//! Its `[ci]` table sets how often a wait on CI polls GitHub when it can't listen for events,
//! and its `[monitor]` table the context steward's thresholds.
//! Its `[notify]` table is the switch for event delivery: whether `factory wait --notify` may
//! post into the sessions that ask. It is off unless the owner switches it on.
//!
//! ```toml
//! [ci]
//! poll = "30s"             # a duration: 90s, 9m, 2h
//!
//! [monitor]
//! ask = 300000             # context at which an idle lead is asked to stop, then compacted
//! grace = "2m"             # how long an asked lead stays idle before it is compacted
//! subagent = 200000        # context at which a subagent is told about to its lead
//!
//! [notify]
//! enabled = false          # true lets the daemon post into Claude Code sessions
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
//! reserve = 0.2            # agy only: below this share of its group's 5-hour bucket, only
//!                          # hard and spec reviews may take it
//!
//! [review]
//! skill = "~/.agents/skills/code-review/SKILL.md"  # the reviewer skill every prompt carries
//!
//! [review.chains]          # per tier: "pool:model:effort", model and effort optional
//! hard = ["agy:gemini-4-neon-high:high", "codex:gpt-6.2-sol:high"]
//! spec = ["agy:gemini-4-neon-high:high"]   # reviews labelled spec-*; default: hard's chain when
//!                                          # set, else Neon high first (the old hard chain), or
//!                                          # the backends in their order when you list them
//! standard = ["codex:gpt-6.2-sol:medium", "agy"]
//! trivial = ["codex::low"]
//! ```
//!
//! The hooks (`factory hook`, spec #1, "Hooks") read the `[hook]` table:
//!
//! ```toml
//! [hook]
//! private_addresses = ["me@example.invalid"]  # a commit or push adding one is refused
//!
//! [hook.report]                # the template of a subagent's final report; no limit when absent
//! max_lines = 40
//! max_chars = 4000
//!
//! [hook.report.agent.implementer]  # an agent type's own limits, over the ones above
//! max_lines = 5
//! ```
//!
//! Its `[lead]` table sets how `factory lead spawn` starts Claude:
//!
//! ```toml
//! [lead]
//! model = "opus"                                # default: opus
//! effort = "medium"                             # default: medium
//! disable_plugins = ["expo@claude-plugins-official", "cloudflare@cloudflare"]  # the default
//! retry_pause = 2                               # seconds between tries to submit the prompt
//! ```

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_adapters::reviewer::AgyAuth;
use factory_core::hook::{Template, Templates};
use factory_core::quota::WhenBusy;
use factory_core::steward::Policy;
use factory_core::tier::Tier;
use serde::Deserialize;

use crate::output::Failure;
use crate::paths::Paths;

const DEFAULT_TIMEOUT_S: u64 = 1800;
const DEFAULT_CONCURRENCY: u32 = 2;
const DEFAULT_POLL: Duration = Duration::from_secs(30);
/// Today's ctx-monitor.sh threshold; Claude Code's own auto-compact near 380k is the backstop.
const DEFAULT_ASK: u64 = 300_000;
const DEFAULT_GRACE: Duration = Duration::from_secs(120);
/// Below the spec's target for the largest subagent context, 250k.
const DEFAULT_SUBAGENT: u64 = 200_000;

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
    /// What a review does when it is busy; unset, the chain decides: the next pool, or a wait
    /// on the chain's last.
    pub when_busy: Option<WhenBusy>,
    /// How agy signs in; the other CLIs take none.
    pub auth: AgyAuth,
    /// The share of agy's five-hour bucket kept for hard reviews.
    pub reserve: Option<f64>,
}

/// One step of a tier's chain: the backend it runs on, by its index, and the model and effort
/// it runs, else the backend's own.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub backend: usize,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The review backends and each tier's chain over them.
#[derive(Clone, Debug)]
pub struct ReviewSetup {
    pub backends: Vec<Backend>,
    pub hard: Vec<Step>,
    pub spec: Vec<Step>,
    pub standard: Vec<Step>,
    pub trivial: Vec<Step>,
    /// The reviewer skill the prompt carries, as configured.
    pub skill: Option<PathBuf>,
}

impl ReviewSetup {
    pub fn chain(&self, tier: Tier) -> &[Step] {
        match tier {
            Tier::Hard => &self.hard,
            Tier::Standard => &self.standard,
            Tier::Trivial => &self.trivial,
            Tier::Spec => &self.spec,
        }
    }
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    review: Review,
    #[serde(default)]
    lead: LeadSection,
    #[serde(default)]
    ci: CiSection,
    #[serde(default)]
    monitor: MonitorSection,
    #[serde(default)]
    notify: NotifySection,
    #[serde(default)]
    claude: ClaudeSection,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ClaudeSection {
    hold_at_percent: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct NotifySection {
    #[serde(default)]
    enabled: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct MonitorSection {
    ask: Option<u64>,
    grace: Option<String>,
    subagent: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CiSection {
    poll: Option<String>,
}

/// The file as the hooks read it: `[hook]` alone, so a mistake in `[review]` cannot stop a
/// hook and a mistake in `[hook]` cannot stop a review.
#[derive(Deserialize, Default)]
struct HookConfigFile {
    #[serde(default)]
    hook: HookFile,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HookFile {
    #[serde(default)]
    private_addresses: Vec<String>,
    #[serde(default)]
    report: ReportFile,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReportFile {
    max_lines: Option<usize>,
    max_chars: Option<usize>,
    #[serde(default)]
    agent: BTreeMap<String, TemplateFile>,
}

#[derive(Clone, Copy, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    max_lines: Option<usize>,
    max_chars: Option<usize>,
}

impl From<TemplateFile> for Template {
    fn from(file: TemplateFile) -> Template {
        Template {
            max_lines: file.max_lines,
            max_chars: file.max_chars,
        }
    }
}

/// What the hooks read from the config.
#[derive(Debug, Default)]
pub struct HookSettings {
    /// Addresses a commit or push must not add.
    pub private_addresses: Vec<String>,
    pub templates: Templates,
}

/// The hooks' settings from the config file in `config_dir`; defaults without the file.
pub fn hook_settings(config_dir: &Path) -> Result<HookSettings, Failure> {
    let path = config_dir.join("config.toml");
    let text = read(&path)?;
    // The message is left out of the failure: toml quotes the value it choked on, and the value
    // may be the private address the hook is there to keep out of the output.
    let hook = toml::from_str::<HookConfigFile>(&text)
        .map_err(|e| {
            bad(
                &path,
                &text,
                "a value is not what [hook] takes (see the docs in config.rs)",
                &e,
            )
        })?
        .hook;
    Ok(HookSettings {
        private_addresses: hook.private_addresses,
        templates: Templates {
            default: Template {
                max_lines: hook.report.max_lines,
                max_chars: hook.report.max_chars,
            },
            agents: hook
                .report
                .agent
                .into_iter()
                .map(|(agent, template)| (agent, template.into()))
                .collect(),
        },
    })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LeadSection {
    model: Option<String>,
    effort: Option<String>,
    disable_plugins: Option<Vec<String>>,
    retry_pause: Option<u64>,
}

/// How a lead's Claude is started, as configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadSetup {
    pub model: String,
    pub effort: String,
    /// Plugins (`name@marketplace`) the lead starts with turned off: the ones the factory never
    /// uses, whose skills would otherwise ride along on every request.
    pub disable_plugins: Vec<String>,
    /// How long to wait between tries to get the lead's prompt submitted.
    pub retry_pause: Duration,
}

const DEFAULT_LEAD_MODEL: &str = "opus";
const DEFAULT_LEAD_EFFORT: &str = "medium";
const DEFAULT_RETRY_PAUSE_S: u64 = 2;
/// The most a pause may be: a spawn tries the prompt up to five times, and the CLI waits for
/// all of them, so a longer pause only keeps a lead that needs one enter from its work.
const MAX_RETRY_PAUSE_S: u64 = 30;
/// The plugins the 2026-11-23 retro measured at about 6.3k tokens of skill listings per request.
const DEFAULT_DISABLED_PLUGINS: [&str; 2] =
    ["expo@claude-plugins-official", "cloudflare@cloudflare"];

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Review {
    #[serde(default)]
    backend: Vec<Entry>,
    #[serde(default)]
    chains: Chains,
    skill: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Chains {
    hard: Option<Vec<String>>,
    spec: Option<Vec<String>>,
    standard: Option<Vec<String>>,
    trivial: Option<Vec<String>>,
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
    reserve: Option<f64>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Busy {
    Queue,
    Next,
}

/// How often a wait on CI polls GitHub when it can't listen for events.
pub fn ci_poll(paths: &Paths) -> Result<Duration, Failure> {
    let (path, file) = load_config(paths)?;
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

/// Whether event delivery is switched on, and the file that says so: off without the file.
pub fn notify_enabled(paths: &Paths) -> Result<(bool, PathBuf), Failure> {
    let (path, file) = load_config(paths)?;
    Ok((file.notify.enabled, path))
}

/// The refusal of a `--notify` wait while event delivery is off. Only the owner switches it on.
pub fn notify_off(config: &Path) -> Failure {
    Failure::precondition(format!(
        "--notify is off: event delivery into sessions stays off until the owner sets \
         [notify] enabled = true in {}; wait without --notify",
        config.display()
    ))
}

/// The context steward's thresholds.
pub fn monitor_policy(paths: &Paths) -> Result<Policy, Failure> {
    let (path, file) = load_config(paths)?;
    let monitor = file.monitor;
    let grace = match monitor.grace {
        None => DEFAULT_GRACE,
        Some(grace) => factory_core::duration::parse(&grace).map_err(|e| {
            Failure::precondition(format!(
                "{}: monitor.grace: {e}; fix the config",
                path.display()
            ))
        })?,
    };
    Ok(Policy {
        ask_at: monitor.ask.unwrap_or(DEFAULT_ASK),
        grace,
        subagent_at: monitor.subagent.unwrap_or(DEFAULT_SUBAGENT),
    })
}

/// The config file's path and contents; an absent file is an empty config.
fn load_config(paths: &Paths) -> Result<(PathBuf, ConfigFile), Failure> {
    let path = paths.config_file();
    let file = load(&path)?;
    Ok((path, file))
}

/// The share of the Claude plan's 5-hour window, in percent, past which a lead holds new
/// dispatches: `[claude] hold_at_percent`, more than 0 and at most 100.
pub fn claude_hold_at(paths: &Paths) -> Result<f64, Failure> {
    let (path, file) = load_config(paths)?;
    match file.claude.hold_at_percent {
        None => Ok(factory_core::claude::HOLD_AT_PERCENT),
        Some(at) if at > 0.0 && at <= 100.0 => Ok(at),
        Some(at) => Err(Failure::precondition(format!(
            "{}: [claude] hold_at_percent is {at}; give it more than 0 and at most 100",
            path.display()
        ))),
    }
}

/// The review backends, each tier's chain over them, and the reviewer skill.
pub fn review_setup(paths: &Paths) -> Result<ReviewSetup, Failure> {
    let (path, file) = load_config(paths)?;
    let refuse = |what: String| Failure::precondition(format!("{}: {what}", path.display()));
    let review = file.review;
    let configured = !review.backend.is_empty();
    let backends = if configured {
        let mut names = HashSet::new();
        review
            .backend
            .into_iter()
            .map(|entry| {
                let (auth, reserve) = (entry.auth.is_some(), entry.reserve);
                let backend = backend(entry);
                if auth && backend.cli != Cli::Agy {
                    return Err(refuse(format!(
                        "backend {} sets auth, which only agy takes; remove it",
                        backend.name
                    )));
                }
                match reserve {
                    Some(_) if backend.cli != Cli::Agy => {
                        return Err(refuse(format!(
                            "backend {} sets reserve, which only agy takes; remove it",
                            backend.name
                        )));
                    }
                    Some(reserve) if !(0.0..1.0).contains(&reserve) => {
                        return Err(refuse(format!(
                            "backend {} has reserve {reserve}; give a share from 0 to below 1",
                            backend.name
                        )));
                    }
                    _ => {}
                }
                if backend.concurrency == 0 {
                    return Err(refuse(format!(
                        "backend {} has concurrency 0; give it 1 or more",
                        backend.name
                    )));
                }
                if !names.insert(backend.name.clone()) {
                    return Err(refuse(format!(
                        "two backends are named {}; give each its own name",
                        backend.name
                    )));
                }
                Ok(backend)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        default_backends()
    };
    let chain = |tier: Tier, given: Option<Vec<String>>| -> Result<Vec<Step>, Failure> {
        let Some(given) = given else {
            return Ok(if configured {
                (0..backends.len())
                    .map(|backend| Step {
                        backend,
                        model: None,
                        effort: None,
                    })
                    .collect()
            } else {
                default_chain(tier, &backends)
            });
        };
        if given.is_empty() {
            return Err(refuse(format!(
                "review.chains.{tier} is empty; name at least one \"pool:model:effort\""
            )));
        }
        given
            .iter()
            .map(|step| {
                parse_step(step, &backends)
                    .map_err(|why| refuse(format!("review.chains.{tier}: {why}; fix the config")))
            })
            .collect()
    };
    let chains = review.chains;
    Ok(ReviewSetup {
        hard: chain(Tier::Hard, chains.hard.clone())?,
        // A config that sets `hard` and not `spec` keeps its spec reviews on that chain, as before
        // the tier existed.
        spec: chain(Tier::Spec, chains.spec.or_else(|| chains.hard.clone()))?,
        standard: chain(Tier::Standard, chains.standard)?,
        trivial: chain(Tier::Trivial, chains.trivial)?,
        skill: review.skill.map(|skill| expand_home(&skill)),
        backends,
    })
}

/// The review backends, the pools the chains run on.
pub fn review_backends(paths: &Paths) -> Result<Vec<Backend>, Failure> {
    review_setup(paths).map(|setup| setup.backends)
}

/// A chain's step, `pool:model:effort`, with the model and effort optional. `agy-argon`, when no
/// pool has that name, is `agy-gemini`'s.
fn parse_step(step: &str, backends: &[Backend]) -> Result<Step, String> {
    let mut parts = step.splitn(3, ':');
    let pool = parts.next().unwrap_or_default();
    let given = |part: Option<&str>| part.filter(|part| !part.is_empty()).map(str::to_owned);
    let (model, effort) = (given(parts.next()), given(parts.next()));
    let named = |pool: &str| backends.iter().position(|backend| backend.name == pool);
    let backend = named(pool)
        .or_else(|| (pool == FOLDED.0).then(|| named(FOLDED.1)).flatten())
        .ok_or_else(|| format!("{step:?} names no backend {pool:?}"))?;
    Ok(Step {
        backend,
        model,
        effort,
    })
}

/// A path the config names, with a leading `~/` as the home directory.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map_or_else(
            || PathBuf::from(path),
            |home| PathBuf::from(home).join(rest),
        ),
        None => PathBuf::from(path),
    }
}

/// Reads the config file at `path`; an absent file is an empty config.
fn load(path: &Path) -> Result<ConfigFile, Failure> {
    let text = read(path)?;
    toml::from_str(&text).map_err(|e| bad(path, &text, e.message().trim(), &e))
}

/// The text of the config file at `path`; an absent file is empty.
fn read(path: &Path) -> Result<String, Failure> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Failure::precondition(format!(
                "cannot read {}: {e}; check its permissions",
                path.display()
            )));
        }
    };
    Ok(text)
}

/// How `factory lead spawn` starts Claude.
pub fn lead_setup(paths: &Paths) -> Result<LeadSetup, Failure> {
    let (path, file) = load_config(paths)?;
    let lead = file.lead;
    let pause = lead.retry_pause.unwrap_or(DEFAULT_RETRY_PAUSE_S);
    if pause > MAX_RETRY_PAUSE_S {
        return Err(Failure::usage(format!(
            "{}: [lead] retry_pause is {pause}s, more than the {MAX_RETRY_PAUSE_S}s a stalled \
             prompt is worth; lower it",
            path.display()
        )));
    }
    Ok(LeadSetup {
        model: lead.model.unwrap_or_else(|| DEFAULT_LEAD_MODEL.to_owned()),
        effort: lead
            .effort
            .unwrap_or_else(|| DEFAULT_LEAD_EFFORT.to_owned()),
        disable_plugins: lead.disable_plugins.unwrap_or_else(|| {
            DEFAULT_DISABLED_PLUGINS
                .iter()
                .map(|plugin| (*plugin).to_owned())
                .collect()
        }),
        retry_pause: Duration::from_secs(pause),
    })
}

fn backend(entry: Entry) -> Backend {
    Backend {
        name: entry.name.unwrap_or_else(|| cli_name(entry.cli).to_owned()),
        cli: entry.cli,
        model: entry.model,
        effort: entry.effort,
        timeout: Duration::from_secs(entry.timeout.unwrap_or(DEFAULT_TIMEOUT_S)),
        concurrency: entry.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        when_busy: entry.when_busy.map(|busy| match busy {
            Busy::Next => WhenBusy::Next,
            Busy::Queue => WhenBusy::Queue,
        }),
        auth: entry.auth.unwrap_or_default(),
        reserve: entry.reserve,
    }
}

/// review.sh's pools: agy's two usage groups, Codex and Claude, two slots each.
fn default_backends() -> Vec<Backend> {
    let pool = |name: &str, cli, reserve| Backend {
        reserve,
        ..backend(Entry {
            cli,
            name: Some(name.to_owned()),
            model: None,
            effort: None,
            timeout: None,
            concurrency: None,
            when_busy: None,
            auth: None,
            reserve: None,
        })
    };
    vec![
        pool("agy-gemini", Cli::Agy, Some(DEFAULT_GEMINI_RESERVE)),
        pool("agy-claude", Cli::Agy, None),
        pool("codex", Cli::Codex, None),
        pool("claude", Cli::Claude, None),
    ]
}

/// The share of Gemini's five-hour bucket kept for hard reviews, whichever Gemini model spends
/// it: Argon draws on the same group as Neon and Flash.
const DEFAULT_GEMINI_RESERVE: f64 = 0.2;

/// The pool an old config's `agy-argon` names when it configures no pool of that name: Argon
/// shares the Gemini group's quota (spec #47).
const FOLDED: (&str, &str) = ("agy-argon", "agy-gemini");

/// review.sh's chains, over [`default_backends`]. Argon left them (owner, 2026-11-24): it
/// shares the Gemini quota, and the replays' known findings went to the models kept. Config can
/// still name it.
///
/// A spec's own review keeps what hard was: Gemini Neon leads.
const DEFAULT_SPEC: [&str; 4] = [
    "agy-gemini:gemini-4-neon-high:high",
    "agy-claude:claude-opus-5-5-high:high",
    "codex:gpt-6.2-sol:high",
    "claude:opus:high",
];
/// A ticket's hard review leads with Codex (spec #67, story 23): 16 agy hard reviews used 16M
/// tokens, against 3M for 8 Codex ones.
const DEFAULT_HARD: [&str; 4] = [
    "codex:gpt-6.2-sol:high",
    "agy-gemini:gemini-4-neon-high:high",
    "agy-claude:claude-opus-5-5-high:high",
    "claude:opus:high",
];
/// Standard (owner, 2026-11-23): Codex medium leads, because Neon medium and Argon high missed
/// known hard findings in replays that Codex caught.
const DEFAULT_STANDARD: [&str; 5] = [
    "codex:gpt-6.2-sol:medium",
    "agy-gemini:gemini-4-neon-medium:medium",
    "agy-claude:claude-opus-5-5-high:high",
    "agy-claude:claude-sonnet-5-5-high:high",
    "claude:sonnet:high",
];
const DEFAULT_TRIVIAL: [&str; 4] = [
    "agy-gemini:gemini-3.8-flash-high:high",
    "agy-gemini:gemini-4-neon-low:low",
    "codex:gpt-6.2-sol:low",
    "claude:sonnet:medium",
];

fn default_chain(tier: Tier, backends: &[Backend]) -> Vec<Step> {
    let steps: &[&str] = match tier {
        Tier::Hard => &DEFAULT_HARD,
        Tier::Spec => &DEFAULT_SPEC,
        Tier::Standard => &DEFAULT_STANDARD,
        Tier::Trivial => &DEFAULT_TRIVIAL,
    };
    // Every step names a default pool; the acceptance tests run each chain.
    steps
        .iter()
        .filter_map(|step| parse_step(step, backends).ok())
        .collect()
}

pub fn cli_name(cli: Cli) -> &'static str {
    match cli {
        Cli::Agy => "agy",
        Cli::Codex => "codex",
        Cli::Claude => "claude",
    }
}

/// A config the file can't be read as: its path, line and toml's reason, in one line.
fn bad(path: &Path, text: &str, message: &str, e: &toml::de::Error) -> Failure {
    let line = e
        .span()
        .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
    let at = line.map_or_else(String::new, |line| format!(" line {line}"));
    Failure::precondition(format!("{}{at}: {message}; fix the config", path.display()))
}
