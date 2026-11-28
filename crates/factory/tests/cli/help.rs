//! `factory --help` stays short, and the command line refuses in one line.

use std::process::Command;

use crate::support::Sandbox;

#[test]
fn help_fits_in_about_thirty_lines_and_names_the_exit_codes() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["--help"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout_lines().len() <= 30, "{}", out.stdout);
    assert!(out.stdout.contains("log"), "{}", out.stdout);
    assert!(out.stdout.contains("--json"), "{}", out.stdout);
    assert!(
        out.stdout.contains("0 ok, 2 usage, 3 precondition"),
        "{}",
        out.stdout
    );
}

#[test]
fn a_bad_command_line_is_refused_in_one_line_with_exit_2() {
    let sandbox = Sandbox::new();

    for args in [
        &[][..],
        &["nonsense"],
        &["log"],
        &["log", "review", "--nope"],
    ] {
        let out = sandbox.factory(args);

        assert_eq!(out.code, 2, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "", "{args:?}");
        assert_eq!(out.stderr_lines().len(), 1, "{args:?}: {out:?}");
        assert!(out.stderr.starts_with("factory: "), "{out:?}");
        assert!(out.stderr.ends_with("; see factory --help\n"), "{out:?}");
    }
}

#[test]
fn a_bad_command_line_with_json_is_one_object() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["log", "--json"]);

    assert_eq!(out.code, 2, "{out:?}");
    let shown: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(shown["exit"], 2);
}

/// The build's commit, from the checkout the tests were built in: its short sha, or `unknown`
/// when there is no git to ask, as in a source tarball.
fn checkout_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map_or_else(
            || "unknown".to_owned(),
            |out| String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        )
}

/// Whether anything tracked or new under the workspace's source differs from its commit.
fn edited_since_commit() -> bool {
    Command::new("git")
        .args([
            "status",
            "--porcelain",
            "--",
            "../../crates",
            "../../Cargo.toml",
            "../../Cargo.lock",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_or(true, |out| !out.stdout.is_empty())
}

#[test]
fn version_names_the_build_by_its_commit() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["--version"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.stderr, "");
    let sha = checkout_sha();
    let prefix = format!("factory {} (", env!("CARGO_PKG_VERSION"));
    let build = out
        .stdout
        .trim_end()
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_default()
        .to_owned();
    // A tree with edits since its commit may say so; a tree without any must not.
    let accepted = if edited_since_commit() {
        vec![sha.clone(), format!("{sha}-dirty")]
    } else {
        vec![sha.clone()]
    };
    assert!(
        accepted.contains(&build),
        "{accepted:?} in {:?}",
        out.stdout
    );
    if sha != "unknown" {
        assert!(
            sha.len() == 7 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{sha}"
        );
    }
}
