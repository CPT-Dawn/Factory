//! What the daemon stops because nothing used it for long enough (#18): a ready emulator no lease
//! holds, after the emulator timer; a stack devlab started on a slot no lease holds, after the
//! stack timer; never a stack started outside devlab.

use factory_core::lab::idle::{IdleEmulator, IdleSlot, Sweep, Timers, sweep};

const NOW: f64 = 10_000.0;

/// Ten minutes for an emulator, forty-five for a stack: today's timers.
fn timers() -> Timers {
    Timers::from_minutes(10.0, 45.0)
}

fn emulator(idx: u32, idle_since: Option<i64>) -> IdleEmulator {
    IdleEmulator {
        idx,
        ready: true,
        held: false,
        idle_since,
    }
}

fn slot(slot: u32, idle_since: Option<i64>) -> IdleSlot {
    IdleSlot {
        project: "demo".to_owned(),
        slot,
        stack_ours: true,
        held: false,
        last_worktree: Some("/w/demo-a".to_owned()),
        idle_since,
    }
}

#[test]
fn an_emulator_idle_for_its_timer_is_stopped_and_one_idle_for_less_is_due_later() {
    let due = sweep(
        &[
            emulator(0, Some(10_000 - 600)),
            emulator(1, Some(10_000 - 599)),
        ],
        &[],
        timers(),
        NOW,
    );

    assert_eq!(
        due,
        Sweep {
            emulators: vec![0],
            stacks: vec![],
            next: Some(10_001.0),
        }
    );
}

#[test]
fn a_booting_held_or_never_idle_emulator_is_never_stopped() {
    let long_ago = Some(0);
    let booting = IdleEmulator {
        ready: false,
        ..emulator(0, long_ago)
    };
    let held = IdleEmulator {
        held: true,
        ..emulator(1, long_ago)
    };
    let never_idle = emulator(2, None);

    let due = sweep(&[booting, held, never_idle], &[], timers(), NOW);

    assert_eq!(due, Sweep::default());
}

#[test]
fn a_stack_devlab_started_is_stopped_after_its_own_timer() {
    let due = sweep(
        &[],
        &[slot(0, Some(10_000 - 2700)), slot(1, Some(10_000 - 600))],
        timers(),
        NOW,
    );

    assert_eq!(
        due,
        Sweep {
            emulators: vec![],
            stacks: vec![("demo".to_owned(), 0)],
            next: Some(10_000.0 - 600.0 + 2700.0),
        }
    );
}

#[test]
fn a_stack_started_outside_devlab_or_on_a_held_slot_is_never_stopped() {
    let long_ago = Some(0);
    let outside = IdleSlot {
        stack_ours: false,
        ..slot(0, long_ago)
    };
    let held = IdleSlot {
        held: true,
        ..slot(1, long_ago)
    };
    let never_idle = slot(2, None);
    let no_worktree = IdleSlot {
        last_worktree: None,
        ..slot(3, long_ago)
    };

    let due = sweep(
        &[],
        &[outside, held, never_idle, no_worktree],
        timers(),
        NOW,
    );

    assert_eq!(due, Sweep::default());
}

#[test]
fn the_next_look_is_the_soonest_of_both_kinds_and_fractional_minutes_count() {
    let due = sweep(
        &[emulator(0, Some(9_990))],
        &[slot(0, Some(9_995))],
        Timers::from_minutes(0.5, 0.25),
        NOW,
    );

    assert_eq!(due.next, Some(10_010.0));
    assert!(due.emulators.is_empty() && due.stacks.is_empty(), "{due:?}");
}
