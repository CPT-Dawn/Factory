//! Names the build by its commit, so `factory --version` and the events it writes say which
//! binary they came from: the short sha, with `-dirty` after it when a file the binary is built
//! from has uncommitted edits, or `unknown` when there is no git to ask (a source tarball, or a
//! tree unpacked inside some other repository).
//!
//! The name must not go stale, in a worktree too (there `.git` is a file). The build reruns when
//! a watched source path changes, the worktree's HEAD, index or reflog changes, or a branch ref
//! is written (the common git dir keeps them, loose or packed).

use std::env;
use std::path::Path;
use std::process::Command;

/// What the binary is built from, relative to this crate: the dirty flag reads these, and the
/// build reruns when any of them changes.
const SOURCES: [&str; 9] = [
    "build.rs",
    "Cargo.toml",
    "src",
    "../factory-core/Cargo.toml",
    "../factory-core/src",
    "../factory-adapters/Cargo.toml",
    "../factory-adapters/src",
    "../../Cargo.toml",
    "../../Cargo.lock",
];

fn main() {
    println!("cargo:rustc-env=FACTORY_BUILD={}", build());
    for source in SOURCES {
        println!("cargo:rerun-if-changed={source}");
    }
    watch_git();
}

/// `ed05528`, `ed05528-dirty` or `unknown`.
fn build() -> String {
    // The repository must be this workspace's own, not one the tree happens to sit inside.
    if git(&["rev-parse", "--show-prefix"]).as_deref() != Some("crates/factory/") {
        return "unknown".to_owned();
    }
    let Some(sha) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return "unknown".to_owned();
    };
    let mut status = vec!["status", "--porcelain", "--"];
    status.extend(SOURCES);
    match git(&status) {
        Some(edits) if !edits.is_empty() => format!("{sha}-dirty"),
        _ => sha,
    }
}

/// What `git <args>` prints in this crate's directory, trimmed; none when it fails.
fn git(args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(env!("CARGO_MANIFEST_DIR"));
    // A build run from a git hook (the pre-push verify) inherits GIT_DIR and its kin, which
    // would make git take this crate's directory for the top of the work tree.
    for (name, _) in env::vars_os().filter(|(name, _)| name.to_string_lossy().starts_with("GIT_")) {
        command.env_remove(name);
    }
    let out = command.output().ok().filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Tells cargo to rerun when the commit moves. A path that does not exist is left out: cargo
/// reads a missing one as always changed.
fn watch_git() {
    let (Some(git_dir), Some(common)) = (
        git(&["rev-parse", "--path-format=absolute", "--git-dir"]),
        git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ) else {
        return;
    };
    let (git_dir, common) = (Path::new(&git_dir), Path::new(&common));
    for path in [
        git_dir.join("HEAD"),
        git_dir.join("index"),
        git_dir.join("logs/HEAD"),
        common.join("packed-refs"),
        // A directory is watched whole: a branch that was packed gets a loose ref again when
        // it moves, and that file did not exist to be watched when the build ran.
        common.join("refs/heads"),
    ] {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
