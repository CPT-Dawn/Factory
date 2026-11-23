//! Every `--timeout` and `--wait` reads one grammar: a bare number is seconds, and `90s`, `9m`
//! and `1h` work too.

use crate::support::Sandbox;

/// Every factory command that takes a `--timeout`.
const TIMED: &[&[&str]] = &[
    &["wait", "lead", "cart"],
    &["wait", "ci", "12"],
    &["wait", "review", "t1-r1"],
    &["wait", "claude"],
    &["wait", "lease", "1"],
    &["review", "main", "t1-r1"],
];

/// The help text with its wrapping undone.
fn help(sandbox: &Sandbox, args: &[&str]) -> String {
    let mut args = args.to_vec();
    args.push("--help");
    let out = sandbox.factory(&args);
    assert_eq!(out.code, 0, "{args:?}: {out:?}");
    out.stdout.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_timeout_says_in_its_help_that_it_takes_seconds_or_a_unit() {
    let sandbox = Sandbox::new();

    for args in TIMED {
        let shown = help(&sandbox, args);
        assert!(
            shown.contains("seconds, or 90s, 9m or 1h"),
            "{args:?}: {shown}"
        );
    }
}

#[test]
fn every_devlab_wait_says_in_its_help_that_it_takes_seconds_or_a_unit() {
    let sandbox = Sandbox::new();

    for args in [&["lab", "--help"][..], &["lab", "reserve", "--help"]] {
        let out = sandbox.factory(args);
        assert_eq!(out.code, 0, "{args:?}: {out:?}");
        // The help is long, so the line names the file that holds it.
        let file = out.stdout.trim_end().rsplit(' ').next().unwrap_or_default();
        let shown = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{e}: {out:?}"));
        assert!(
            shown.contains("seconds, or 90s, 9m or 1h"),
            "{args:?}: {shown}"
        );
    }
}

#[test]
fn a_bare_number_is_a_timeout_in_seconds() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let out = sandbox.factory_in(&repo, &["wait", "review", "t9-r1", "--timeout", "1"]);

    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            124,
            "",
            "factory: no review t9-r1 ended within 1s; wait again\n"
        )
    );
}

#[test]
fn a_malformed_duration_is_refused_in_one_line_naming_the_flag() {
    let sandbox = Sandbox::new();

    for args in TIMED {
        for bad in ["soon", "1.5m", "9d", "9 m", "-1"] {
            let flag = format!("--timeout={bad}");
            let mut args = args.to_vec();
            args.push(&flag);

            let out = sandbox.factory(&args);

            assert_eq!(out.code, 2, "{args:?}: {out:?}");
            assert_eq!(out.stdout, "", "{args:?}");
            assert_eq!(out.stderr_lines().len(), 1, "{args:?}: {out:?}");
            assert!(out.stderr.contains("--timeout"), "{args:?}: {out:?}");
        }
    }
}
