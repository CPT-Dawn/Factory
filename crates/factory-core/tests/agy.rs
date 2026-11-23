//! What agy's own log says about its terminal sandbox (#29).

use factory_core::agy::{self, Sandbox};

/// What agy 1.5 logged at the start of a real `agy -p --sandbox` run.
const STARTED: &str =
    "I1123 09:37:00.811680 1 session.go:87] Print mode: enabling terminal sandbox for this session";

#[test]
fn a_log_that_says_the_sandbox_started_is_started() {
    for log in [
        format!("I1123 printmode.go:209] Print mode: starting\n{STARTED}\n"),
        "I1123 x.go:1] --sandbox: enabling terminal sandbox for this session".to_owned(),
    ] {
        assert_eq!(agy::sandbox(&log), Sandbox::Started, "{log}");
    }
}

#[test]
fn a_log_that_says_the_sandbox_is_unavailable_is_unavailable_whatever_else_it_says() {
    let cases = [
        (
            "W1123 sandbox.go:40] Terminal sandbox unavailable: unprivileged user namespaces are disabled (kernel.unprivileged_userns_clone = 0)",
            "unprivileged user namespaces are disabled (kernel.unprivileged_userns_clone = 0)",
        ),
        (
            "W1123 printmode.go:90] Print mode: --mode plan is unavailable (terminal sandbox unavailable), continuing in the default mode",
            "Print mode: --mode plan is unavailable (terminal sandbox unavailable), continuing in the default mode",
        ),
        (
            "warning: --mode plan is not available because the terminal sandbox is not available on this host; continuing in the default mode.",
            "warning: --mode plan is not available because the terminal sandbox is not available on this host; continuing in the default mode.",
        ),
    ];
    for (line, why) in cases {
        let log = format!("{STARTED}\n{line}\n");
        assert_eq!(
            agy::sandbox(&log),
            Sandbox::Unavailable(why.to_owned()),
            "{line}"
        );
    }
}

#[test]
fn a_log_with_no_sign_of_the_sandbox_is_unknown() {
    for log in [
        "",
        "I1123 printmode.go:209] Print mode: starting\n",
        "terminal sandbox",
    ] {
        assert_eq!(agy::sandbox(log), Sandbox::Unknown, "{log:?}");
    }
}
