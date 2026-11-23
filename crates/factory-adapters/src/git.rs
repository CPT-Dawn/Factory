//! The repository under review, through `git`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::process;

/// How long one git command may take.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The most untracked files a staged-diff check reads, and the most bytes of them.
const UNTRACKED_LIMIT: usize = 200;
const UNTRACKED_BYTES: u64 = 16 << 20;

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
    /// The git dir its worktrees share, which holds their objects and refs.
    fn common_dir(&self) -> Result<PathBuf, GitError>;
}

/// The repository at a directory, through the `git` on PATH.
pub struct GitRepo {
    dir: PathBuf,
    /// `GIT_DIR` and `GIT_WORK_TREE` for every call, when the repository is not found from `dir`.
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
}

impl GitRepo {
    pub fn at(dir: &Path) -> GitRepo {
        GitRepo {
            dir: dir.to_owned(),
            git_dir: None,
            work_tree: None,
        }
    }

    /// The repository at `git_dir` and the working tree at `work_tree`, as git's `--git-dir`
    /// and `--work-tree` set them, each when given.
    pub fn pointed_at(mut self, git_dir: Option<PathBuf>, work_tree: Option<PathBuf>) -> GitRepo {
        self.git_dir = git_dir;
        self.work_tree = work_tree;
        self
    }

    /// Runs git to its end, whatever its exit status.
    fn output(&self, args: &[&str]) -> Result<Output, GitError> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(&self.dir)
            // A hook runs on every tool call; it must not take the index lock to refresh stat info.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .envs(self.git_dir.iter().map(|dir| ("GIT_DIR", dir)))
            .envs(self.work_tree.iter().map(|dir| ("GIT_WORK_TREE", dir)))
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

    fn staged_diff(&self, tracked: bool, added: Option<&[String]>) -> Result<String, GitError> {
        // What a commit made now takes: the index, or with `tracked` every tracked file as it is
        // in the working tree. An earlier `git add` puts its paths' working tree in the index
        // too, untracked files included.
        let all = [String::from(":/")];
        // Declared before the jobs that may borrow it.
        let empty_tree: String;
        let spec = added.map(|paths| if paths.is_empty() { &all[..] } else { paths });
        let mut jobs: Vec<Vec<&str>> = vec![if tracked {
            vec![HEAD]
        } else {
            vec!["--cached"]
        }];
        if let (Some(spec), false) = (spec, tracked) {
            jobs.push([&[HEAD, "--"][..], &as_strs(spec)].concat());
        }
        let mut jobs: Vec<Vec<&str>> = jobs
            .into_iter()
            .map(|job| [&["diff", "--no-color", "--no-ext-diff"][..], &job].concat())
            .collect();
        if let Some(spec) = spec {
            let listing = [
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--full-name",
                "--",
            ];
            jobs.push([&listing[..], &as_strs(spec)].concat());
            // The listing names files from the top of the working tree.
            jobs.push(vec!["rev-parse", "--show-toplevel"]);
        }
        // A hook runs on every tool call: the git calls run side by side.
        let mut outputs = self.all(&jobs)?;
        // Where a diff job names what it compares with, after its three options.
        const AGAINST: usize = 3;
        let against_head = |job: &Vec<&str>| job.get(AGAINST) == Some(&HEAD);
        if jobs
            .iter()
            .zip(&outputs)
            .any(|(job, out)| against_head(job) && !out.status.success())
            && self.commit(HEAD)?.is_none()
        {
            // Before the first commit, what is tracked is all new.
            empty_tree = self.git(&["hash-object", "-t", "tree", "/dev/null"])?;
            let empty = empty_tree.trim_end();
            for (job, out) in jobs.iter_mut().zip(outputs.iter_mut()) {
                if against_head(job) {
                    job[AGAINST] = empty;
                    *out = self.output(job)?;
                }
            }
        }
        let mut text = String::new();
        let mut untracked = None;
        let mut top = None;
        for (job, out) in jobs.iter().zip(&outputs) {
            if !out.status.success() {
                return Err(failed(job, out));
            }
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            match job[0] {
                "ls-files" => untracked = Some(stdout),
                "rev-parse" => top = Some(PathBuf::from(stdout.trim_end())),
                _ => text.push_str(&stdout),
            }
        }
        if let (Some(listing), Some(top)) = (untracked, top) {
            untracked_diff(&top, &listing, &mut text)?;
        }
        Ok(text)
    }
}

/// What the staged diff compares with; before the first commit, the empty tree takes its place.
const HEAD: &str = "HEAD";

fn as_strs(strings: &[String]) -> Vec<&str> {
    strings.iter().map(String::as_str).collect()
}

impl GitRepo {
    /// Runs each git command to its end, side by side, in order of `jobs`.
    fn all(&self, jobs: &[Vec<&str>]) -> Result<Vec<Output>, GitError> {
        std::thread::scope(|scope| {
            let running: Vec<_> = jobs
                .iter()
                .map(|job| scope.spawn(move || self.output(job)))
                .collect();
            running
                .into_iter()
                .map(|run| {
                    run.join()
                        .unwrap_or_else(|_| Err(GitError("a git call panicked".to_owned())))
                })
                .collect()
        })
    }
}

/// Appends the untracked files `ls-files -z --full-name` listed under `top` as the diff
/// `git add` would stage: each file's lines added, a symlink's target as its content.
fn untracked_diff(top: &Path, listing: &str, text: &mut String) -> Result<(), GitError> {
    // A nested repository is listed with a trailing slash; `git add` stages no content of it.
    let files: Vec<&str> = listing
        .split('\0')
        .filter(|file| !file.is_empty() && !file.ends_with('/'))
        .collect();
    if files.len() > UNTRACKED_LIMIT {
        return Err(GitError(format!(
            "{} untracked files are to be added, more than the {UNTRACKED_LIMIT} it checks",
            files.len()
        )));
    }
    let mut read = 0;
    for file in files {
        let path = top.join(file);
        let unreadable = |e: std::io::Error| GitError(format!("cannot read {file}: {e}"));
        let content = match std::fs::symlink_metadata(&path).map_err(unreadable)? {
            meta if meta.file_type().is_symlink() => std::fs::read_link(&path)
                .map_err(unreadable)?
                .to_string_lossy()
                .into_owned(),
            meta if meta.len() + read > UNTRACKED_BYTES => {
                return Err(GitError(format!(
                    "the untracked files to be added hold more than the {UNTRACKED_BYTES} bytes it checks"
                )));
            }
            _ => String::from_utf8_lossy(&std::fs::read(&path).map_err(unreadable)?).into_owned(),
        };
        read += content.len() as u64;
        text.push_str(&format!(
            "diff --git a/{file} b/{file}\nnew file mode 100644\n--- /dev/null\n+++ b/{file}\n@@ -0,0 +1 @@\n"
        ));
        for line in content.lines() {
            text.push('+');
            text.push_str(line);
            text.push('\n');
        }
    }
    Ok(())
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

    fn staged_diff(&self, _tracked: bool, _added: Option<&[String]>) -> Result<String, GitError> {
        Ok(self.staged_diff.clone())
    }
}
