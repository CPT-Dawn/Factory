//! What the daemon stops because nothing used it for long enough (#18): a ready emulator no lease
//! holds, after the emulator timer; a stack devlab owns on a slot no lease holds, after the stack
//! timer; never a stack that isn't devlab's.

use factory_core::lab::devices::Mode;
use factory_core::lab::idle::{
    IdleDevice, IdleEmulator, IdleSlot, Need, Sweep, Timers, sweep, unmet,
};

const NOW: f64 = 10_000.0;

/// Ten minutes for an emulator, forty-five for a stack, ten for a leased device: the defaults.
fn timers() -> Timers {
    Timers::from_minutes(10.0, 45.0, 10.0)
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
        &[],
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
            ..Sweep::default()
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

    let due = sweep(&[booting, held, never_idle], &[], &[], &[], timers(), NOW);

    assert_eq!(due, Sweep::default());
}

#[test]
fn a_stack_devlab_started_is_stopped_after_its_own_timer() {
    let due = sweep(
        &[],
        &[slot(0, Some(10_000 - 2700)), slot(1, Some(10_000 - 600))],
        &[],
        &[],
        timers(),
        NOW,
    );

    assert_eq!(
        due,
        Sweep {
            emulators: vec![],
            stacks: vec![("demo".to_owned(), 0)],
            next: Some(10_000.0 - 600.0 + 2700.0),
            ..Sweep::default()
        }
    );
}

#[test]
fn a_stack_that_isnt_devlabs_or_on_a_held_slot_is_never_stopped() {
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

    let due = sweep(&[], &[outside, held, never_idle], &[], &[], timers(), NOW);

    assert_eq!(due, Sweep::default());
}

#[test]
fn the_next_look_is_the_soonest_of_both_kinds_and_fractional_minutes_count() {
    let due = sweep(
        &[emulator(0, Some(9_990))],
        &[slot(0, Some(9_995))],
        &[],
        &[],
        Timers::from_minutes(0.5, 0.25, 10.0),
        NOW,
    );

    assert_eq!(due.next, Some(10_010.0));
    assert!(due.emulators.is_empty() && due.stacks.is_empty(), "{due:?}");
}

/// A phone a lease holds, its lease last used at `touched` and running nothing.
fn phone(lease: i64, serial: &str, touched: i64) -> IdleDevice {
    IdleDevice {
        lease,
        serial: serial.to_owned(),
        phone: true,
        busy: false,
        touched,
    }
}

fn needs(mode: Mode, want: u32) -> Need {
    Need { mode, want }
}

const TWENTY_MIN_AGO: i64 = 10_000 - 1200;

#[test]
fn a_held_phone_idle_past_its_timer_is_reclaimed_while_a_waiter_needs_a_phone() {
    let due = sweep(
        &[],
        &[],
        &[phone(1, "P1", TWENTY_MIN_AGO)],
        &[needs(Mode::Phone, 1)],
        timers(),
        NOW,
    );

    assert_eq!(due.devices, vec![(1, "P1".to_owned())]);
}

#[test]
fn an_idle_phone_is_kept_with_no_waiter_or_while_its_lease_runs_a_command() {
    let no_waiter = sweep(
        &[],
        &[],
        &[phone(1, "P1", TWENTY_MIN_AGO)],
        &[],
        timers(),
        NOW,
    );
    let running = IdleDevice {
        busy: true,
        ..phone(1, "P1", TWENTY_MIN_AGO)
    };
    let busy = sweep(
        &[],
        &[],
        &[running],
        &[needs(Mode::Phone, 1)],
        timers(),
        NOW,
    );

    assert_eq!((no_waiter, busy), (Sweep::default(), Sweep::default()));
}

#[test]
fn only_the_kind_a_waiter_needs_is_reclaimed_and_only_as_many_as_it_wants() {
    let emulator = IdleDevice {
        phone: false,
        ..phone(3, "emulator-5580", 0)
    };
    let held = [phone(1, "P1", TWENTY_MIN_AGO), phone(2, "P2", 0), emulator];

    let one_phone = sweep(&[], &[], &held, &[needs(Mode::Phone, 1)], timers(), NOW);
    let any_two = sweep(&[], &[], &held, &[needs(Mode::Any, 2)], timers(), NOW);

    // The longest idle goes first.
    assert_eq!(one_phone.devices, vec![(2, "P2".to_owned())]);
    assert_eq!(
        any_two.devices,
        vec![(2, "P2".to_owned()), (3, "emulator-5580".to_owned())]
    );
}

#[test]
fn a_phone_idle_for_less_than_its_timer_is_due_when_it_reaches_it() {
    let due = sweep(
        &[],
        &[],
        &[phone(1, "P1", 10_000 - 60)],
        &[needs(Mode::Phone, 1)],
        timers(),
        NOW,
    );

    assert_eq!(
        due,
        Sweep {
            next: Some(10_000.0 - 60.0 + 600.0),
            ..Sweep::default()
        }
    );
}

#[test]
fn a_free_device_meets_a_need_before_any_is_taken_from_a_lease() {
    // A phone just taken back for a waiter is free until the waiter's grant takes it: that
    // need is met, and the next look takes nothing more for it.
    assert_eq!(unmet(&[needs(Mode::Phone, 1)], 1, 0), Vec::<Need>::new());
    assert_eq!(
        unmet(&[needs(Mode::Phone, 2)], 1, 3),
        vec![needs(Mode::Phone, 1)]
    );
    // A need of either kind takes what the kind-bound needs leave over.
    assert_eq!(
        unmet(
            &[
                needs(Mode::Any, 2),
                needs(Mode::Emulator, 1),
                needs(Mode::Phone, 1)
            ],
            1,
            2
        ),
        vec![needs(Mode::Any, 1)]
    );
    assert_eq!(
        unmet(&[needs(Mode::Emulator, 1)], 4, 0),
        vec![needs(Mode::Emulator, 1)]
    );
}
