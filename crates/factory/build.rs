//! Names the build by its commit, so `factory --version` and the events it writes say which
//! binary they came from: the short sha, with `-dirty` after it when the tree had edits that
//! were not committed, or `unknown` when there is no git to ask (a source tarball).
//!
//! The sha must not go stale in a worktree, where `.git` is a file: the build reruns when the
//! worktree's own HEAD or index changes, or the branch HEAD names moves (kept in the common git
//! dir), and when a source file the dirty flag reads changes.

use std::path::Path;
use std::process::Command;

fn main() {
    let build = git(&["rev-parse", "--short=7", "HEAD"]).map_or_else(
        || "unknown".to_owned(),
        |sha| {
            let edited = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|status| !status.is_empty());
            if edited { format!("{sha}-dirty") } else { sha }
        },
    );
    println!("cargo:rustc-env=FACTORY_BUILD={build}");
    watch();
}

/// What `git <args>` prints in this crate's directory, trimmed; none when it fails.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Tells cargo what the build depends on. A path that does not exist is left out: cargo reads a
/// missing one as always changed.
fn watch() {
    println!("cargo:rerun-if-changed=build.rs");
    for source in [
        "src",
        "Cargo.toml",
        "../factory-core/src",
        "../factory-adapters/src",
        "../../Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed={source}");
    }
    let (Some(git_dir), Some(common)) = (
        git(&["rev-parse", "--path-format=absolute", "--git-dir"]),
        git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ) else {
        return;
    };
    let mut paths = vec![
        Path::new(&git_dir).join("HEAD"),
        Path::new(&git_dir).join("index"),
        Path::new(&common).join("packed-refs"),
    ];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(Path::new(&common).join(branch));
    }
    for path in paths.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
