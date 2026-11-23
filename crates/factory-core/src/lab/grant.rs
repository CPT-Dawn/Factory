//! Whether one acquire request is served now: the queue, the per-owner cap and the slot layout
//! put together over one picture of the board.

use std::collections::BTreeSet;

use super::queue::{Holding, QueueEntry, held_by, queue_turn};
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
    /// The asker's project's slots by index, as far as they are known; whether each is held is
    /// read from `leases`.
    pub slots: Vec<SlotView>,
    /// TCP ports something listens on.
    pub listening: BTreeSet<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Served: the slot it holds, and whether that is a new lease.
    Grant { slot: Option<u32>, new: bool },
    /// Not yet, and why.
    Wait(String),
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
    // The asker's own entry is read live: a lease that exists by now asks for no new slot.
    let queue: Vec<QueueEntry> = board
        .queue
        .iter()
        .map(|e| {
            if e.id != ask.waiter {
                return e.clone();
            }
            QueueEntry {
                project: ask.project.as_ref().map(|p| p.name.clone()),
                cap: request_cap(ask.project.as_ref(), ask.existing.is_some()),
                max_slots: ask.project.as_ref().map_or(0, |p| p.max_slots),
                ..e.clone()
            }
        })
        .collect();
    let held = held_by(&board.leases);
    if let Some(why) = queue_turn(&queue, ask.waiter, &held, now) {
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
    let slots: Vec<SlotView> = (0..project.max_slots)
        .map(|slot| SlotView {
            held: taken.contains(&slot),
            ..board.slots.get(slot as usize).cloned().unwrap_or_default()
        })
        .collect();
    match pick_slot(&project.name, &slots, &board.listening, project.has_stack) {
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
            project: Some(PROJECT.to_owned()),
            cap: Some(cap),
            max_slots,
        }
    }

    fn holding(o: usize, slot: u32) -> Holding {
        Holding {
            project: Some(PROJECT.to_owned()),
            slot: Some(slot),
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
            }),
            existing,
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
        }
    }

    #[test]
    fn the_head_of_the_queue_takes_the_lowest_free_slot() {
        let b = board(vec![holding(0, 0)], vec![entry(1, 1, 1, 1, 2)], 2);
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
            vec![holding(0, 0)],
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
        let b = board(vec![holding(0, 0)], vec![entry(1, 0, 0, 1, 2)], 2);
        assert_eq!(
            decide(&ask(1, 2, 1, Some(Some(0))), &b, 0),
            Decision::Grant {
                slot: Some(0),
                new: false
            }
        );
    }

    #[test]
    fn every_slot_held_is_a_wait_that_says_so() {
        let b = board(
            vec![holding(0, 0), holding(1, 1)],
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
                self.leases.iter().map(|&(o, _, s)| holding(o, s)).collect(),
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
                            Decision::Wait(_) => None,
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
