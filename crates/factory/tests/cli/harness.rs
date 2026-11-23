//! The sandbox's fakes behave as later tests rely on.

use crate::support::{Sandbox, run};

#[test]
fn an_unscripted_fake_refuses_and_records_its_call() {
    let sandbox = Sandbox::new();

    let out = run(sandbox.command("gh", &["pr", "checks", "12"]));

    assert_eq!(out.code, 127);
    assert_eq!(out.stderr, "fake gh: no script for: pr checks 12\n");
    assert_eq!(sandbox.calls("gh"), vec![vec!["pr", "checks", "12"]]);
}

#[test]
fn a_scripted_fake_plays_back_its_script_and_records_each_call() {
    let sandbox = Sandbox::new();
    sandbox.fake("adb", "echo \"devices: $#\"; exit 3");

    let first = run(sandbox.command("adb", &["devices", "-l"]));
    let second = run(sandbox.command("adb", &["shell", "echo two words"]));

    assert_eq!((first.code, first.stdout.as_str()), (3, "devices: 2\n"));
    assert_eq!((second.code, second.stdout.as_str()), (3, "devices: 2\n"));
    assert_eq!(
        sandbox.calls("adb"),
        vec![vec!["devices", "-l"], vec!["shell", "echo two words"]]
    );
}
