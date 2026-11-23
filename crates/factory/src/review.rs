//! `factory review <base> <label> [spec] [prev]`: review.sh's arguments, a prepared review
//! bundle, one Codex run, and one line back.
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
use factory_adapters::reviewer::{Codex, Job, Reviewer, ReviewerError};
use factory_core::event::Event;
use factory_core::exit::Exit;
use factory_core::review::{self, Bundle, Finding, Part, Reading, Record, Verdict};
use serde_json::json;

use crate::client;
use crate::output::{Done, Failure};
use crate::paths::Paths;
use crate::protocol::{LOG, LogParams, LogResult};

/// Lines of context around each change in the bundle's diff, so the reviewer rarely opens a
/// file to see what surrounds a hunk.
const DIFF_CONTEXT: u32 = 15;
/// The files a repo documents its standards in, at its top level.
const STANDARDS: [&str; 2] = ["CODING_STANDARDS.md", "CONTRIBUTING.md"];
/// Where the implementer leaves its evidence, in the git dir so it is never committed.
const EVIDENCE: &str = "factory-evidence.md";

pub struct Args {
    pub base: String,
    pub label: String,
    pub spec: Option<String>,
    pub previous: Option<PathBuf>,
    pub timeout: Duration,
}

pub fn run(args: &Args) -> Result<Done, Failure> {
    review::check_label(&args.label).map_err(|e| Failure::usage(e.to_string()))?;
    let paths = Paths::from_env()?;
    let started = Instant::now();
    let mut run = Run::default();
    let result = review(args, &paths, &mut run);
    let failed = result.as_ref().err().map(|failure| failure.message.clone());
    let record = Record {
        repo: &run.repo,
        label: &args.label,
        round: review::round(&args.label, args.previous.is_some()),
        base: &args.base,
        head: &run.head,
        spec: &run.spec,
        backend: &run.backend,
        model: run.model.as_deref(),
        effort: run.effort.as_deref(),
        duration_s: started.elapsed().as_secs(),
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
    if let Err(e) = log_event(&paths, &record) {
        eprintln!("factory: the review event was not logged: {}", e.message);
    }
    result
}

/// What a run learned on its way, for its event.
#[derive(Default)]
struct Run {
    repo: String,
    head: String,
    spec: String,
    backend: String,
    model: Option<String>,
    effort: Option<String>,
    report: String,
    stream: String,
    findings: String,
    reading: Option<Reading>,
}

fn review(args: &Args, paths: &Paths, run: &mut Run) -> Result<Done, Failure> {
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
    run.report = files.report.display().to_string();
    run.stream = files.stream.display().to_string();
    run.findings = files.findings.display().to_string();
    let inputs = Inputs::gather(&repo, args, &files)?;
    run.head = inputs.head.clone();
    let spec = inputs.spec.as_ref().map_or("none", |(name, _)| name);
    run.spec = spec.to_owned();
    write(&files.bundle, &inputs.bundle(args).render())?;
    let prompt = review::prompt(
        &args.base,
        &files.bundle.display().to_string(),
        spec,
        inputs.previous.as_ref().map(|(name, _)| name.as_str()),
    );
    write(&files.prompt, &prompt)?;
    let codex = Codex::configured_in(&codex_home());
    run.backend = codex.name().to_owned();
    run.model = codex.model().map(str::to_owned);
    run.effort = codex.effort().map(str::to_owned);
    let job = Job {
        cwd,
        prompt: files.prompt.clone(),
        report: files.report.clone(),
        stream: files.stream.clone(),
        timeout: args.timeout,
    };
    files.clear_outputs()?;
    codex
        .review(&job)
        .map_err(|e| reviewer_failed(&codex, &e, &files.stream))?;
    let text = fs::read_to_string(&files.report).unwrap_or_default();
    if text.trim().is_empty() {
        return Err(Failure::precondition(format!(
            "{} finished without a report ({}); see {}",
            codex.name(),
            files.report.display(),
            files.stream.display()
        )));
    }
    let reading = Reading::of(&text).map_err(|e| {
        Failure::precondition(format!("{e}: {}; read it there", files.report.display()))
    })?;
    let ledger = json!({
        "label": args.label,
        "round": review::round(&args.label, args.previous.is_some()),
        "base": args.base,
        "head": inputs.head,
        "backend": codex.name(),
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
    let mut done = Done::new(
        reading.summary(&args.label, &run.report),
        json!({
            "label": args.label,
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

/// Lands the run's `review` event in the ledger, through the daemon.
fn log_event(paths: &Paths, record: &Record<'_>) -> Result<(), Failure> {
    let session = env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let event = Event::from_args("review", &record.pairs(), session.as_deref())
        .map_err(|e| Failure::usage(e.to_string()))?;
    let params = LogParams {
        kind: event.kind().to_owned(),
        fields: event.fields().clone(),
    };
    let _: LogResult = client::call(paths, LOG, &params)?;
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
        let diff = repo.diff(base, DIFF_CONTEXT).map_err(git_failed)?;
        if diff.is_empty() {
            return Err(Failure::usage(format!(
                "no committed changes since {base} (commit first)"
            )));
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
            commits: repo.commits(base).map_err(git_failed)?,
            diff_stat: repo.diff_stat(base).map_err(git_failed)?,
            diff,
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

/// The files of one review, in `<state>/reviews/<repo>/`.
struct Files {
    bundle: PathBuf,
    /// An issue ref's spec, as fetched.
    spec: PathBuf,
    prompt: PathBuf,
    report: PathBuf,
    stream: PathBuf,
    /// The findings ledger read from the report.
    findings: PathBuf,
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
            stream: file("jsonl"),
            findings: file("findings.json"),
        })
    }

    /// Removes what an earlier run under the same label left, so nothing stale is read.
    fn clear_outputs(&self) -> Result<(), Failure> {
        for path in [&self.report, &self.stream, &self.findings] {
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

/// Codex's own dir: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> PathBuf {
    let var = |key: &str| env::var_os(key).filter(|value| !value.is_empty());
    var("CODEX_HOME").map_or_else(
        || Path::new(&var("HOME").unwrap_or_default()).join(".codex"),
        PathBuf::from,
    )
}

/// The spec's name for the prompt and its text. An issue ref is fetched with gh and saved to
/// `saved`, since the reviewer's sandbox has no network; anything else names a file.
fn fetch_spec(spec: &str, saved: &Path) -> Result<(String, String), Failure> {
    let Some(number) = review::issue_number(spec) else {
        return Ok((spec.to_owned(), read_given("spec", Path::new(spec))?));
    };
    let text = Gh.issue(number).map_err(|e| {
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
fn reviewer_failed(reviewer: &impl Reviewer, e: &ReviewerError, stream: &Path) -> Failure {
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
