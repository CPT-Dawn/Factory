//! `.githooks/commit-msg`: a subject is a conventional commit of 72 characters or fewer, on
//! commits and on merges, with one line naming the rule and the length when it is not.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// The hooks folder of this checkout.
fn hooks() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.githooks")
}

/// A fresh repository in a temp dir with the hooks wired, and one commit on `main`.
fn repo() -> TempDir {
    let dir = tempfile::tempdir().expect("make a temp repo");
    for args in [
        &["init", "-q", "-b", "main"][..],
        &[
            "config",
            "core.hooksPath",
            hooks().to_str().expect("a UTF-8 path"),
        ],
        &["commit", "-q", "--allow-empty", "-m", "chore: start"],
    ] {
        let out = git(dir.path(), args);
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    dir
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("run git")
}

/// Commits with `message`: whether git took it, and what it said on stderr.
fn commit(dir: &Path, message: &str) -> (bool, String) {
    let out = git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A conventional subject exactly `len` characters long.
fn subject_of(len: usize) -> String {
    let head = "feat(lab): ";
    format!("{head}{}", "x".repeat(len - head.len()))
}

#[test]
fn a_conventional_subject_of_72_characters_is_taken() {
    let dir = repo();
    for subject in [
        subject_of(72),
        "fix(auth)!: stop tokens from ghosting users".to_owned(),
        "docs: one line".to_owned(),
        "merge: one notification per change, and the #52 loose ends (#65)".to_owned(),
    ] {
        assert_eq!(
            commit(dir.path(), &subject),
            (true, String::new()),
            "{subject}"
        );
    }
}

#[test]
fn a_subject_over_72_characters_is_refused_in_one_line_naming_its_length() {
    let dir = repo();
    let (taken, said) = commit(dir.path(), &format!("{}\n\nA body.", subject_of(73)));
    assert!(!taken);
    assert_eq!(said.lines().count(), 1, "{said}");
    assert!(
        said.starts_with("commit-msg: subject is 73 characters, over the 72 limit: feat(lab): x"),
        "{said}"
    );
    // Characters, not bytes.
    let wide = format!("feat: {}", "é".repeat(66));
    assert!(commit(dir.path(), &wide).0);
}

#[test]
fn a_subject_that_is_not_a_conventional_commit_is_refused() {
    let dir = repo();
    for subject in [
        "Fix the thing",
        "Feat: shout",
        "feat:no space",
        "feat(): empty scope",
        "wip",
    ] {
        let (taken, said) = commit(dir.path(), subject);
        assert!(!taken, "{subject}");
        assert_eq!(said.lines().count(), 1, "{said}");
        assert!(
            said.starts_with(
                "commit-msg: subject is not a conventional commit (type(scope): subject), "
            ),
            "{said}"
        );
        assert!(
            said.contains(&format!(
                "{} characters: {subject}",
                subject.chars().count()
            )),
            "{said}"
        );
    }
}

#[test]
fn fixup_and_squash_subjects_git_makes_are_taken_at_any_length() {
    let dir = repo();
    assert!(commit(dir.path(), &subject_of(70)).0);
    for how in ["--fixup=HEAD", "--squash=HEAD"] {
        let out = git(
            dir.path(),
            &["commit", "-q", "--allow-empty", how, "--no-edit"],
        );
        assert!(out.status.success(), "{how}: {out:?}");
    }
    let log = git(dir.path(), &["log", "-2", "--format=%s"]);
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.lines().all(|s| s.chars().count() > 72), "{log}");
}

#[test]
fn a_revert_git_words_is_taken_at_any_length() {
    let dir = repo();
    std::fs::write(dir.path().join("file"), "x").expect("write a file");
    assert!(git(dir.path(), &["add", "file"]).status.success());
    assert!(commit(dir.path(), &subject_of(72)).0);
    let out = git(dir.path(), &["revert", "--no-edit", "HEAD"]);
    assert!(out.status.success(), "{out:?}");
    // The same subject written by hand, with no body naming the commit, is held to 72.
    let by_hand = format!("Revert \"{}\"", subject_of(72));
    let (taken, said) = commit(dir.path(), &by_hand);
    assert!(!taken);
    assert!(
        said.starts_with("commit-msg: subject is 81 characters, over the 72 limit"),
        "{said}"
    );
}

#[test]
fn a_merge_with_a_long_subject_is_refused_but_gits_own_merge_subject_is_taken() {
    let dir = repo();
    let branch = format!("retro/{}", "b".repeat(70));
    for args in [
        &["switch", "-q", "-c", branch.as_str()][..],
        &["commit", "-q", "--allow-empty", "-m", "feat: on the branch"],
        &["switch", "-q", "main"],
    ] {
        assert!(git(dir.path(), args).status.success(), "{args:?}");
    }

    let long = git(
        dir.path(),
        &["merge", "-q", "--no-ff", "-m", &subject_of(78), &branch],
    );
    assert!(!long.status.success(), "{long:?}");
    let said = String::from_utf8_lossy(&long.stderr);
    assert!(
        said.contains("commit-msg: subject is 78 characters, over the 72 limit"),
        "{said}"
    );
    let _ = git(dir.path(), &["merge", "--abort"]);

    // git's own wording, long only for the branch's name.
    let own = git(
        dir.path(),
        &["merge", "-q", "--no-ff", "--no-edit", &branch],
    );
    assert!(own.status.success(), "{own:?}");
    let subject = git(dir.path(), &["log", "-1", "--format=%s"]);
    assert_eq!(
        String::from_utf8_lossy(&subject.stdout).trim(),
        format!("Merge branch '{branch}'")
    );
}
