//! The repository under review, through `git`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::process;

/// How long one git command may take.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The most untracked files a staged-diff check reads.
const UNTRACKED_LIMIT: usize = 200;

/// A git command that failed: which one, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError(pub String);

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitError {}

/// What a review needs to know about the repository it runs in.
pub trait Repo {
    /// The main repository's name, the same from any of its worktrees.
    fn name(&self) -> Result<String, GitError>;
    /// The commit `rev` names; none when it names no commit.
    fn commit(&self, rev: &str) -> Result<Option<String>, GitError>;
    /// The full diff of `base...HEAD`, with `context` lines around each change.
    fn diff(&self, base: &str, context: u32) -> Result<String, GitError>;
    /// The diff stat of `base...HEAD`.
    fn diff_stat(&self, base: &str) -> Result<String, GitError>;
    /// The commits of `base..HEAD`, newest first: short sha, subject and body.
    fn commits(&self, base: &str) -> Result<String, GitError>;
    /// The top of the working tree.
    fn toplevel(&self) -> Result<PathBuf, GitError>;
    /// The git dir of this worktree, where the implementer leaves its evidence.
    fn git_dir(&self) -> Result<PathBuf, GitError>;
    /// What a commit made now would add: the staged diff; with `tracked`, every tracked change
    /// too (`commit -a`); with `added`, what `git add` of those pathspecs would stage as well,
    /// the whole tree for none, untracked files included.
    fn staged_diff(&self, tracked: bool, added: Option<&[String]>) -> Result<String, GitError>;
}

/// The repository at a directory, through the `git` on PATH.
pub struct GitRepo {
    dir: PathBuf,
}

impl GitRepo {
    pub fn at(dir: &Path) -> GitRepo {
        GitRepo {
            dir: dir.to_owned(),
        }
    }

    /// Runs git to its end, whatever its exit status.
    fn output(&self, args: &[&str]) -> Result<Output, GitError> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(&self.dir)
            // A hook runs on every tool call; it must not take the index lock to refresh stat info.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process::run(cmd, TIMEOUT).map_err(|e| GitError(format!("git {}: {e}", args.join(" "))))
    }

    /// Runs git and returns its stdout, failing when it exits non-zero.
    fn git(&self, args: &[&str]) -> Result<String, GitError> {
        let output = self.output(args)?;
        if !output.status.success() {
            return Err(failed(args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The error a git command that exited non-zero is: its first stderr line.
fn failed(args: &[&str], output: &Output) -> GitError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    GitError(format!(
        "git {} failed: {}",
        args.join(" "),
        stderr.lines().next().unwrap_or_default()
    ))
}

impl Repo for GitRepo {
    fn name(&self) -> Result<String, GitError> {
        let common = self.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let common = Path::new(common.trim_end());
        common
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| GitError(format!("no repository name in {}", common.display())))
    }

    fn commit(&self, rev: &str) -> Result<Option<String>, GitError> {
        let args = ["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")];
        let output = self.output(&args)?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_owned(),
            )),
            // `--verify -q` exits 1, silently, for a rev that names no commit.
            Some(1) => Ok(None),
            _ => Err(failed(&args, &output)),
        }
    }

    fn diff(&self, base: &str, context: u32) -> Result<String, GitError> {
        self.git(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            &format!("-U{context}"),
            &format!("{base}...HEAD"),
        ])
    }

    fn diff_stat(&self, base: &str) -> Result<String, GitError> {
        self.git(&["diff", "--no-color", "--stat", &format!("{base}...HEAD")])
    }

    fn commits(&self, base: &str) -> Result<String, GitError> {
        self.git(&[
            "log",
            "--no-color",
            "--format=%h %s%n%n%b",
            &format!("{base}..HEAD"),
        ])
    }

    fn toplevel(&self) -> Result<PathBuf, GitError> {
        let top = self.git(&["rev-parse", "--show-toplevel"])?;
        Ok(PathBuf::from(top.trim_end()))
    }

    fn git_dir(&self) -> Result<PathBuf, GitError> {
        let dir = self.git(&["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(dir.trim_end()))
    }

    fn staged_diff(&self, tracked: bool, added: Option<&[String]>) -> Result<String, GitError> {
        let diff = ["diff", "--no-color", "--no-ext-diff"];
        // Before the first commit there is no HEAD to compare with; any other failure is real.
        let head = self
            .output(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?
            .status
            .success();
        let mut text = if tracked && head {
            self.git(&[&diff[..], &["HEAD"]].concat())?
        } else {
            self.git(&[&diff[..], &["--cached"]].concat())?
        };
        let Some(paths) = added else {
            return Ok(text);
        };
        // What `git add paths` stages: no paths named is the whole tree.
        let top = GitRepo::at(&self.toplevel()?);
        let all = [String::from(":/")];
        let paths = if paths.is_empty() { &all[..] } else { paths };
        let spec: Vec<&str> = paths.iter().map(String::as_str).collect();
        if !tracked {
            let tail = [&["--"][..], &spec].concat();
            let against = if head { &["HEAD"][..] } else { &[] };
            text.push_str(&self.git(&[&diff[..], against, &tail].concat())?);
        }
        let listing = [
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--full-name",
                "--",
            ][..],
            &spec,
        ]
        .concat();
        let untracked = self.git(&listing)?;
        let files: Vec<&str> = untracked
            .split('\0')
            .filter(|file| !file.is_empty())
            .collect();
        if files.len() > UNTRACKED_LIMIT {
            return Err(GitError(format!(
                "{} untracked files are to be added, more than the {UNTRACKED_LIMIT} it checks",
                files.len()
            )));
        }
        for file in files {
            let args = [&diff[..], &["--no-index", "--", "/dev/null", file]].concat();
            let output = top.output(&args)?;
            // Exit 1 is how `--no-index` says the files differ.
            if !matches!(output.status.code(), Some(0 | 1)) {
                return Err(failed(&args, &output));
            }
            text.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        Ok(text)
    }
}

/// A repository held in memory, for tests of code that reads one.
#[derive(Clone, Debug, Default)]
pub struct FakeRepo {
    pub name: String,
    /// Each known rev and the commit it names.
    pub revs: Vec<(String, String)>,
    pub diff: String,
    pub diff_stat: String,
    pub commits: String,
    pub toplevel: PathBuf,
    pub git_dir: PathBuf,
    pub staged_diff: String,
}

impl Repo for FakeRepo {
    fn name(&self) -> Result<String, GitError> {
        Ok(self.name.clone())
    }

    fn commit(&self, rev: &str) -> Result<Option<String>, GitError> {
        Ok(self
            .revs
            .iter()
            .find(|(known, _)| known == rev)
            .map(|(_, sha)| sha.clone()))
    }

    fn diff(&self, _base: &str, _context: u32) -> Result<String, GitError> {
        Ok(self.diff.clone())
    }

    fn diff_stat(&self, _base: &str) -> Result<String, GitError> {
        Ok(self.diff_stat.clone())
    }

    fn commits(&self, _base: &str) -> Result<String, GitError> {
        Ok(self.commits.clone())
    }

    fn toplevel(&self) -> Result<PathBuf, GitError> {
        Ok(self.toplevel.clone())
    }

    fn git_dir(&self) -> Result<PathBuf, GitError> {
        Ok(self.git_dir.clone())
    }

    fn staged_diff(&self, _tracked: bool, _added: Option<&[String]>) -> Result<String, GitError> {
        Ok(self.staged_diff.clone())
    }
}
