//! A lead's state, folded from what herdr says, its report and its waits.

use factory_core::exit::Exit;
use factory_core::lead::{AgentStatus, LeadFacts, LeadState, lead_name, slug};
use factory_core::wait::{Wait, WaitOn, Waits};

fn state(agent: Option<&str>, reported: bool, waiting_on: Option<&str>) -> LeadState {
    LeadState::of(LeadFacts {
        agent: agent.map(AgentStatus::parse),
        reported,
        waiting_on,
    })
}

#[test]
fn the_state_follows_the_report_then_herdr_then_the_wait() {
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
        (Some("blocked"), true, None, "finished"),
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
