//! `factory review <base> <label> [spec] [prev]`: review.sh's arguments, a prepared review
//! bundle, one Codex run, and one line back.
//!
//! Every file of a review is kept in `<state>/reviews/<repo>/`, named by its label: the
//! bundle (`.bundle.md`), the prompt (`.prompt`), the report (`.md`) and Codex's event
//! stream (`.jsonl`, which factory-stats reads).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use factory_adapters::gh::{Gh, Issues};
use factory_adapters::git::{GitError, GitRepo, Repo};
use factory_adapters::reviewer::{Codex, Job, Reviewer};
use factory_core::exit::Exit;
use factory_core::review::{self, Bundle, Part, Reading};
use serde_json::json;

use crate::output::{Done, Failure};
use crate::paths::Paths;

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
    let cwd = env::current_dir().map_err(|e| {
        Failure::precondition(format!(
            "cannot read the working directory: {e}; cd into the repo under review"
        ))
    })?;
    let repo = GitRepo::at(&cwd);
    let name = repo.name().map_err(|e| {
        Failure::precondition(format!(
            "{e}; run factory review inside the repo under review"
        ))
    })?;
    let files = Files::new(&paths, &name, &args.label)?;
    let inputs = Inputs::gather(&repo, args, &files)?;
    write(&files.bundle, &inputs.bundle(args).render())?;
    let prompt = review::prompt(
        &args.base,
        &files.bundle.display().to_string(),
        inputs.spec.as_ref().map_or("none", |(name, _)| name),
        inputs.previous.as_ref().map(|(name, _)| name.as_str()),
    );
    write(&files.prompt, &prompt)?;
    let codex = Codex::configured_in(&codex_home());
    let job = Job {
        cwd,
        prompt: files.prompt.clone(),
        report: files.report.clone(),
        stream: files.stream.clone(),
        timeout: args.timeout,
    };
    codex
        .review(&job)
        .map_err(|e| Failure::precondition(format!("{} {e}", codex.name())))?;
    let text = fs::read_to_string(&files.report).unwrap_or_default();
    let reading = Reading::of(&text).map_err(|e| Failure::precondition(e.to_string()))?;
    let shown = files.report.display().to_string();
    let mut done = Done::new(
        reading.summary(&args.label, &shown),
        json!({"report": shown}),
    );
    done.more = reading.blocking_lines();
    if reading.counts.blocking > 0 {
        done.exit = Exit::NeedsAttention;
    }
    Ok(done)
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
        })
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

fn git_failed(e: GitError) -> Failure {
    Failure::precondition(format!("{e}; check the repository"))
}
