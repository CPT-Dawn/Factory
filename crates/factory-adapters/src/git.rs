//! The repository under review, through `git`.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use factory_core::hook::{Patch, Push, PushRef};

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
    /// The tree of the commit `rev` names: what it holds, whatever its history; none when
    /// it names no commit.
    fn tree(&self, rev: &str) -> Result<Option<String>, GitError>;
    /// The full diff of `base...HEAD`, with `context` lines around each change. Lockfiles are
    /// left out: they show in the diff stat only.
    fn diff(&self, base: &str, context: u32) -> Result<String, GitError>;
    /// Whether the commit `rev` names is an ancestor of HEAD (or HEAD itself).
    fn is_ancestor(&self, rev: &str) -> Result<bool, GitError>;
    /// The diff stat of `base...HEAD`.
    fn diff_stat(&self, base: &str) -> Result<String, GitError>;
    /// The commits of `base..HEAD`, newest first, one line each: short sha, author email, the
    /// subject's length in characters, and the subject.
    fn commits(&self, base: &str) -> Result<String, GitError>;
    /// `git diff --numstat` of `base...HEAD`: each file's added and deleted lines.
    fn numstat(&self, base: &str) -> Result<String, GitError>;
    /// The branch checked out, empty when HEAD is detached.
    fn branch(&self) -> Result<String, GitError>;
    /// Whether a local branch called `branch` exists.
    fn branch_exists(&self, branch: &str) -> Result<bool, GitError>;
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
    /// The commits `push` would send, oldest first, each with its patch (a merge's as a
    /// combined diff), worked out from the refs here, with no call to the remote. A branch the
    /// remote has, as its remote-tracking ref says, sends the commits that ref lacks; a new
    /// branch, and a tag, every commit that no remote-tracking ref has. A ref that names no
    /// commit sends nothing, as git refuses to push it.
    fn sent(&self, push: &Push) -> Result<Vec<Patch>, GitError>;
    /// A detached checkout of `rev` at `dir`, outside this repository, in a worktree of its
    /// own: what is done in it leaves this repository's working tree alone. Whatever an earlier
    /// checkout left at `dir` is removed first. The checkout is removed when it is dropped.
    fn checkout(&self, rev: &str, dir: &Path) -> Result<Checkout, GitError>;
    /// Forgets the worktrees whose directory is gone.
    fn prune_worktrees(&self) -> Result<(), GitError>;
}

/// Opens the repository at a directory: how an adapter handed only a directory reaches it.
pub trait Repos: fmt::Debug + Send + Sync {
    fn at(&self, dir: &Path) -> Box<dyn Repo>;
}

/// The repositories the `git` on PATH finds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Git;

impl Repos for Git {
    fn at(&self, dir: &Path) -> Box<dyn Repo> {
        Box::new(GitRepo::at(dir))
    }
}

/// The repository at a directory, through the `git` on PATH.
#[derive(Clone, Debug)]
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

    /// The URL of the remote `origin`; none when there is no such remote.
    pub fn origin_url(&self) -> Option<String> {
        self.git(&["remote", "get-url", "origin"])
            .ok()
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
    }

    /// The branch each worktree of the repository has checked out (none for a detached one),
    /// this one among them.
    pub fn worktree_branches(&self) -> Result<Vec<String>, GitError> {
        Ok(self
            .git(&["worktree", "list", "--porcelain"])?
            .lines()
            .filter_map(|line| line.strip_prefix("branch refs/heads/"))
            .map(str::to_owned)
            .collect())
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
    /// The object `rev` names, full; none when it names none.
    fn object(&self, rev: &str) -> Result<Option<String>, GitError> {
        let args = ["rev-parse", "-q", "--verify", rev];
        let output = self.output(&args)?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_owned(),
            )),
            // `--verify -q` exits 1, silently, for a rev that names no object.
            Some(1) => Ok(None),
            _ => Err(failed(&args, &output)),
        }
    }

    fn git(&self, args: &[&str]) -> Result<String, GitError> {
        let output = self.output(args)?;
        if !output.status.success() {
            return Err(failed(args, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl GitRepo {
    /// Removes the worktree at `dir` and whatever is left of the directory, and forgets it.
    /// Nothing to remove is not a failure.
    fn remove_worktree(&self, dir: &Path) {
        if let Some(path) = dir.to_str() {
            let _ = self.output(&["worktree", "remove", "--force", "--force", path]);
        }
        let _ = fs::remove_dir_all(dir);
        let _ = self.prune_worktrees();
    }
}

/// A throwaway checkout, from [`Repo::checkout`]: removed, worktree and directory, when
/// dropped, however the work in it ended.
pub struct Checkout {
    dir: PathBuf,
    remove: Box<dyn Fn(&Path) + Send + Sync>,
}

impl Checkout {
    /// The checkout at `dir`, which `remove` removes when it is dropped.
    pub fn new(dir: PathBuf, remove: Box<dyn Fn(&Path) + Send + Sync>) -> Checkout {
        Checkout { dir, remove }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl fmt::Debug for Checkout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checkout").field("dir", &self.dir).finish()
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        (self.remove)(&self.dir);
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
    fn checkout(&self, rev: &str, dir: &Path) -> Result<Checkout, GitError> {
        let path = dir
            .to_str()
            .ok_or_else(|| GitError(format!("{} is not UTF-8", dir.display())))?;
        self.remove_worktree(dir);
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| GitError(format!("cannot create {}: {e}", parent.display())))?;
        }
        let repo = self.clone();
        let checkout = Checkout::new(
            dir.to_owned(),
            Box::new(move |dir| repo.remove_worktree(dir)),
        );
        self.git(&["worktree", "add", "--detach", "--quiet", path, rev])?;
        Ok(checkout)
    }

    fn prune_worktrees(&self) -> Result<(), GitError> {
        self.git(&["worktree", "prune"]).map(drop)
    }

    fn name(&self) -> Result<String, GitError> {
        let common = self.common_dir()?;
        common
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| GitError(format!("no repository name in {}", common.display())))
    }

    fn commit(&self, rev: &str) -> Result<Option<String>, GitError> {
        self.object(&format!("{rev}^{{commit}}"))
    }

    fn tree(&self, rev: &str) -> Result<Option<String>, GitError> {
        self.object(&format!("{rev}^{{commit}}^{{tree}}"))
    }

    fn diff(&self, base: &str, context: u32) -> Result<String, GitError> {
        self.git(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            &format!("-U{context}"),
            &format!("{base}...HEAD"),
            "--",
            ":/",
            ":(exclude,glob)**/bun.lock",
            ":(exclude,glob)**/bun.lockb",
            ":(exclude,glob)**/package-lock.json",
            ":(exclude,glob)**/pnpm-lock.yaml",
            ":(exclude,glob)**/yarn.lock",
            ":(exclude,glob)**/Cargo.lock",
        ])
    }

    fn diff_stat(&self, base: &str) -> Result<String, GitError> {
        self.git(&[
            "diff",
            "--no-color",
            "--stat=160",
            &format!("{base}...HEAD"),
        ])
    }

    fn numstat(&self, base: &str) -> Result<String, GitError> {
        self.git(&[
            "diff",
            "--no-color",
            "--no-renames",
            "--numstat",
            &format!("{base}...HEAD"),
        ])
    }

    fn branch(&self) -> Result<String, GitError> {
        Ok(self
            .git(&["branch", "--show-current"])?
            .trim_end()
            .to_owned())
    }

    fn is_ancestor(&self, rev: &str) -> Result<bool, GitError> {
        let args = ["merge-base", "--is-ancestor", rev, HEAD];
        let output = self.output(&args)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed(&args, &output)),
        }
    }

    fn commits(&self, base: &str) -> Result<String, GitError> {
        let log = self.git(&[
            "log",
            "--no-color",
            "--format=%h%x1f%ae%x1f%s",
            &format!("{base}..HEAD"),
        ])?;
        Ok(log
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, '\x1f');
                let (sha, email, subject) = (fields.next()?, fields.next()?, fields.next()?);
                Some(format!(
                    "{sha} {email} {} {subject}\n",
                    subject.chars().count()
                ))
            })
            .collect())
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, GitError> {
        let args = [
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{branch}"),
        ];
        let output = self.output(&args)?;
        match output.status.code() {
            Some(0) => Ok(true),
            // `--verify -q` exits 1, silently, for a ref that doesn't exist.
            Some(1) => Ok(false),
            _ => Err(failed(&args, &output)),
        }
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

    fn sent(&self, push: &Push) -> Result<Vec<Patch>, GitError> {
        self.sent_commits(push)
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

impl GitRepo {
    fn sent_commits(&self, push: &Push) -> Result<Vec<Patch>, GitError> {
        let mut sent: Vec<Patch> = Vec::new();
        for (src, dst) in self.updates(push)? {
            // Git refuses to push a ref that names no commit, so it sends nothing. A commit made
            // earlier in the same line is checked as a commit.
            let Some(local) = self.commit(&src)? else {
                continue;
            };
            let remote = match self.remote_branch(push.remote.as_deref(), &src, dst.as_deref())? {
                Some(remote) => vec![local, format!("^{remote}")],
                None => vec![local, "--not".to_owned(), "--remotes".to_owned()],
            };
            let mut args = vec![
                "log",
                "--reverse",
                "-p",
                "--cc",
                "--no-color",
                "--no-ext-diff",
                "--format=%x00%h",
            ];
            args.extend(as_strs(&remote));
            for chunk in self.git(&args)?.split('\0').skip(1) {
                let (commit, diff) = chunk.split_once('\n').unwrap_or((chunk, ""));
                // Two updates can send the same commit.
                if !sent.iter().any(|patch| patch.commit == commit) {
                    sent.push(Patch {
                        commit: commit.to_owned(),
                        diff: diff.to_owned(),
                    });
                }
            }
        }
        Ok(sent)
    }

    /// Each ref the push updates: what it sends, and the remote ref it goes to when the line
    /// names one.
    fn updates(&self, push: &Push) -> Result<Vec<(String, Option<String>)>, GitError> {
        let mut updates = Vec::new();
        for pushed in &push.refs {
            match pushed {
                PushRef::Current => updates.push((HEAD.to_owned(), None)),
                PushRef::Ref { src, dst } => updates.push((src.clone(), dst.clone())),
                PushRef::Branches | PushRef::Tags => {
                    let under = if *pushed == PushRef::Branches {
                        "refs/heads"
                    } else {
                        "refs/tags"
                    };
                    // A tag goes to the remote under its own name, which no remote-tracking ref
                    // follows.
                    let format = "--format=%(refname)%00%(*objecttype)%(objecttype)";
                    for line in self.git(&["for-each-ref", format, under])?.lines() {
                        let (name, kind) = line.split_once('\0').unwrap_or((line, ""));
                        // A tag of a tree or blob sends no commit.
                        if kind.starts_with("commit") {
                            let dst = (*pushed == PushRef::Tags).then(|| name.to_owned());
                            updates.push((name.to_owned(), dst));
                        }
                    }
                }
            }
        }
        Ok(updates)
    }

    /// The remote-tracking commit of the branch an update of `src` (to `dst` when named)
    /// goes to on `remote`: none for a branch the remote does not have, or a ref that is no
    /// branch.
    fn remote_branch(
        &self,
        remote: Option<&str>,
        src: &str,
        dst: Option<&str>,
    ) -> Result<Option<String>, GitError> {
        let branch = match dst {
            Some(dst) if dst.starts_with("refs/") && !dst.starts_with("refs/heads/") => {
                return Ok(None);
            }
            Some(dst) => dst.trim_start_matches("refs/heads/").to_owned(),
            None => {
                let full = self.output(&["rev-parse", "--symbolic-full-name", src])?;
                let full = String::from_utf8_lossy(&full.stdout);
                match full.trim().strip_prefix("refs/heads/") {
                    Some(branch) => branch.to_owned(),
                    None => return Ok(None),
                }
            }
        };
        // With no remote named, the branch's push ref says where it goes, else origin.
        let remote = match remote {
            Some(remote) => remote,
            None => {
                if dst.is_none() {
                    // A branch with nowhere to push to fails here; origin is where git push goes then.
                    let pushes_to = format!("{branch}@{{push}}");
                    if let Ok(Some(commit)) = self.commit(&pushes_to) {
                        return Ok(Some(commit));
                    }
                }
                "origin"
            }
        };
        self.commit(&format!("refs/remotes/{remote}/{branch}"))
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
    /// Each known rev and the tree of its commit.
    pub trees: Vec<(String, String)>,
    pub diff: String,
    pub diff_stat: String,
    /// The revs that are ancestors of HEAD.
    pub ancestors: Vec<String>,
    pub commits: String,
    pub numstat: String,
    /// The branch checked out.
    pub branch: String,
    /// The local branches that exist.
    pub branches: Vec<String>,
    pub toplevel: PathBuf,
    pub git_dir: PathBuf,
    pub staged_diff: String,
    /// What any push sends.
    pub sent: Vec<Patch>,
    pub common_dir: PathBuf,
    /// The checkouts made and not yet dropped, by directory; each is an empty directory.
    pub checkouts: Arc<Mutex<Vec<PathBuf>>>,
}

impl Repos for FakeRepo {
    fn at(&self, _dir: &Path) -> Box<dyn Repo> {
        Box::new(self.clone())
    }
}

impl Repo for FakeRepo {
    fn checkout(&self, _rev: &str, dir: &Path) -> Result<Checkout, GitError> {
        fs::create_dir_all(dir)
            .map_err(|e| GitError(format!("cannot create {}: {e}", dir.display())))?;
        let held = Arc::clone(&self.checkouts);
        lock(&held).push(dir.to_owned());
        Ok(Checkout::new(
            dir.to_owned(),
            Box::new(move |dir| {
                lock(&held).retain(|made| made != dir);
                let _ = fs::remove_dir_all(dir);
            }),
        ))
    }

    fn prune_worktrees(&self) -> Result<(), GitError> {
        Ok(())
    }

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

    fn tree(&self, rev: &str) -> Result<Option<String>, GitError> {
        Ok(self
            .trees
            .iter()
            .find(|(known, _)| known == rev)
            .map(|(_, tree)| tree.clone()))
    }

    fn diff(&self, _base: &str, _context: u32) -> Result<String, GitError> {
        Ok(self.diff.clone())
    }

    fn diff_stat(&self, _base: &str) -> Result<String, GitError> {
        Ok(self.diff_stat.clone())
    }

    fn is_ancestor(&self, rev: &str) -> Result<bool, GitError> {
        Ok(self.ancestors.iter().any(|known| known == rev))
    }

    fn commits(&self, _base: &str) -> Result<String, GitError> {
        Ok(self.commits.clone())
    }

    fn numstat(&self, _base: &str) -> Result<String, GitError> {
        Ok(self.numstat.clone())
    }

    fn branch(&self) -> Result<String, GitError> {
        Ok(self.branch.clone())
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, GitError> {
        Ok(self.branches.iter().any(|known| known == branch))
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

    fn sent(&self, _push: &Push) -> Result<Vec<Patch>, GitError> {
        Ok(self.sent.clone())
    }

    fn staged_diff(&self, _tracked: bool, _added: Option<&[String]>) -> Result<String, GitError> {
        Ok(self.staged_diff.clone())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
