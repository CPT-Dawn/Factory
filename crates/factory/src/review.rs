//! `factory review [--tier] <base> <label> [spec] [prev]`: review.sh's arguments, a prepared review
//! bundle, a run on the first step of the tier's chain the daemon's router grants a slot, and
//! one line back.
//!
//! The tier picks the chain (spec review hard; small or docs-only diff trivial; else the
//! caller's, or standard), and each step names a pool, a model and an effort. The daemon holds
//! each pool's slots and cooldown. A pool's preflight runs before the review is prepared for
//! it: one out of quota is cooled down until its reset, and one that can't review now (agy
//! without an active sandbox or quota above its reserve, or whose usage answer can't be read)
//! is skipped for this run. A pool skipped for its usage answer says so: a line on stderr, and
//! `factory status` shows its quota unknown with the reason. A step that runs out of quota partway hands the review to the chain's next step: the
//! same pool's next model when that is next, else the next pool, once this one is cooled down.
//!
//! Every file of a review is kept in `<state>/reviews/<repo>/`, named by its label: the
//! bundle (`.bundle.md`), the prompt (`.prompt`), the report (`.md`), the reviewer's run record
//! (Codex `.jsonl`, agy `.agy.json`, Claude `.claude.json`, which factory-stats reads), the spec an
//! issue ref names (`.spec.md`), the builder's evidence (`.evidence.md`), review.sh's ledger of the
//! run (`.ledger.json`) and the findings ledger read from the report (`.findings.json`, not
//! `.jsonl`, which factory-stats would read as a stream). Each run lands one `review` event in the
//! ledger, with review.sh's fields.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use factory_adapters::gh::{Gh, Issues};
use factory_adapters::git::{GitError, GitRepo, Repo};
use factory_adapters::reviewer::{Agy, Claude, Codex, Job, Reviewer, ReviewerError, home_of};
use factory_core::exit::Exit;
use factory_core::quota::{Reset, WhenBusy};
use factory_core::review::{
    self, Bundle, Doc, Finding, Fix, Part, Previous, Reading, Record, Verdict,
};
use factory_core::tier::{self, Tier};
use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};

use crate::client;
use crate::clock;
use crate::config::{self, Backend, Cli, ReviewSetup, Step};
use crate::log;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{
    AcquireParams, AcquireResult, BackendSlots, QuotaParam, QuotaParams, REVIEW_ACQUIRE,
    REVIEW_QUOTA, ReviewRef,
};

/// Lines of context around each change in the bundle's diff, so the reviewer rarely opens a
/// file to see what surrounds a hunk.
const DIFF_CONTEXT: u32 = 15;
/// The files a repo documents its standards in, at its top level: the first there is counts.
const STANDARDS: [&str; 4] = [
    "CODING_STANDARDS.md",
    "CONTRIBUTING.md",
    "CLAUDE.md",
    "AGENTS.md",
];
/// Where the implementer leaves its evidence, in the git dir so it is never committed.
const EVIDENCE: &str = "factory-evidence.md";
/// The longest a review may run, in seconds: a week.
const MAX_TIMEOUT_S: u64 = 7 * 24 * 60 * 60;
/// A touched doc larger than this is named in the bundle, not carried.
const DOC_BYTES: u64 = 12 * 1024;
/// The reviewer skill a prompt carries when neither the caller nor the config names one.
const DEFAULT_SKILL: &str = ".agents/skills/code-review/SKILL.md";

pub struct Args {
    pub base: String,
    pub label: String,
    pub spec: Option<String>,
    pub previous: Option<PathBuf>,
    /// Overrides every backend's configured timeout.
    pub timeout: Option<Duration>,
    /// The ticket's tier, as the caller named it.
    pub tier: Option<String>,
    /// The reviewer skill the prompt carries, over the config's.
    pub skill: Option<PathBuf>,
}

pub fn run(args: &Args) -> Result<Done, Failure> {
    review::check_label(&args.label).map_err(|e| Failure::usage(e.to_string()))?;
    let caller = args
        .tier
        .as_deref()
        .map(|tier| {
            Tier::parse(tier).ok_or_else(|| {
                Failure::usage(format!("tier must be hard, standard or trivial: {tier}"))
            })
        })
        .transpose()?;
    let paths = Paths::from_env()?;
    let started = Instant::now();
    let setup = config::review_setup(&paths);
    let mut run = Run::default();
    let result = match &setup {
        Ok(setup) => review(args, caller, &paths, setup, &mut run),
        Err(failure) => Err(Failure::new(failure.exit, failure.message.clone())),
    };
    // A skip the spec has reported (an inactive sandbox) stays in the line of a run that fails.
    let result = result.map_err(|failure| match run.notes.is_empty() {
        true => failure,
        false => Failure::new(
            failure.exit,
            format!("{}; {}", failure.message, run.notes.join("; ")),
        ),
    });
    let failed = result.as_ref().err().map(|failure| failure.message.clone());
    let chosen = run.chosen.as_ref();
    let fallback_from: Vec<&str> = run.cooled.iter().map(|(name, _)| name.as_str()).collect();
    let fallback_from = fallback_from.join(",");
    let skipped: Vec<&str> = run.skipped.iter().map(|(name, _)| name.as_str()).collect();
    let skipped = skipped.join(",");
    let record = Record {
        repo: &run.repo,
        label: &args.label,
        round: review::round(&args.label, args.previous.is_some()),
        base: &args.base,
        head: &run.head,
        spec: &run.spec,
        backend: chosen.map_or("", |chosen| chosen.pool.as_str()),
        reviewer: chosen.map_or("", |chosen| config::cli_name(chosen.cli)),
        model: chosen.and_then(|chosen| chosen.model.as_deref()),
        effort: chosen.and_then(|chosen| chosen.effort.as_deref()),
        tier: match run.tier {
            Some(tier) => tier.as_str(),
            None => args.tier.as_deref().unwrap_or_default(),
        },
        tier_source: run.tier_source,
        tokens: run.tokens,
        branch: &run.branch,
        files: &run.files,
        bundle_bytes: run.bundle_bytes,
        duration_s: started.elapsed().as_secs(),
        fallback_from: &fallback_from,
        skipped: &skipped,
        report: &run.report,
        stream: &run.stream,
        findings: &run.findings,
        from: &run.from,
        result: match (&run.reading, &failed) {
            (Some(reading), None) => Ok(reading),
            _ => Err(failed
                .as_deref()
                .unwrap_or("the run ended without a reading")),
        },
    };
    let pairs = record.pairs();
    let logged = log::land(&paths, "review", &pairs);
    match (result, logged) {
        (result, Ok(_)) => result,
        (Ok(done), Err(e)) => Err(Failure::precondition(format!(
            "{}; but its event was not logged: {}",
            done.line, e.message
        ))),
        (Err(failure), Err(e)) => Err(Failure::new(
            failure.exit,
            format!(
                "{}; its event was not logged either: {}",
                failure.message, e.message
            ),
        )),
    }
}

/// The adapter that runs a chain's step on its backend, keeping the backend's reserve unless
/// the review is hard.
fn reviewer(backend: &Backend, step: &Step, tier: Tier, paths: &Paths) -> Box<dyn Reviewer> {
    let model = step.model.clone().or_else(|| backend.model.clone());
    let effort = step.effort.clone().or_else(|| backend.effort.clone());
    match backend.cli {
        Cli::Agy => Box::new(
            Agy::new(
                model,
                effort,
                backend.auth,
                paths.state.join("agy-usage.json"),
                paths.state.join("checkouts"),
            )
            .reserving(backend.reserve.filter(|_| tier != Tier::Hard)),
        ),
        Cli::Codex => Box::new(Codex::new(model, effort, &home_of("CODEX_HOME", ".codex"))),
        Cli::Claude => Box::new(Claude::new(
            model,
            effort,
            &home_of("CLAUDE_CONFIG_DIR", ".claude"),
        )),
    }
}

/// The step that runs (or last ran) the review, as its event names it.
struct Chosen {
    pool: String,
    cli: Cli,
    model: Option<String>,
    effort: Option<String>,
}

/// What a run learned on its way, for its event. The report, stream and findings ledger are
/// named once the reviewer starts, as before then they hold nothing of this run.
#[derive(Default)]
struct Run {
    repo: String,
    head: String,
    spec: String,
    branch: String,
    files: String,
    tier: Option<Tier>,
    tier_source: &'static str,
    bundle_bytes: Option<u64>,
    tokens: Option<u64>,
    chosen: Option<Chosen>,
    /// The pools that ran out of quota on the way, with their reset.
    cooled: Vec<(String, Timestamp)>,
    /// The pools passed over for this run, with why.
    skipped: Vec<(String, String)>,
    /// What the run reports besides its result: a backend whose sandbox is not active.
    notes: Vec<String>,
    report: String,
    stream: String,
    findings: String,
    /// On a fix round, the head the previous round reviewed.
    from: String,
    reading: Option<Reading>,
}

fn review(
    args: &Args,
    caller: Option<Tier>,
    paths: &Paths,
    setup: &ReviewSetup,
    run: &mut Run,
) -> Result<Done, Failure> {
    bounded(args.timeout, &setup.backends)?;
    let cwd = env::current_dir().map_err(|e| {
        Failure::precondition(format!(
            "cannot read the working directory: {e}; cd into the repo under review"
        ))
    })?;
    let repo = GitRepo::at(&cwd);
    run.repo = repo.name().map_err(|e| {
        Failure::precondition(format!(
            "{e}; run factory review inside the repo under review"
        ))
    })?;
    run.branch = repo.branch().unwrap_or_default();
    let files = Files::new(paths, &run.repo, &args.label)?;
    let inputs = Inputs::gather(&repo, args, &files)?;
    let (tier, source) = tier::tier(&args.label, caller, &inputs.whole);
    run.tier = Some(tier);
    run.tier_source = source.as_str();
    run.files = inputs
        .whole
        .iter()
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>()
        .join(",");
    run.head = inputs.head.clone();
    run.notes.extend(inputs.notes.iter().cloned());
    run.from = inputs
        .fix
        .as_ref()
        .map_or_else(String::new, |fix| fix.head.clone());
    let spec = inputs.spec.as_ref().map_or("none", |(name, _)| name);
    run.spec = spec.to_owned();
    let bundle = inputs.bundle(args, &run.repo, tier).render();
    write(&files.bundle, &bundle)?;
    run.bundle_bytes = u64::try_from(bundle.len()).ok();
    let skill = skill(args, setup)?;
    let chain = setup.chain(tier);
    // The steps tried and done with: one whose model ran out while its pool's next model
    // is next in the chain.
    let mut spent = vec![false; chain.len()];
    // The daemon holds the backend's slot for as long as this session lives.
    let mut daemon = client::Session::open(paths)?;
    let text = loop {
        let pools = candidates(&setup.backends, chain, &spent, run);
        if pools.is_empty() && !run.skipped.is_empty() {
            let why: Vec<String> = run
                .skipped
                .iter()
                .map(|(name, why)| format!("{name} {why}"))
                .collect();
            return Err(Failure::precondition(format!(
                "no review backend can take the {tier} review: {}; fix that or configure another backend",
                why.join(", ")
            )));
        }
        let last = pools.len().saturating_sub(1);
        let slots = AcquireParams {
            review: Some(ReviewRef {
                repo: run.repo.clone(),
                label: args.label.clone(),
            }),
            backends: pools
                .iter()
                .enumerate()
                .map(|(at, &index)| {
                    let backend = &setup.backends[index];
                    // A busy pool passes the review on, but the chain's last waits for a slot.
                    let chained = if at == last {
                        WhenBusy::Queue
                    } else {
                        WhenBusy::Next
                    };
                    BackendSlots {
                        name: backend.name.clone(),
                        concurrency: backend.concurrency,
                        when_busy: backend.when_busy.unwrap_or(chained).into(),
                    }
                })
                .collect(),
        };
        let granted: AcquireResult = daemon.wait(REVIEW_ACQUIRE, &slots)?;
        let index = setup
            .backends
            .iter()
            .position(|backend| backend.name == granted.backend)
            .ok_or_else(|| {
                Failure::precondition(format!(
                    "the daemon granted an unknown backend {}; restart the daemon",
                    granted.backend
                ))
            })?;
        let backend = &setup.backends[index];
        // Granted again only once its reset has come and no other backend is left; a backend
        // that runs out twice in one review is not tried a third time.
        if run
            .cooled
            .iter()
            .filter(|(name, _)| *name == backend.name)
            .count()
            > 1
        {
            return Err(Failure::precondition(format!(
                "{} ran out of quota twice in this review; retry after its reset",
                backend.name
            )));
        }
        // The pool's first step not yet spent; once its reset has come, its first.
        let at = (0..chain.len())
            .find(|&at| chain[at].backend == index && !spent[at])
            .or_else(|| (0..chain.len()).find(|&at| chain[at].backend == index))
            .unwrap_or_default();
        let step = chain.get(at).cloned().unwrap_or(Step {
            backend: index,
            model: None,
            effort: None,
        });
        let reviewer = reviewer(backend, &step, tier, paths);
        run.chosen = Some(Chosen {
            pool: backend.name.clone(),
            cli: backend.cli,
            model: reviewer.model().map(str::to_owned),
            effort: reviewer.effort().map(str::to_owned),
        });
        match reviewer.preflight() {
            Ok(()) => {}
            Err(ReviewerError::OutOfQuota(reset)) => {
                cool(&mut daemon, run, &backend.name, reset)?;
                continue;
            }
            Err(ReviewerError::NoSandbox(why)) => {
                run.notes.push(format!("{} skipped: {why}", backend.name));
                run.skipped.push((backend.name.clone(), why));
                continue;
            }
            Err(ReviewerError::Unready(why)) => {
                run.skipped.push((backend.name.clone(), why));
                continue;
            }
            // Said, so a pool whose usage can't be read is never passed over unseen: on this
            // review's stderr, and in factory status until a review learns its quota.
            Err(ReviewerError::NoUsage(why)) => {
                run.notes.push(format!("{} skipped: {why}", backend.name));
                let unknown = QuotaParams {
                    backend: backend.name.clone(),
                    quota: QuotaParam::Unknown {
                        reason: why.clone(),
                    },
                };
                // The review goes on whether or not this lands.
                let _: Result<QuotaParams, _> = daemon.call(REVIEW_QUOTA, &unknown);
                run.skipped.push((backend.name.clone(), why));
                continue;
            }
            Err(e) => return Err(reviewer_failed(reviewer.as_ref(), &e, &files.prompt)),
        }
        let fix_round = inputs.fix.as_ref().is_some_and(|fix| fix.open.is_some());
        let prompt = review::prompt(
            skill.as_deref(),
            &reviewer.opening(
                inputs
                    .fix
                    .as_ref()
                    .map_or(args.base.as_str(), |fix| short(&fix.head)),
            ),
            &files.bundle.display().to_string(),
            spec,
            inputs.previous.as_ref().map(|(name, _)| name.as_str()),
            fix_round,
        );
        write(&files.prompt, &prompt)?;
        let stream = files.stream(reviewer.stream_extension());
        let job = Job {
            cwd: cwd.clone(),
            head: inputs.head.clone(),
            prompt: files.prompt.clone(),
            report: files.report.clone(),
            stream: stream.clone(),
            timeout: args.timeout.unwrap_or(backend.timeout),
        };
        files.clear_outputs(&stream)?;
        run.report = files.report.display().to_string();
        run.stream = stream.display().to_string();
        let outcome = reviewer.review(&job);
        run.tokens = fs::read_to_string(&stream)
            .ok()
            .and_then(|record| review::tokens(reviewer.name(), &record));
        match outcome {
            Ok(()) => {}
            Err(ReviewerError::OutOfQuota(reset)) => {
                spent[at] = true;
                // agy's quota can be one model's: the chain's next model in this pool gets its
                // try before the pool cools.
                if chain.get(at + 1).is_some_and(|next| next.backend == index) {
                    continue;
                }
                cool(&mut daemon, run, &backend.name, reset)?;
                continue;
            }
            // Its review ran without the sandbox, so it is not kept: the next backend reviews.
            Err(ReviewerError::NoSandbox(why)) => {
                run.notes.push(format!("{} skipped: {why}", backend.name));
                run.skipped.push((backend.name.clone(), why));
                continue;
            }
            Err(e) => return Err(reviewer_failed(reviewer.as_ref(), &e, &stream)),
        }
        let text = fs::read_to_string(&files.report).unwrap_or_default();
        if text.trim().is_empty() {
            return Err(Failure::precondition(format!(
                "{} finished without a report ({}); see {}",
                reviewer.name(),
                files.report.display(),
                stream.display()
            )));
        }
        let available = QuotaParams {
            backend: backend.name.clone(),
            quota: QuotaParam::Available,
        };
        // The review is done whether or not this lands; the next run that cools it down
        // records its quota again. The daemon keeps a cooldown another review hit meanwhile.
        let _: Result<QuotaParams, _> = daemon.call(REVIEW_QUOTA, &available);
        break text;
    };
    let Some(chosen) = run.chosen.as_ref() else {
        return Err(Failure::precondition(
            "the review ended with no backend named; retry it".to_owned(),
        ));
    };
    let pool = chosen.pool.clone();
    let reading = Reading::of(&text).map_err(|e| {
        Failure::precondition(format!("{e}: {}; read it there", files.report.display()))
    })?;
    // A fix round is held to its rules: old findings marked, new ones only on the fix's lines.
    let reading = match inputs.fix.as_ref().and_then(|fix| fix.open.as_deref()) {
        Some(open) => reading.within(open, &Fix::of_diff(&inputs.diff)),
        None => reading,
    };
    let round = review::round(&args.label, args.previous.is_some());
    // review.sh's ledger of the run, which a fix round reads its head from.
    let run_ledger = json!({
        "label": args.label,
        "round": round,
        "base": args.base,
        "from": run.from,
        "head": inputs.head,
        "tier": tier.as_str(),
        "pool": pool,
        "model": chosen.model.clone().unwrap_or_default(),
        "effort": chosen.effort.clone().unwrap_or_default(),
        "verdict": reading.verdict.to_string(),
        "report": run.report,
        "findings": reading.tallies.join(" "),
    });
    write(&files.ledger, &format!("{run_ledger:#}\n"))?;
    let mut ledger = json!({
        "label": args.label,
        "round": round,
        "base": args.base,
        "head": inputs.head,
        "backend": pool,
        "verdict": reading.verdict.to_string(),
        "blocking": reading.counts.blocking,
        "advisory": reading.counts.advisory,
        "question": reading.counts.question,
        "findings": reading
            .findings
            .as_ref()
            .map(|findings| findings.iter().map(Finding::to_value).collect::<Vec<_>>()),
    });
    if !run.from.is_empty() {
        ledger["from"] = run.from.clone().into();
        ledger["fixed"] = reading.fixed.into();
        ledger["still_present"] = reading.still_present.into();
        ledger["dropped"] = reading.dropped.len().into();
        ledger["dropped_findings"] = reading.dropped.iter().map(Finding::to_value).collect();
    }
    write(&files.findings, &format!("{ledger:#}\n"))?;
    run.findings = files.findings.display().to_string();
    let cooled: Vec<Value> = run
        .cooled
        .iter()
        .map(|(name, until)| json!({"backend": name, "until": until.to_string()}))
        .collect();
    let skipped: Vec<Value> = run
        .skipped
        .iter()
        .map(|(name, why)| json!({"backend": name, "reason": why}))
        .collect();
    let mut done = Done::new(
        reading.summary(&args.label, &run.report),
        json!({
            "label": args.label,
            "backend": pool,
            "reviewer": config::cli_name(chosen.cli),
            "model": chosen.model,
            "effort": chosen.effort,
            "tier": tier.as_str(),
            "tier_source": run.tier_source,
            "cooled": cooled,
            "skipped": skipped,
            "verdict": reading.verdict.to_string(),
            "blocking": reading.counts.blocking,
            "advisory": reading.counts.advisory,
            "question": reading.counts.question,
            "fixed": reading.fixed,
            "still_present": reading.still_present,
            "dropped": reading.dropped.len(),
            "report": run.report,
            "findings": run.findings,
            "blocking_findings": reading.blocking_lines(),
        }),
    );
    // Whole: the line names the report, and a shim reads that path.
    done.whole = true;
    done.more = reading.blocking_lines();
    done.notes = std::mem::take(&mut run.notes);
    if reading.verdict == Verdict::Findings {
        done.exit = Exit::NeedsAttention;
        if reading.findings.is_none() {
            done.more = vec![format!(
                "the report has no findings ledger: read its blocking findings in {}",
                run.report
            )];
        }
    }
    run.reading = Some(reading);
    Ok(done)
}

/// The reviewer skill the prompt carries: the caller's, else the config's, else the default
/// when it is there. A skill the caller or the config names must be readable.
fn skill(args: &Args, setup: &ReviewSetup) -> Result<Option<String>, Failure> {
    if let Some(named) = args.skill.as_ref().or(setup.skill.as_ref()) {
        return read_given("reviewer skill", named).map(Some);
    }
    let default = env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_SKILL));
    Ok(default.and_then(|path| fs::read_to_string(path).ok()))
}

/// The pools a run may still be granted, in the chain's order: each with a step not yet
/// spent, none it skipped, and none it cooled down while another is left. The daemon decides
/// whether a cooled one's reset has come.
fn candidates(backends: &[Backend], chain: &[Step], spent: &[bool], run: &Run) -> Vec<usize> {
    let skipped = |index: usize| {
        run.skipped
            .iter()
            .any(|(name, _)| *name == backends[index].name)
    };
    let cooled = |index: usize| {
        run.cooled
            .iter()
            .any(|(name, _)| *name == backends[index].name)
    };
    let mut pools: Vec<usize> = Vec::new();
    for (at, step) in chain.iter().enumerate() {
        let open = !spent[at] || cooled(step.backend);
        if open && !skipped(step.backend) && !pools.contains(&step.backend) {
            pools.push(step.backend);
        }
    }
    if pools.iter().all(|&index| cooled(index)) {
        return pools;
    }
    pools.into_iter().filter(|&index| !cooled(index)).collect()
}

/// Refuses a timeout too long to wait on: the wait would have no deadline.
fn bounded(timeout: Option<Duration>, backends: &[Backend]) -> Result<(), Failure> {
    let limit = Duration::from_secs(MAX_TIMEOUT_S);
    if let Some(timeout) = timeout.filter(|timeout| *timeout > limit) {
        return Err(Failure::usage(format!(
            "--timeout {} is longer than a week; give at most {MAX_TIMEOUT_S}",
            timeout.as_secs()
        )));
    }
    match backends.iter().find(|backend| backend.timeout > limit) {
        Some(backend) if timeout.is_none() => Err(Failure::precondition(format!(
            "backend {} has a timeout of {}s, longer than a week; set at most {MAX_TIMEOUT_S} in the factory config",
            backend.name,
            backend.timeout.as_secs()
        ))),
        _ => Ok(()),
    }
}

/// Cools `backend` down until its `reset`, in the daemon and in the run's record.
fn cool(
    daemon: &mut client::Session,
    run: &mut Run,
    backend: &str,
    reset: Reset,
) -> Result<(), Failure> {
    let now = clock::now();
    let until = reset.until(now, clock::utc_offset(now));
    let cooling = QuotaParams {
        backend: backend.to_owned(),
        quota: QuotaParam::Cooling {
            until: until.unix_millis(),
        },
    };
    let _: QuotaParams = daemon.call(REVIEW_QUOTA, &cooling)?;
    run.cooled.push((backend.to_owned(), until));
    Ok(())
}

/// What the bundle holds, read from the repo and the files the caller named. A text the
/// bundle names comes with its name.
struct Inputs {
    base_commit: String,
    head: String,
    commits: String,
    diff_stat: String,
    diff: String,
    /// The whole diff's files, from the base, whatever the round: what the tier is read from.
    whole: Vec<tier::Change>,
    /// On a fix round, what the previous round's ledger gave: the head it reviewed, where the
    /// diff starts, and its open findings.
    fix: Option<Previous>,
    /// Why this fix round reviews the whole diff after all.
    notes: Vec<String>,
    spec: Option<(String, String)>,
    previous: Option<(String, String)>,
    /// The Markdown files this round's diff touches: each one's path and its text, none when
    /// it is too long to carry.
    docs: Vec<(String, Option<String>)>,
    standards: Option<(String, String)>,
    evidence: Option<(String, String)>,
}

impl Inputs {
    /// Gathers the inputs, refusing an unknown base or an empty diff before anything runs.
    fn gather(repo: &impl Repo, args: &Args, files: &Files) -> Result<Inputs, Failure> {
        let base = &args.base;
        let base_commit = repo.commit(base).map_err(git_failed)?.ok_or_else(|| {
            Failure::usage(format!("unknown ref: {base}; name a branch, tag or commit"))
        })?;
        let mut notes = Vec::new();
        let previous = args
            .previous
            .as_deref()
            .map(|path| {
                Ok((
                    path.display().to_string(),
                    read_given("previous report", path)?,
                ))
            })
            .transpose()?;
        let fix = match &args.previous {
            Some(previous) => fix_start(repo, args, previous, files, &mut notes)?,
            None => None,
        };
        let since = fix.as_ref().map_or(base.as_str(), |fix| fix.head.as_str());
        let diff = repo.diff(since, DIFF_CONTEXT).map_err(git_failed)?;
        let touched = tier::numstat(&repo.numstat(since).map_err(git_failed)?);
        if diff.is_empty() && touched.is_empty() {
            return Err(Failure::usage(match &fix {
                Some(fix) => format!(
                    "no committed changes since the previous review's head {} (commit the fix first)",
                    short(&fix.head)
                ),
                None => format!("no committed changes since {base} (commit first)"),
            }));
        }
        let whole = match fix {
            Some(_) => tier::numstat(&repo.numstat(base).map_err(git_failed)?),
            None => touched.clone(),
        };
        let spec = match args.spec.as_deref().filter(|spec| *spec != "none") {
            Some(spec) => fetch_spec(spec, &files.spec, &mut notes)?,
            None => None,
        };
        let top = repo.toplevel().map_err(git_failed)?;
        let docs = touched
            .iter()
            .filter(|change| change.path.ends_with(".md"))
            .filter_map(|change| {
                let path = top.join(&change.path);
                let meta = fs::metadata(&path).ok().filter(|meta| meta.is_file())?;
                let text = (meta.len() < DOC_BYTES)
                    .then(|| fs::read_to_string(&path).ok())
                    .flatten();
                Some((change.path.clone(), text))
            })
            .collect();
        let standards = STANDARDS.iter().find_map(|name| {
            let path = top.join(name);
            let text = fs::read_to_string(&path)
                .ok()
                .filter(|text| !text.is_empty())?;
            Some((path.display().to_string(), text))
        });
        let evidence = repo.git_dir().map_err(git_failed)?.join(EVIDENCE);
        let evidence = fs::read_to_string(&evidence)
            .ok()
            .filter(|text| !text.trim().is_empty())
            .map(|text| (evidence.display().to_string(), text));
        if let Some((_, text)) = &evidence {
            write(&files.evidence, text)?;
        }
        Ok(Inputs {
            base_commit,
            head: repo.commit("HEAD").map_err(git_failed)?.unwrap_or_default(),
            commits: repo.commits(since).map_err(git_failed)?,
            diff_stat: repo.diff_stat(since).map_err(git_failed)?,
            diff,
            whole,
            fix,
            notes,
            spec,
            previous,
            docs,
            standards,
            evidence,
        })
    }

    fn bundle<'a>(&'a self, args: &'a Args, repo: &'a str, tier: Tier) -> Bundle<'a> {
        let part = |(name, text): &'a (String, String)| Part { name, text };
        Bundle {
            label: &args.label,
            repo,
            round: review::round(&args.label, args.previous.is_some()),
            tier: tier.as_str(),
            base: &args.base,
            base_commit: &self.base_commit,
            head: &self.head,
            fix_from: self.fix.as_ref().map(|fix| fix.head.as_str()),
            open: self.fix.as_ref().and_then(|fix| fix.open.as_deref()),
            commits: &self.commits,
            diff_stat: &self.diff_stat,
            diff: &self.diff,
            spec: self.spec.as_ref().map(part),
            previous: self.previous.as_ref().map(part),
            docs: self
                .docs
                .iter()
                .map(|(name, text)| match text {
                    Some(text) => Doc::Whole(Part { name, text }),
                    None => Doc::TooLong(name),
                })
                .collect(),
            standards: self.standards.as_ref().map(part),
            evidence: self.evidence.as_ref().map(part),
        }
    }
}

/// Where a fix round's diff starts: the head the previous round reviewed, when that is on
/// this branch. Its findings ledger (`.findings.json`) names it with the open findings;
/// review.sh's ledger (`.ledger.json`) or the last ok `review` event of that report name
/// it alone. Otherwise the round reviews the whole diff, and says why in `notes`.
fn fix_start(
    repo: &impl Repo,
    args: &Args,
    previous: &Path,
    files: &Files,
    notes: &mut Vec<String>,
) -> Result<Option<Previous>, Failure> {
    let read = |extension: &str| {
        fs::read_to_string(previous.with_extension(extension))
            .ok()
            .and_then(|text| Previous::of_ledger(&text))
    };
    let logged = || {
        let report = previous.display().to_string();
        let ledger = fs::read_to_string(&files.events).ok()?;
        ledger
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["type"] == "review"
                    && event["report"] == report.as_str()
                    && event["outcome"] == "ok"
            })
            // The ledger logs an all-digit short sha as a number.
            .filter_map(|event| match &event["head"] {
                Value::String(head) => Some(head.clone()),
                Value::Number(head) => Some(head.to_string()),
                _ => None,
            })
            .rfind(|head| !head.is_empty())
            .map(|head| Previous { head, open: None })
    };
    let named = read("findings.json")
        .or_else(|| read("ledger.json"))
        .or_else(logged);
    let on_branch = match named {
        Some(named) => match repo.commit(&named.head).map_err(git_failed)? {
            Some(head) if repo.is_ancestor(&head).map_err(git_failed)? => {
                Some(Previous { head, ..named })
            }
            _ => None,
        },
        None => None,
    };
    if on_branch.is_none() {
        notes.push(format!(
            "no reviewed head for {} on this branch; round {} reviews the whole diff",
            previous.display(),
            review::round(&args.label, true)
        ));
    }
    Ok(on_branch)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// The files of one review, in `<state>/reviews/<repo>/`.
struct Files {
    bundle: PathBuf,
    /// An issue ref's spec, as fetched.
    spec: PathBuf,
    prompt: PathBuf,
    report: PathBuf,
    /// The findings ledger read from the report.
    findings: PathBuf,
    /// review.sh's ledger of the run: label, heads, tier, pool, verdict and FINDINGS words.
    ledger: PathBuf,
    /// The builder's evidence, as the bundle carried it.
    evidence: PathBuf,
    /// The event ledger, where a fix round may find the head its previous round reviewed.
    events: PathBuf,
    dir: PathBuf,
    label: String,
}

impl Files {
    fn new(paths: &Paths, repo: &str, label: &str) -> Result<Files, Failure> {
        let dir = paths.state.join("reviews").join(repo);
        fs::create_dir_all(&dir).map_err(|e| {
            Failure::precondition(format!(
                "cannot create {}: {e}; check FACTORY_STATE",
                dir.display()
            ))
        })?;
        let file = |ext: &str| dir.join(format!("{label}.{ext}"));
        Ok(Files {
            bundle: file("bundle.md"),
            spec: file("spec.md"),
            prompt: file("prompt"),
            report: file("md"),
            findings: file("findings.json"),
            ledger: file("ledger.json"),
            evidence: file("evidence.md"),
            events: paths.state.join("events.jsonl"),
            dir: dir.clone(),
            label: label.to_owned(),
        })
    }

    /// A backend's event stream, by the extension it names: Codex's is `.jsonl`, which
    /// factory-stats reads.
    fn stream(&self, extension: &str) -> PathBuf {
        self.dir.join(format!("{}.{extension}", self.label))
    }

    /// Removes what an earlier run under the same label left in the report, the findings
    /// ledger and this backend's `stream`, so nothing stale is read.
    fn clear_outputs(&self, stream: &Path) -> Result<(), Failure> {
        for path in [&self.report, stream, &self.findings, &self.ledger] {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(Failure::precondition(format!(
                        "cannot remove the stale {}: {e}; remove it by hand",
                        path.display()
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The spec's name for the prompt and its text. An issue ref is fetched with gh and saved to
/// `saved`, since the reviewer's sandbox has no network; anything else names a file. A path
/// with no file there, or an empty one, is no spec, as review.sh had it: the review goes on
/// and says so in `notes`.
fn fetch_spec(
    spec: &str,
    saved: &Path,
    notes: &mut Vec<String>,
) -> Result<Option<(String, String)>, Failure> {
    let Some(issue) = review::issue_ref(spec) else {
        let path = Path::new(spec);
        if !path.is_file() || fs::metadata(path).is_ok_and(|meta| meta.len() == 0) {
            notes.push(format!(
                "spec {spec} is no file with text; the review has no spec"
            ));
            return Ok(None);
        }
        return Ok(Some((spec.to_owned(), read_given("spec", path)?)));
    };
    let text = Gh.issue(&issue).map_err(|e| {
        Failure::usage(format!(
            "could not fetch spec {spec} with gh: {e}; check gh auth status, or pass the spec as a file"
        ))
    })?;
    write(saved, &text)?;
    Ok(Some((saved.display().to_string(), text)))
}

/// Reads a file the caller named, refusing with exit 2 when it can't be read.
fn read_given(what: &str, path: &Path) -> Result<String, Failure> {
    fs::read_to_string(path).map_err(|e| {
        Failure::usage(format!(
            "cannot read the {what} {}: {e}; pass a readable file",
            path.display()
        ))
    })
}

fn write(path: &Path, text: &str) -> Result<(), Failure> {
    fs::write(path, text).map_err(|e: io::Error| {
        Failure::precondition(format!(
            "cannot write {}: {e}; check the disk and FACTORY_STATE",
            path.display()
        ))
    })
}

/// The failure a backend's error is: a timeout exits 124, anything else 3. The line starts
/// `<backend> <what happened>` (`codex exited 1`), which factory-stats' classifier reads.
fn reviewer_failed(reviewer: &dyn Reviewer, e: &ReviewerError, stream: &Path) -> Failure {
    let exit = match e {
        ReviewerError::TimedOut(_) => Exit::Timeout,
        _ => Exit::Precondition,
    };
    let next = match e {
        ReviewerError::Start(_) => {
            format!("check that {} is installed and on PATH", reviewer.name())
        }
        ReviewerError::Checkout(_) => "check the repository and its worktrees".to_owned(),
        ReviewerError::TimedOut(_) => format!(
            "its process group was killed; see {}, then retry or raise --timeout",
            stream.display()
        ),
        _ => format!("see {}", stream.display()),
    };
    Failure::new(exit, format!("{} {e}; {next}", reviewer.name()))
}

fn git_failed(e: GitError) -> Failure {
    Failure::precondition(format!("{e}; check the repository"))
}
