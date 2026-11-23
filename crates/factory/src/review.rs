//! `factory review <base> <label> [spec] [prev]`: review.sh's arguments, a prepared review
//! bundle, a run on the first configured backend the daemon's router grants, and one line back.
//! A backend that runs out of quota partway is cooled down until its reset, and the same review
//! re-runs on the next one. A backend's preflight runs before the review is prepared for it: one
//! out of quota is cooled down the same way, and one that can't review now (agy without a usage
//! answer or an active sandbox) is skipped for this run.
//!
//! Every file of a review is kept in `<state>/reviews/<repo>/`, named by its label: the
//! bundle (`.bundle.md`), the prompt (`.prompt`), the report (`.md`) and Codex's event
//! stream (`.jsonl`, which factory-stats reads), the spec an issue ref names (`.spec.md`)
//! and the findings ledger read from the report (`.findings.json`, not `.jsonl`, which
//! factory-stats would read as a stream). Each run lands one `review` event in the ledger.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use factory_adapters::gh::{Gh, Issues};
use factory_adapters::git::{GitError, GitRepo, Repo};
use factory_adapters::reviewer::{Agy, Claude, Codex, Job, Reviewer, ReviewerError, home_of};
use factory_core::exit::Exit;
use factory_core::quota::Reset;
use factory_core::review::{self, Bundle, Finding, Part, Reading, Record, Verdict, reviewed_head};
use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};

use crate::client;
use crate::clock;
use crate::config::{self, Backend, Cli};
use crate::log;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{
    AcquireParams, AcquireResult, BackendSlots, QuotaParam, QuotaParams, REVIEW_ACQUIRE,
    REVIEW_QUOTA,
};

/// Lines of context around each change in the bundle's diff, so the reviewer rarely opens a
/// file to see what surrounds a hunk.
const DIFF_CONTEXT: u32 = 15;
/// The files a repo documents its standards in, at its top level.
const STANDARDS: [&str; 2] = ["CODING_STANDARDS.md", "CONTRIBUTING.md"];
/// Where the implementer leaves its evidence, in the git dir so it is never committed.
const EVIDENCE: &str = "factory-evidence.md";
/// The longest a review may run, in seconds: a week.
const MAX_TIMEOUT_S: u64 = 7 * 24 * 60 * 60;

pub struct Args {
    pub base: String,
    pub label: String,
    pub spec: Option<String>,
    pub previous: Option<PathBuf>,
    /// Overrides every backend's configured timeout.
    pub timeout: Option<Duration>,
}

pub fn run(args: &Args) -> Result<Done, Failure> {
    review::check_label(&args.label).map_err(|e| Failure::usage(e.to_string()))?;
    let paths = Paths::from_env()?;
    let started = Instant::now();
    let routes = config::review_backends(&paths).map(|backends| {
        backends
            .into_iter()
            .map(|backend| Route {
                reviewer: reviewer(&backend, &paths),
                backend,
            })
            .collect::<Vec<_>>()
    });
    let mut run = Run::default();
    let result = match &routes {
        Ok(routes) => review(args, &paths, routes, &mut run),
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
    let chosen = routes
        .as_ref()
        .ok()
        .and_then(|routes| routes.get(run.backend));
    let reviewer = chosen.map(|route| &route.reviewer);
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
        backend: chosen.map_or("", |route| route.backend.name.as_str()),
        model: reviewer.and_then(|reviewer| reviewer.model()),
        effort: reviewer.and_then(|reviewer| reviewer.effort()),
        duration_s: started.elapsed().as_secs(),
        fallback_from: &fallback_from,
        skipped: &skipped,
        report: &run.report,
        stream: &run.stream,
        findings: &run.findings,
        result: match (&run.reading, &failed) {
            (Some(reading), None) => Ok(reading),
            _ => Err(failed
                .as_deref()
                .unwrap_or("the run ended without a reading")),
        },
    };
    let logged = log::land(&paths, "review", &record.pairs());
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

/// A configured backend and the adapter that runs it.
struct Route {
    backend: Backend,
    reviewer: Box<dyn Reviewer>,
}

/// The adapter that runs a configured backend.
fn reviewer(backend: &Backend, paths: &Paths) -> Box<dyn Reviewer> {
    let (model, effort) = (backend.model.clone(), backend.effort.clone());
    match backend.cli {
        Cli::Agy => Box::new(Agy::new(
            model,
            effort,
            backend.auth,
            paths.state.join("agy-usage.json"),
        )),
        Cli::Codex => Box::new(Codex::new(model, effort, &home_of("CODEX_HOME", ".codex"))),
        Cli::Claude => Box::new(Claude::new(
            model,
            effort,
            &home_of("CLAUDE_CONFIG_DIR", ".claude"),
        )),
    }
}

/// What a run learned on its way, for its event. The report, stream and findings ledger are
/// named once the reviewer starts, as before then they hold nothing of this run.
#[derive(Default)]
struct Run {
    repo: String,
    head: String,
    spec: String,
    /// The configured backend that runs (or last ran) the review: the first until one is picked.
    backend: usize,
    /// The backends that ran out of quota on the way, with their reset.
    cooled: Vec<(String, Timestamp)>,
    /// The backends passed over for this run, with why.
    skipped: Vec<(String, String)>,
    /// What the run reports besides its result: a backend whose sandbox is not active.
    notes: Vec<String>,
    report: String,
    stream: String,
    findings: String,
    reading: Option<Reading>,
}

fn review(args: &Args, paths: &Paths, routes: &[Route], run: &mut Run) -> Result<Done, Failure> {
    bounded(args.timeout, routes)?;
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
    let files = Files::new(paths, &run.repo, &args.label)?;
    let inputs = Inputs::gather(&repo, args, &files)?;
    run.head = inputs.head.clone();
    run.notes.extend(inputs.notes.iter().cloned());
    let spec = inputs.spec.as_ref().map_or("none", |(name, _)| name);
    run.spec = spec.to_owned();
    write(&files.bundle, &inputs.bundle(args).render())?;
    // The daemon holds the backend's slot for as long as this session lives.
    let mut daemon = client::Session::open(paths)?;
    let text = loop {
        let slots = AcquireParams {
            backends: candidates(routes, run)
                .into_iter()
                .map(|Route { backend, .. }| BackendSlots {
                    name: backend.name.clone(),
                    concurrency: backend.concurrency,
                    when_busy: backend.when_busy.into(),
                })
                .collect(),
        };
        if slots.backends.is_empty() && !run.skipped.is_empty() {
            let why: Vec<String> = run
                .skipped
                .iter()
                .map(|(name, why)| format!("{name} {why}"))
                .collect();
            return Err(Failure::precondition(format!(
                "no review backend can take the review: {}; fix that or configure another backend",
                why.join(", ")
            )));
        }
        let granted: AcquireResult = daemon.wait(REVIEW_ACQUIRE, &slots)?;
        let index = routes
            .iter()
            .position(|route| route.backend.name == granted.backend)
            .ok_or_else(|| {
                Failure::precondition(format!(
                    "the daemon granted an unknown backend {}; restart the daemon",
                    granted.backend
                ))
            })?;
        let Route { backend, reviewer } = &routes[index];
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
        run.backend = index;
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
            Err(e) => return Err(reviewer_failed(reviewer.as_ref(), &e, &files.prompt)),
        }
        let prompt = review::prompt(
            &reviewer.opening(&args.base),
            &files.bundle.display().to_string(),
            spec,
            inputs.previous.as_ref().map(|(name, _)| name.as_str()),
        );
        write(&files.prompt, &prompt)?;
        let stream = files.stream(reviewer.stream_extension());
        let job = Job {
            cwd: cwd.clone(),
            prompt: files.prompt.clone(),
            report: files.report.clone(),
            stream: stream.clone(),
            timeout: args.timeout.unwrap_or(backend.timeout),
        };
        files.clear_outputs(&stream)?;
        run.report = files.report.display().to_string();
        run.stream = stream.display().to_string();
        match reviewer.review(&job) {
            Ok(()) => {}
            Err(ReviewerError::OutOfQuota(reset)) => {
                cool(&mut daemon, run, &backend.name, reset)?;
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
    let backend = &routes[run.backend].backend;
    let reading = Reading::of(&text).map_err(|e| {
        Failure::precondition(format!("{e}: {}; read it there", files.report.display()))
    })?;
    let ledger = json!({
        "label": args.label,
        "round": review::round(&args.label, args.previous.is_some()),
        "base": args.base,
        "head": inputs.head,
        "backend": backend.name,
        "verdict": reading.verdict.to_string(),
        "blocking": reading.counts.blocking,
        "advisory": reading.counts.advisory,
        "question": reading.counts.question,
        "findings": reading
            .findings
            .as_ref()
            .map(|findings| findings.iter().map(Finding::to_value).collect::<Vec<_>>()),
    });
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
            "backend": backend.name,
            "cooled": cooled,
            "skipped": skipped,
            "verdict": reading.verdict.to_string(),
            "blocking": reading.counts.blocking,
            "advisory": reading.counts.advisory,
            "question": reading.counts.question,
            "report": run.report,
            "findings": run.findings,
            "blocking_findings": reading.blocking_lines(),
        }),
    );
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

/// Cools `backend` down until its `reset`, in the daemon and in the run's record.
/// The routes a run may still be granted: none it skipped, and none it cooled down while
/// another is left. The daemon decides whether a cooled one's reset has come.
fn candidates<'a>(routes: &'a [Route], run: &Run) -> Vec<&'a Route> {
    let skipped = |route: &Route| {
        run.skipped
            .iter()
            .any(|(name, _)| *name == route.backend.name)
    };
    let cooled = |route: &Route| {
        run.cooled
            .iter()
            .any(|(name, _)| *name == route.backend.name)
    };
    let open: Vec<&Route> = routes.iter().filter(|route| !skipped(route)).collect();
    if open.iter().all(|route| cooled(route)) {
        return open;
    }
    open.into_iter().filter(|route| !cooled(route)).collect()
}

/// Refuses a timeout too long to wait on: the wait would have no deadline.
fn bounded(timeout: Option<Duration>, routes: &[Route]) -> Result<(), Failure> {
    let limit = Duration::from_secs(MAX_TIMEOUT_S);
    if let Some(timeout) = timeout.filter(|timeout| *timeout > limit) {
        return Err(Failure::usage(format!(
            "--timeout {} is longer than a week; give at most {MAX_TIMEOUT_S}",
            timeout.as_secs()
        )));
    }
    match routes.iter().find(|route| route.backend.timeout > limit) {
        Some(route) if timeout.is_none() => Err(Failure::precondition(format!(
            "backend {} has a timeout of {}s, longer than a week; set at most {MAX_TIMEOUT_S} in the factory config",
            route.backend.name,
            route.backend.timeout.as_secs()
        ))),
        _ => Ok(()),
    }
}

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
    /// On a fix round, the head the previous round reviewed, where the diff starts.
    fix_from: Option<String>,
    /// Why this fix round reviews the whole diff after all.
    notes: Vec<String>,
    spec: Option<(String, String)>,
    previous: Option<(String, String)>,
    standards: Vec<(String, String)>,
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
        let fix_from = match &args.previous {
            Some(previous) => fix_start(repo, args, previous, &mut notes)?,
            None => None,
        };
        let since = fix_from.as_deref().unwrap_or(base);
        let diff = repo.diff(since, DIFF_CONTEXT).map_err(git_failed)?;
        if diff.is_empty() {
            return Err(Failure::usage(match &fix_from {
                Some(from) => format!(
                    "no committed changes since the previous review's head {} (commit the fix first)",
                    short(from)
                ),
                None => format!("no committed changes since {base} (commit first)"),
            }));
        }
        let spec = args
            .spec
            .as_deref()
            .filter(|spec| *spec != "none")
            .map(|spec| fetch_spec(spec, &files.spec))
            .transpose()?;
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
        let top = repo.toplevel().map_err(git_failed)?;
        let standards = STANDARDS
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), fs::read_to_string(top.join(name)).ok()?)))
            .collect();
        let evidence = repo.git_dir().map_err(git_failed)?.join(EVIDENCE);
        let evidence = fs::read_to_string(&evidence)
            .ok()
            .filter(|text| !text.trim().is_empty())
            .map(|text| (evidence.display().to_string(), text));
        Ok(Inputs {
            base_commit,
            head: repo.commit("HEAD").map_err(git_failed)?.unwrap_or_default(),
            commits: repo.commits(since).map_err(git_failed)?,
            diff_stat: repo.diff_stat(since).map_err(git_failed)?,
            diff,
            fix_from,
            notes,
            spec,
            previous,
            standards,
            evidence,
        })
    }

    fn bundle<'a>(&'a self, args: &'a Args) -> Bundle<'a> {
        let part = |(name, text): &'a (String, String)| Part { name, text };
        Bundle {
            label: &args.label,
            round: review::round(&args.label, args.previous.is_some()),
            base: &args.base,
            base_commit: &self.base_commit,
            head: &self.head,
            fix_from: self.fix_from.as_deref(),
            commits: &self.commits,
            diff_stat: &self.diff_stat,
            diff: &self.diff,
            spec: self.spec.as_ref().map(part),
            previous: self.previous.as_ref().map(part),
            standards: self.standards.iter().map(part).collect(),
            evidence: self.evidence.as_ref().map(part),
        }
    }
}

/// Where a fix round's diff starts: the head the previous report's findings ledger says it
/// reviewed, when that is on this branch. Otherwise the round reviews the whole diff, and
/// says why in `notes`.
fn fix_start(
    repo: &impl Repo,
    args: &Args,
    previous: &Path,
    notes: &mut Vec<String>,
) -> Result<Option<String>, Failure> {
    let ledger = previous.with_extension("findings.json");
    let named = fs::read_to_string(&ledger)
        .ok()
        .and_then(|text| reviewed_head(&text));
    let on_branch = match named {
        Some(head) => match repo.commit(&head).map_err(git_failed)? {
            Some(commit) if repo.is_ancestor(&commit).map_err(git_failed)? => Some(commit),
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
        for path in [&self.report, stream, &self.findings] {
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
/// `saved`, since the reviewer's sandbox has no network; anything else names a file.
fn fetch_spec(spec: &str, saved: &Path) -> Result<(String, String), Failure> {
    let Some(issue) = review::issue_ref(spec) else {
        return Ok((spec.to_owned(), read_given("spec", Path::new(spec))?));
    };
    let text = Gh.issue(issue).map_err(|e| {
        Failure::usage(format!(
            "could not fetch spec {spec} with gh: {e}; check gh auth status, or pass the spec as a file"
        ))
    })?;
    write(saved, &text)?;
    Ok((saved.display().to_string(), text))
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
