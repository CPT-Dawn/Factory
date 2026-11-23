//! The repository under review, through `git`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

    fn git(&self, args: &[&str]) -> Result<String, GitError> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let shown = args.join(" ");
        let output =
            process::run(cmd, TIMEOUT).map_err(|e| GitError(format!("git {shown}: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GitError(format!(
                "git {shown} failed: {}",
                stderr.lines().next().unwrap_or_default()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
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
        match self.git(&["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")]) {
            Ok(sha) => Ok(Some(sha.trim_end().to_owned())),
            Err(_) => Ok(None),
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
}
