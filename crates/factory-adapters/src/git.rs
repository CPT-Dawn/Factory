//! The repository under review, through `git`.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::process;

/// How long one git command may take.
const TIMEOUT: Duration = Duration::from_secs(60);

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
    /// The git dir its worktrees share, which holds their objects and refs.
    fn common_dir(&self) -> Result<PathBuf, GitError>;
}

/// The repository at a directory, through the `git` on PATH.
#[derive(Debug)]
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

impl GitRepo {
    /// A detached checkout of `rev` at `dir`, outside this repository, in a worktree of its
    /// own: what is done in it leaves this repository's working tree alone. Whatever an earlier
    /// checkout left at `dir` is removed first. The checkout is removed when it is dropped.
    pub fn checkout(&self, rev: &str, dir: &Path) -> Result<Checkout, GitError> {
        let path = dir
            .to_str()
            .ok_or_else(|| GitError(format!("{} is not UTF-8", dir.display())))?;
        let checkout = Checkout {
            repo: GitRepo::at(&self.dir),
            dir: dir.to_owned(),
        };
        checkout.remove();
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| GitError(format!("cannot create {}: {e}", parent.display())))?;
        }
        self.git(&["worktree", "add", "--detach", "--quiet", path, rev])?;
        Ok(checkout)
    }

    /// Forgets the worktrees whose directory is gone.
    pub fn prune_worktrees(&self) -> Result<(), GitError> {
        self.git(&["worktree", "prune"]).map(drop)
    }
}

/// A throwaway checkout, from [`GitRepo::checkout`]: removed, worktree and directory, when
/// dropped, however the work in it ended.
#[derive(Debug)]
pub struct Checkout {
    repo: GitRepo,
    dir: PathBuf,
}

impl Checkout {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Removes the worktree and whatever is left of its directory, and has the repository
    /// forget it. Nothing to remove is not a failure.
    fn remove(&self) {
        if let Some(path) = self.dir.to_str() {
            let _ = self
                .repo
                .output(&["worktree", "remove", "--force", "--force", path]);
        }
        let _ = fs::remove_dir_all(&self.dir);
        let _ = self.repo.prune_worktrees();
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        self.remove();
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
        let common = self.common_dir()?;
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

    fn common_dir(&self) -> Result<PathBuf, GitError> {
        let dir = self.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        Ok(PathBuf::from(dir.trim_end()))
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
    pub common_dir: PathBuf,
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

    fn common_dir(&self) -> Result<PathBuf, GitError> {
        Ok(self.common_dir.clone())
    }
}
