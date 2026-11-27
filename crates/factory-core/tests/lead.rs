//! A lead's state, folded from what herdr says, its report and its waits.

use std::time::Duration;

use factory_core::exit::Exit;
use factory_core::lead::{
    AgentStatus, LeadFacts, LeadState, branch, claude_args, lead_name, prompt, slug, spawn_longest,
};
use factory_core::wait::{Wait, WaitOn, Waits};

fn state(agent: Option<&str>, reported: bool, waiting_on: Option<&str>) -> LeadState {
    LeadState::of(LeadFacts {
        agent: agent.map(AgentStatus::parse),
        reported,
        waiting_on,
    })
}

#[test]
fn a_prompt_outranks_the_report_which_outranks_the_wait() {
    let b = Some("lead-b");
    let cases = [
        // agent, reported, waiting on, state
        (Some("working"), false, None, "working"),
        (Some("unknown"), false, None, "working"),
        (Some("thinking"), false, None, "working"),
        (Some("blocked"), false, None, "blocked"),
        (Some("idle"), false, None, "asking"),
        (Some("done"), false, None, "asking"),
        (Some("idle"), true, None, "finished"),
        (Some("done"), true, None, "finished"),
        (Some("blocked"), true, None, "blocked"),
        (Some("working"), true, None, "working"),
        (None, true, None, "finished"),
        (None, false, None, "dead"),
        (None, false, b, "dead"),
        (Some("working"), false, b, "waiting: lead-b"),
        (Some("idle"), false, b, "waiting: lead-b"),
        (Some("blocked"), false, b, "blocked"),
    ];
    for (agent, reported, waiting_on, expected) in cases {
        assert_eq!(
            state(agent, reported, waiting_on).to_string(),
            expected,
            "{agent:?} reported={reported} waiting_on={waiting_on:?}"
        );
    }
}

#[test]
fn a_wait_answers_ok_only_on_finished_and_never_while_the_lead_works() {
    let cases = [
        (LeadState::Working, None),
        (LeadState::Waiting("lead-b".to_owned()), None),
        (LeadState::Finished, Some(Exit::Ok)),
        (LeadState::Blocked, Some(Exit::NeedsAttention)),
        (LeadState::Asking, Some(Exit::NeedsAttention)),
        (LeadState::Dead, Some(Exit::NeedsAttention)),
    ];
    for (state, exit) in cases {
        assert_eq!(state.wait_exit(), exit, "{state:?}");
        assert_eq!(
            state.shows_screen(),
            matches!(state, LeadState::Blocked | LeadState::Asking),
            "{state:?}"
        );
    }
}

#[test]
fn a_lead_is_named_by_its_slug_or_its_agent_name() {
    assert_eq!(lead_name("cart"), Ok("lead-cart".to_owned()));
    assert_eq!(lead_name("lead-cart-2"), Ok("lead-cart-2".to_owned()));
    assert_eq!(slug("lead-cart"), "cart");
    for bad in [
        "",
        "lead-",
        "Cart",
        "2cart",
        "cart_x",
        "a/b",
        &"x".repeat(28),
    ] {
        assert!(lead_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_pane_waits_on_what_it_registered_until_the_wait_clears() {
    let mut waits = Waits::default();
    let first = waits.register(Wait {
        waiter: Some("w1:p1".to_owned()),
        on: WaitOn::Lead("lead-b".to_owned()),
    });
    let second = waits.register(Wait {
        waiter: Some("w1:p1".to_owned()),
        on: WaitOn::Lead("lead-c".to_owned()),
    });
    waits.register(Wait {
        waiter: None,
        on: WaitOn::Lead("lead-d".to_owned()),
    });

    assert_eq!(
        waits
            .waiting_on("w1:p1")
            .map(ToString::to_string)
            .as_deref(),
        Some("lead-b")
    );
    assert_eq!(waits.waiting_on("w2:p1"), None);
    waits.clear(first);
    assert_eq!(
        waits
            .waiting_on("w1:p1")
            .map(ToString::to_string)
            .as_deref(),
        Some("lead-c")
    );
    waits.clear(second);
    assert_eq!(waits.waiting_on("w1:p1"), None);
}

#[test]
fn a_pane_waiting_on_a_review_shows_it_by_its_label() {
    let mut waits = Waits::default();
    waits.register(Wait {
        waiter: Some("w1:p1".to_owned()),
        on: WaitOn::Review("t9-r1".to_owned()),
    });

    assert_eq!(
        waits
            .waiting_on("w1:p1")
            .map(ToString::to_string)
            .as_deref(),
        Some("review t9-r1")
    );
}

#[test]
fn a_leads_branch_is_factory_slash_its_slug() {
    assert_eq!(branch("cart"), "factory/cart");
}

#[test]
fn the_lead_prompt_is_the_one_lead_sh_sent() {
    let text = prompt(
        "implement-spec",
        "#12",
        "factory/cart",
        "$HOME/leads/Shop/cart.md",
    );

    assert_eq!(
        text,
        "/implement-spec #12. Integration branch: factory/cart, already checked out here. \
         You run unattended: wherever a skill says to ask or confirm with the user, decide from \
         the spec and tickets yourself and list each such decision in your report. When everything \
         is finished, run devlab release (a finished lead must not hold a stack slot or a phone), \
         then write your final report to $HOME/leads/Shop/cart.md."
    );
}

#[test]
fn claude_starts_on_the_model_and_effort_with_the_plugins_disabled() {
    let plugins = ["expo@market".to_owned(), "cloud@flare".to_owned()];

    let args = claude_args("opus", "medium", &plugins);

    assert_eq!(
        args,
        [
            "--model",
            "opus",
            "--effort",
            "medium",
            "--settings",
            r#"{"enabledPlugins":{"expo@market":false,"cloud@flare":false}}"#,
        ]
    );
}

#[test]
fn claude_gets_no_settings_when_no_plugin_is_disabled() {
    assert_eq!(
        claude_args("opus", "high", &[]),
        ["--model", "opus", "--effort", "high"]
    );
}

/// Every herdr call at its limit: the worktree, Claude's two minutes to come up, the trust
/// dialog's minute, the prompt's minute, then five stalled tries of the pause and a minute
/// each, a busy pane's 10 s of waiting and one more call, with 10 s of connect and answer on
/// each of the 18 calls.
#[test]
fn a_spawns_longest_run_is_every_herdr_call_at_its_limit() {
    assert_eq!(
        spawn_longest(Duration::from_secs(2)),
        Duration::from_millis(740_300)
    );
}

#[test]
fn each_second_of_pause_adds_five_to_a_spawns_longest_run() {
    assert_eq!(
        spawn_longest(Duration::from_secs(30)) - spawn_longest(Duration::from_secs(2)),
        Duration::from_secs(5 * 28)
    );
}
