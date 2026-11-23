//! `factory --help` stays short, and the command line refuses in one line.

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
