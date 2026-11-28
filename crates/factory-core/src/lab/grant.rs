//! Whether one acquire request is served now: the queue, the per-owner cap and the slot layout
//! put together over one picture of the board.

use std::collections::BTreeSet;

use super::queue::{
    Counts, DeviceNeed, Holding, QueueEntry, devices_held_by, held_by, live_queue, queue_turn,
};
use super::slots::{SlotView, pick_slot};

/// The project a request asks a slot of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAsk {
    pub name: String,
    pub max_slots: u32,
    /// The per-owner cap on this machine.
    pub cap: u32,
    /// Whether the project runs a local stack.
    pub has_stack: bool,
    /// The free memory a stack start needs (its RAM and the host's floor), when the request
    /// starts one: a slot whose stack is down is not granted below it.
    pub stack_need_mb: Option<i64>,
}

/// One acquire request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// Its entry in the queue.
    pub waiter: i64,
    pub project: Option<ProjectAsk>,
    /// The slot of the lease this worktree and owner already hold, when they hold one: it asks
    /// for no new slot.
    pub existing: Option<Option<u32>>,
    /// The devices it asks for; none for no device.
    pub devices: Option<DeviceNeed>,
    /// Why the devices it asks for are not to be had now, when they are not: it waits once its
    /// turn in the queue comes, before any slot is looked at.
    pub devices_wait: Option<String>,
}

/// The board as the request sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Board {
    pub leases: Vec<Holding>,
    /// Every waiter, oldest first, the asker among them.
    pub queue: Vec<QueueEntry>,
    /// The asker's project's slots by index, as far as they are known. A slot is held when a
    /// lease in `leases` holds it, or when its view says so (its stack is still stopping).
    pub slots: Vec<SlotView>,
    /// TCP ports something listens on.
    pub listening: BTreeSet<u32>,
    /// The memory available to start a stack with, after CI's headroom.
    pub mem_available_mb: i64,
    /// The CI headroom already taken from `mem_available_mb`, in MB, to explain a wait.
    pub ci_headroom_mb: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Served: the slot it holds, and whether that is a new lease.
    Grant { slot: Option<u32>, new: bool },
    /// Not yet, and why.
    Wait(String),
    /// Never: it asks for more than its owner may ever hold. Why.
    Refuse(String),
}

/// The cap a request is held to: none when its lease exists, so it asks for no new slot.
pub fn request_cap(project: Option<&ProjectAsk>, existing: bool) -> Option<u32> {
    if existing {
        None
    } else {
        project.map(|p| p.cap)
    }
}

/// Serves `ask` now, or says why it waits. `now` is seconds since the epoch.
pub fn decide(ask: &Ask, board: &Board, now: i64) -> Decision {
    if let Some(d) = &ask.devices
        && d.want > d.cap
    {
        return Decision::Refuse(format!(
            "asks for {} devices; one owner may hold at most {} (maxDevicesPerOwner)",
            d.want, d.cap
        ));
    }
    // Every entry is read live: one a lease serves by now asks for no new slot, the asker's
    // own entry as the request itself sees it.
    let queue: Vec<QueueEntry> = live_queue(&board.queue, &board.leases)
        .into_iter()
        .map(|e| {
            if e.id != ask.waiter {
                return e;
            }
            QueueEntry {
                project: ask.project.as_ref().map(|p| p.name.clone()),
                cap: request_cap(ask.project.as_ref(), ask.existing.is_some()),
                max_slots: ask.project.as_ref().map_or(0, |p| p.max_slots),
                devices: ask.devices,
                ..e
            }
        })
        .collect();
    let (slots, devices) = (held_by(&board.leases), devices_held_by(&board.leases));
    let counts = Counts {
        slots: &slots,
        devices: &devices,
    };
    if let Some(why) = queue_turn(&queue, ask.waiter, &counts, now) {
        return Decision::Wait(why);
    }
    if let Some(why) = &ask.devices_wait {
        return Decision::Wait(why.clone());
    }
    if let Some(slot) = ask.existing {
        return Decision::Grant { slot, new: false };
    }
    let Some(project) = &ask.project else {
        return Decision::Grant {
            slot: None,
            new: true,
        };
    };
    let taken: BTreeSet<u32> = board
        .leases
        .iter()
        .filter(|l| l.project.as_deref() == Some(&project.name))
        .filter_map(|l| l.slot)
        .collect();
    // A slot is held by a lease on it, or while its stack is still being stopped.
    let slots: Vec<SlotView> = (0..project.max_slots)
        .map(|slot| {
            let view = board.slots.get(slot as usize).cloned().unwrap_or_default();
            SlotView {
                held: view.held || taken.contains(&slot),
                ..view
            }
        })
        .collect();
    let starved = project
        .stack_need_mb
        .filter(|need| board.mem_available_mb < *need)
        .map(|need| {
            format!(
                "too little memory to start a stack ({} MB free, need {need}){}",
                board.mem_available_mb,
                crate::lab::memory::after_ci(board.ci_headroom_mb)
            )
        });
    match pick_slot(
        &project.name,
        &slots,
        &board.listening,
        project.has_stack,
        starved.as_deref(),
    ) {
        Ok(slot) => Decision::Grant {
            slot: Some(slot),
            new: true,
        },
        Err(why) => Decision::Wait(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::config::slot_cap;
    use crate::lab::devices::Mode;
    use crate::lab::queue::waiter_capped;
    use proptest::prelude::*;

    const PROJECT: &str = "demo";
    const OWNERS: usize = 3;
    const WORKTREES: usize = 4;

    fn owner(o: usize) -> String {
        format!("{o}:start")
    }

    fn port(slot: u32) -> u32 {
        27000 + slot * 100
    }

    fn entry(id: i64, o: usize, wt: usize, cap: u32, max_slots: u32) -> QueueEntry {
        QueueEntry {
            id,
            label: format!("claude:{o} @ wt{wt}"),
            created: 0,
            owner: owner(o),
            owner_label: format!("claude:{o}"),
            worktree: format!("wt{wt}"),
            lease: None,
            project: Some(PROJECT.to_owned()),
            cap: Some(cap),
            max_slots,
            devices: None,
        }
    }

    /// Owner `o`'s lease from worktree `wt` on `slot`; its id is the slot's.
    fn holding(o: usize, wt: usize, slot: u32) -> Holding {
        Holding {
            id: i64::from(slot) + 1,
            project: Some(PROJECT.to_owned()),
            slot: Some(slot),
            devices: 0,
            worktree: format!("wt{wt}"),
            owner: owner(o),
            owner_label: format!("claude:{o}"),
        }
    }

    fn ask(waiter: i64, max_slots: u32, cap: u32, existing: Option<Option<u32>>) -> Ask {
        Ask {
            waiter,
            project: Some(ProjectAsk {
                name: PROJECT.to_owned(),
                max_slots,
                cap,
                has_stack: false,
                stack_need_mb: None,
            }),
            existing,
            devices: None,
            devices_wait: None,
        }
    }

    fn board(leases: Vec<Holding>, queue: Vec<QueueEntry>, max_slots: u32) -> Board {
        Board {
            leases,
            queue,
            slots: (0..max_slots)
                .map(|s| SlotView {
                    own_ports: vec![port(s)],
                    ..SlotView::default()
                })
                .collect(),
            listening: BTreeSet::new(),
            mem_available_mb: 0,
            ci_headroom_mb: 0,
        }
    }

    #[test]
    fn a_stack_start_waits_for_memory_when_no_slot_has_its_stack_running() {
        let mut a = ask(1, 2, 2, None);
        if let Some(p) = a.project.as_mut() {
            p.has_stack = true;
            p.stack_need_mb = Some(7168);
        }
        let mut b = board(vec![], vec![entry(1, 0, 0, 1, 2)], 2);
        b.mem_available_mb = 3400;
        assert_eq!(
            decide(&a, &b, 0),
            Decision::Wait(
                "too little memory to start a stack (3400 MB free, need 7168)".to_owned()
            )
        );
        // Once memory is back, or when a slot's stack already runs, it is granted.
        b.mem_available_mb = 7168;
        assert!(matches!(decide(&a, &b, 0), Decision::Grant { .. }));
        // CI's headroom is already taken from the free memory a wait names.
        b.mem_available_mb = 3400;
        b.ci_headroom_mb = 2048;
        assert_eq!(
            decide(&a, &b, 0),
            Decision::Wait(
                "too little memory to start a stack (3400 MB free, need 7168): too little memory after CI's headroom (2048 MB)".to_owned()
            )
        );
        b.ci_headroom_mb = 0;
        b.slots[1].stack_running = true;
        assert_eq!(
            decide(&a, &b, 0),
            Decision::Grant {
                slot: Some(1),
                new: true
            }
        );
    }

    #[test]
    fn the_head_of_the_queue_takes_the_lowest_free_slot() {
        let b = board(vec![holding(0, 0, 0)], vec![entry(1, 1, 1, 1, 2)], 2);
        assert_eq!(
            decide(&ask(1, 2, 1, None), &b, 0),
            Decision::Grant {
                slot: Some(1),
                new: true
            }
        );
    }

    #[test]
    fn a_request_behind_the_head_waits_naming_it() {
        let b = board(vec![], vec![entry(1, 0, 0, 1, 2), entry(2, 1, 1, 1, 2)], 2);
        assert_eq!(
            decide(&ask(2, 2, 1, None), &b, 0),
            Decision::Wait("queued behind claude:0 @ wt0 (waiting since 0 min)".to_owned())
        );
    }

    #[test]
    fn an_owner_at_its_cap_waits_and_steps_aside_for_the_next() {
        let b = board(
            vec![holding(0, 0, 0)],
            vec![entry(1, 0, 1, 1, 2), entry(2, 1, 2, 1, 2)],
            2,
        );
        assert_eq!(
            decide(&ask(1, 2, 1, None), &b, 0),
            Decision::Wait(
                "owner claude:0 already holds 1 of 2 demo stack slots; cap 1 (waits for one of its own leases to end)"
                    .to_owned()
            )
        );
        assert_eq!(
            decide(&ask(2, 2, 1, None), &b, 0),
            Decision::Grant {
                slot: Some(1),
                new: true
            }
        );
    }

    #[test]
    fn a_lease_that_exists_is_served_again_over_the_cap() {
        let b = board(vec![holding(0, 0, 0)], vec![entry(1, 0, 0, 1, 2)], 2);
        assert_eq!(
            decide(&ask(1, 2, 1, Some(Some(0))), &b, 0),
            Decision::Grant {
                slot: Some(0),
                new: false
            }
        );
    }

    #[test]
    fn a_request_its_own_lease_now_serves_keeps_its_place_ahead_of_later_owners() {
        // Owner 0 queued twice from wt0 at cap 1. The first was served with slot 0, so the
        // second asks for no new slot and is the head: owner 1 behind it waits its turn.
        let b = board(
            vec![holding(0, 0, 0)],
            vec![entry(2, 0, 0, 1, 2), entry(3, 1, 1, 1, 2)],
            2,
        );
        assert_eq!(
            decide(&ask(3, 2, 1, None), &b, 0),
            Decision::Wait("queued behind claude:0 @ wt0 (waiting since 0 min)".to_owned())
        );
        assert_eq!(
            decide(&ask(2, 2, 1, Some(Some(0))), &b, 0),
            Decision::Grant {
                slot: Some(0),
                new: false
            }
        );
    }

    #[test]
    fn a_slot_whose_stack_is_still_stopping_is_held_with_no_lease_on_it() {
        let mut b = board(vec![holding(0, 1, 1)], vec![entry(1, 1, 2, 1, 2)], 2);
        b.slots[0].held = true;
        assert_eq!(
            decide(&ask(1, 2, 1, None), &b, 0),
            Decision::Wait("all 2 stack slots of demo are held".to_owned())
        );
    }

    #[test]
    fn every_slot_held_is_a_wait_that_says_so() {
        let b = board(
            vec![holding(0, 0, 0), holding(1, 1, 1)],
            vec![entry(1, 2, 2, 1, 2)],
            2,
        );
        assert_eq!(
            decide(&ask(1, 2, 1, None), &b, 0),
            Decision::Wait("all 2 stack slots of demo are held".to_owned())
        );
    }

    #[test]
    fn a_request_without_a_project_holds_no_slot() {
        let b = board(vec![], vec![entry(1, 0, 0, 1, 2)], 2);
        let no_project = Ask {
            waiter: 1,
            project: None,
            existing: None,
            devices: None,
            devices_wait: None,
        };
        assert_eq!(
            decide(&no_project, &b, 0),
            Decision::Grant {
                slot: None,
                new: true
            }
        );
    }

    #[test]
    fn devices_to_be_waited_for_hold_a_request_after_its_turn_and_before_its_slot() {
        let b = board(vec![], vec![entry(1, 0, 0, 1, 2)], 2);
        let no_phone = Ask {
            devices_wait: Some("no free phone: 1 phone(s) busy".to_owned()),
            ..ask(1, 2, 1, None)
        };
        assert_eq!(
            decide(&no_phone, &b, 0),
            Decision::Wait("no free phone: 1 phone(s) busy".to_owned())
        );
    }

    fn need(mode: Mode, lacks: u32, short: bool) -> DeviceNeed {
        DeviceNeed {
            mode,
            want: lacks,
            lacks,
            cap: 2,
            short,
        }
    }

    /// Waiter `id` of owner `o` asking for `devices`, with no project.
    fn device_entry(id: i64, o: usize, devices: Option<DeviceNeed>) -> QueueEntry {
        QueueEntry {
            project: None,
            cap: None,
            max_slots: 0,
            devices,
            ..entry(id, o, o, 1, 2)
        }
    }

    fn device_ask(waiter: i64, devices: Option<DeviceNeed>) -> Ask {
        Ask {
            waiter,
            project: None,
            existing: None,
            devices,
            devices_wait: None,
        }
    }

    const NEW: Decision = Decision::Grant {
        slot: None,
        new: true,
    };

    #[test]
    fn a_phone_waiter_short_of_a_phone_lets_a_waiter_for_no_device_go_first() {
        let phone_waiter = device_entry(1, 0, Some(need(Mode::Phone, 1, true)));
        let b = board(vec![], vec![phone_waiter, entry(2, 1, 1, 1, 2)], 2);
        assert_eq!(
            decide(&ask(2, 2, 1, None), &b, 0),
            Decision::Grant {
                slot: Some(0),
                new: true
            }
        );
    }

    #[test]
    fn a_phone_waiter_short_of_a_phone_lets_an_emulator_waiter_go_first() {
        let phone_waiter = device_entry(1, 0, Some(need(Mode::Phone, 1, true)));
        let emulator = Some(need(Mode::Emulator, 1, false));
        let b = board(vec![], vec![phone_waiter, device_entry(2, 1, emulator)], 2);
        assert_eq!(decide(&device_ask(2, emulator), &b, 0), NEW);
    }

    /// A waiter queued only behind a device-short waiter it competes with waits on those devices
    /// too, though some are free for it: it steps aside for a request that needs none of them.
    #[test]
    fn a_waiter_held_behind_a_device_short_one_steps_aside_too() {
        let phone_waiter = device_entry(1, 0, Some(need(Mode::Phone, 1, true)));
        let any_waiter = device_entry(2, 1, Some(need(Mode::Any, 1, false)));
        let emulator = Some(need(Mode::Emulator, 1, false));
        for (asker, devices) in [(None, None), (Some(Mode::Emulator), emulator)] {
            let b = board(
                vec![],
                vec![
                    phone_waiter.clone(),
                    any_waiter.clone(),
                    device_entry(3, 2, devices),
                ],
                2,
            );
            let expected = match asker {
                // An emulator competes with any device: it keeps its place behind the second.
                Some(_) => {
                    Decision::Wait("queued behind claude:1 @ wt1 (waiting since 0 min)".to_owned())
                }
                None => NEW,
            };
            assert_eq!(
                decide(&device_ask(3, devices), &b, 0),
                expected,
                "{asker:?}"
            );
        }
    }

    #[test]
    fn waiters_for_the_same_devices_keep_their_order() {
        for (short_of, mine) in [
            (Mode::Phone, Mode::Phone),
            (Mode::Phone, Mode::Any),
            (Mode::Any, Mode::Emulator),
        ] {
            let first = device_entry(1, 0, Some(need(short_of, 1, true)));
            let mine = Some(need(mine, 1, false));
            let b = board(vec![], vec![first, device_entry(2, 1, mine)], 2);
            assert_eq!(
                decide(&device_ask(2, mine), &b, 0),
                Decision::Wait("queued behind claude:0 @ wt0 (waiting since 0 min)".to_owned()),
                "{short_of:?} then {mine:?}"
            );
        }
    }

    #[test]
    fn a_device_waiter_not_short_of_devices_keeps_its_place() {
        let first = device_entry(1, 0, Some(need(Mode::Phone, 1, false)));
        let b = board(vec![], vec![first, entry(2, 1, 1, 1, 2)], 2);
        assert_eq!(
            decide(&ask(2, 2, 1, None), &b, 0),
            Decision::Wait("queued behind claude:0 @ wt0 (waiting since 0 min)".to_owned())
        );
    }

    /// Owner 0's lease holding `devices` devices, with no slot.
    fn devices_lease(id: i64, o: usize, devices: u32) -> Holding {
        Holding {
            id,
            project: None,
            slot: None,
            devices,
            worktree: format!("wt{id}"),
            owner: owner(o),
            owner_label: format!("claude:{o}"),
        }
    }

    #[test]
    fn an_owner_at_its_device_cap_waits_naming_it_and_steps_aside_for_other_owners() {
        let phone = Some(need(Mode::Phone, 1, false));
        let leases = vec![devices_lease(1, 0, 1), devices_lease(2, 0, 1)];
        let b = board(
            leases,
            vec![device_entry(1, 0, phone), device_entry(2, 1, phone)],
            2,
        );
        assert_eq!(
            decide(&device_ask(1, phone), &b, 0),
            Decision::Wait(
                "owner claude:0 already holds 2 device(s); 1 more would pass its cap of 2 (maxDevicesPerOwner; waits for one of its own leases to end)"
                    .to_owned()
            )
        );
        assert_eq!(decide(&device_ask(2, phone), &b, 0), NEW);
    }

    #[test]
    fn a_top_up_within_what_the_owner_has_left_is_served() {
        let phone = Some(need(Mode::Phone, 1, false));
        let b = board(
            vec![devices_lease(1, 0, 1)],
            vec![device_entry(1, 0, phone)],
            2,
        );
        assert_eq!(decide(&device_ask(1, phone), &b, 0), NEW);
    }

    #[test]
    fn a_request_for_more_devices_than_the_cap_is_refused_whatever_is_free() {
        let three = Some(DeviceNeed {
            want: 3,
            ..need(Mode::Any, 3, false)
        });
        let b = board(vec![], vec![device_entry(1, 0, three)], 2);
        assert_eq!(
            decide(&device_ask(1, three), &b, 0),
            Decision::Refuse(
                "asks for 3 devices; one owner may hold at most 2 (maxDevicesPerOwner)".to_owned()
            )
        );
    }

    #[derive(Debug, Clone)]
    enum Op {
        Ask { owner: usize, worktree: usize },
        Release(usize),
        Squat(u32),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => (0..OWNERS, 0..WORKTREES).prop_map(|(owner, worktree)| Op::Ask { owner, worktree }),
            2 => (0..8usize).prop_map(Op::Release),
            1 => (0..4u32).prop_map(Op::Squat),
        ]
    }

    /// A board played forward: leases (owner, worktree, slot) and waiters (id, owner, worktree).
    struct Sim {
        max_slots: u32,
        cap: u32,
        leases: Vec<(usize, usize, u32)>,
        waiters: Vec<(i64, usize, usize)>,
        next_waiter: i64,
        listening: BTreeSet<u32>,
    }

    impl Sim {
        fn board(&self) -> Board {
            let mut b = board(
                self.leases
                    .iter()
                    .map(|&(o, wt, s)| holding(o, wt, s))
                    .collect(),
                self.waiters
                    .iter()
                    .map(|&(id, o, wt)| entry(id, o, wt, self.cap, self.max_slots))
                    .collect(),
                self.max_slots,
            );
            b.listening = self.listening.clone();
            b
        }

        fn existing(&self, o: usize, wt: usize) -> Option<Option<u32>> {
            self.leases
                .iter()
                .find(|&&(lo, lwt, _)| lo == o && lwt == wt)
                .map(|&(_, _, s)| Some(s))
        }

        /// Serves waiters, oldest first, until none can be served; checks every grant.
        fn settle(&mut self) -> Result<(), TestCaseError> {
            loop {
                let board = self.board();
                let served = self
                    .waiters
                    .iter()
                    .enumerate()
                    .find_map(|(at, &(id, o, wt))| {
                        let a = ask(id, self.max_slots, self.cap, self.existing(o, wt));
                        match decide(&a, &board, 0) {
                            Decision::Grant { slot, new } => Some((at, o, wt, slot, new)),
                            Decision::Wait(_) | Decision::Refuse(_) => None,
                        }
                    });
                let Some((at, o, wt, slot, new)) = served else {
                    return Ok(());
                };
                self.waiters.remove(at);
                if new {
                    let slot =
                        slot.ok_or_else(|| TestCaseError::fail("a new lease without a slot"))?;
                    prop_assert!(
                        !self.leases.iter().any(|&(_, _, s)| s == slot),
                        "slot {slot} granted twice"
                    );
                    prop_assert!(
                        !self.listening.contains(&port(slot)),
                        "a squatted slot granted"
                    );
                    self.leases.push((o, wt, slot));
                    let held = self.leases.iter().filter(|&&(lo, _, _)| lo == o).count();
                    prop_assert!(
                        held as u32 <= self.cap,
                        "owner {o} holds {held}, cap {}",
                        self.cap
                    );
                }
            }
        }

        /// Once settled, the head of the queue (its first waiter not at its cap) is only still
        /// waiting when no free slot is usable.
        fn head_waits_only_for_want_of_a_slot(&self) -> Result<(), TestCaseError> {
            let board = self.board();
            let held = held_by(&board.leases);
            let Some(head) = board
                .queue
                .iter()
                .find(|e| waiter_capped(e, &held).is_none())
            else {
                return Ok(());
            };
            let usable = (0..self.max_slots).find(|&s| {
                !self.leases.iter().any(|&(_, _, held)| held == s)
                    && !self.listening.contains(&port(s))
            });
            prop_assert_eq!(
                usable,
                None,
                "head {} waits with slot {:?} usable",
                &head.label,
                usable
            );
            Ok(())
        }
    }

    proptest! {
        #[test]
        fn no_slot_twice_no_owner_over_its_cap_and_the_head_always_progresses(
            max_slots in 1u32..=4,
            project_cap in proptest::option::of(0u32..=5),
            ops in proptest::collection::vec(op(), 0..60),
        ) {
            let mut sim = Sim {
                max_slots,
                cap: slot_cap(max_slots, project_cap, None),
                leases: Vec::new(),
                waiters: Vec::new(),
                next_waiter: 1,
                listening: BTreeSet::new(),
            };
            for op in ops {
                match op {
                    Op::Ask { owner, worktree } => {
                        sim.waiters.push((sim.next_waiter, owner, worktree));
                        sim.next_waiter += 1;
                    }
                    Op::Release(at) => {
                        if !sim.leases.is_empty() {
                            let at = at % sim.leases.len();
                            sim.leases.remove(at);
                        }
                    }
                    Op::Squat(slot) => {
                        let p = port(slot);
                        if !sim.listening.remove(&p) {
                            sim.listening.insert(p);
                        }
                    }
                }
                sim.settle()?;
                sim.head_waits_only_for_want_of_a_slot()?;
            }
        }
    }
}
