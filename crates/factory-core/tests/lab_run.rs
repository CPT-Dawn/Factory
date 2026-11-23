//! What a `devlab run` a signal ends says, and how the ledger names its outcome (#24).

use factory_core::lab::run::{End, Phase, killed, signal_name, stopped_queued, stopped_running};

#[test]
fn signals_go_by_their_linux_names_and_others_by_number() {
    let table = [
        (1, "SIGHUP"),
        (2, "SIGINT"),
        (9, "SIGKILL"),
        (13, "SIGPIPE"),
        (15, "SIGTERM"),
        (64, "SIG64"),
    ];
    for (signal, name) in table {
        assert_eq!(signal_name(signal), name, "signal {signal}");
    }
}

#[test]
fn an_outcome_is_an_exit_code_a_signal_a_stop_lost_or_a_daemon_stop() {
    let table = [
        (End::Exit(0), "exit 0"),
        (End::Exit(3), "exit 3"),
        (End::Signal(9), "signal SIGKILL"),
        (End::Stopped(15), "stopped SIGTERM"),
        (End::Lost, "lost"),
        (End::DaemonStopped, "daemon-stopped"),
    ];
    for (end, outcome) in table {
        assert_eq!(end.outcome(), outcome, "{end:?}");
    }
    assert_eq!(Phase::Queued.word(), "queued");
    assert_eq!(Phase::Running.word(), "running");
}

#[test]
fn the_messages_name_the_signal_the_seconds_and_what_was_going_on() {
    assert_eq!(
        stopped_queued(15, 42, "all 2 stack slots of demo are held"),
        "stopped by SIGTERM after 42 s, while queued: all 2 stack slots of demo are held"
    );
    assert_eq!(
        stopped_running("pnpm", 2, 7),
        "pnpm stopped by SIGINT after 7 s"
    );
    assert_eq!(killed("sh", 9, 0), "sh killed by SIGKILL after 0 s");
}
