//! The FIFO queue acquire waits in. An owner at its per-owner cap steps aside: it can't be served
//! until one of its own leases ends, so it must not hold up the owners behind it. So does a waiter
//! short of devices, for the waiters that ask for none of the kind it waits for (#58): FIFO holds
//! only among waiters for the same devices.

use super::devices::Mode;
use super::js_round;

/// The devices a waiter asks for, as its last look found them: the waiters behind it read this
/// to see whether it stands in their way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceNeed {
    /// The kind of device it asks for.
    pub mode: Mode,
    /// The devices it asks for in all.
    pub want: u32,
    /// The devices it still has to take: `want` less those its lease holds already.
    pub lacks: u32,
    /// The devices, phones and emulators together, one owner may hold at once.
    pub cap: u32,
    /// No free device of its kind was there for it.
    pub short: bool,
}

/// A waiter in the queue. `cap` is none when it asks for no new stack slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: i64,
    /// `<owner label> @ <worktree name>`, as status and the wait reason show it.
    pub label: String,
    /// Seconds since the epoch.
    pub created: i64,
    /// The owner's key, `<pid>:<start>`.
    pub owner: String,
    pub owner_label: String,
    /// The worktree it asks from: with the owner, it finds the lease that serves it.
    pub worktree: String,
    /// `DEVLAB_LEASE`: the lease it asks for instead of its worktree and owner's.
    pub lease: Option<i64>,
    pub project: Option<String>,
    pub cap: Option<u32>,
    pub max_slots: u32,
    /// None when it asks for no device.
    pub devices: Option<DeviceNeed>,
}

/// Whether `e` is served by a lease that exists, so it asks for no new slot: the one it names,
/// else its worktree and owner's.
pub fn has_lease(e: &QueueEntry, leases: &[Holding]) -> bool {
    leases.iter().any(|l| match e.lease {
        Some(id) => l.id == id,
        None => l.owner == e.owner && l.worktree == e.worktree,
    })
}

/// The queue as it stands now: an entry a lease already serves is held to no cap. Every
/// decision reads it afresh, so a grant changes what the requests behind it see.
pub fn live_queue(queue: &[QueueEntry], leases: &[Holding]) -> Vec<QueueEntry> {
    queue
        .iter()
        .map(|e| QueueEntry {
            cap: if has_lease(e, leases) { None } else { e.cap },
            ..e.clone()
        })
        .collect()
}

/// Why a waiter's owner may not take another slot yet, given how many it holds; none when it
/// may.
pub fn cap_wait(e: &QueueEntry, held: u32) -> Option<String> {
    let (Some(cap), Some(project)) = (e.cap, &e.project) else {
        return None;
    };
    (held >= cap).then(|| {
        format!(
            "owner {} already holds {held} of {} {project} stack slots; cap {cap} (waits for one of its own leases to end)",
            e.owner_label, e.max_slots
        )
    })
}

/// [`cap_wait`] with the owner's count looked up: `held(owner, project)`.
pub fn waiter_capped(e: &QueueEntry, held: &dyn Fn(&str, &str) -> u32) -> Option<String> {
    let project = e.project.as_deref()?;
    cap_wait(e, held(&e.owner, project))
}

/// Why a waiter's owner may not take the devices it lacks yet, given how many it holds; none
/// when it may.
pub fn device_cap_wait(e: &QueueEntry, held: u32) -> Option<String> {
    let d = e.devices?;
    over_device_cap(&d, held).then(|| {
        format!(
            "owner {} already holds {held} device(s); {} more would pass its cap of {} (maxDevicesPerOwner; waits for one of its own leases to end)",
            e.owner_label, d.lacks, d.cap
        )
    })
}

/// Whether taking the devices `d` lacks would put an owner holding `held` over its cap.
pub fn over_device_cap(d: &DeviceNeed, held: u32) -> bool {
    d.lacks > 0 && held.saturating_add(d.lacks) > d.cap
}

/// Whether a request for `mine` (none: no device) competes with a waiter short of `theirs`:
/// a phone and an emulator are not the same device, but `any` takes either.
pub fn competes(mine: Option<Mode>, theirs: Mode) -> bool {
    mine.is_some_and(|mine| mine == theirs || mine == Mode::Any || theirs == Mode::Any)
}

/// Whether waiter `e`'s owner is at a cap, so that only one of its own leases ending serves it.
fn capped(e: &QueueEntry, counts: &Counts<'_>) -> bool {
    waiter_capped(e, counts.slots).is_some()
        || device_cap_wait(e, (counts.devices)(&e.owner)).is_some()
}

/// What each owner holds: `slots(owner, project)` stack slots, `devices(owner)` devices.
pub struct Counts<'a> {
    pub slots: &'a dyn Fn(&str, &str) -> u32,
    pub devices: &'a dyn Fn(&str) -> u32,
}

/// Whose turn it is: none when it is waiter `me`'s (devices and slots are checked next), else
/// why it waits. `now` is seconds since the epoch.
pub fn queue_turn(queue: &[QueueEntry], me: i64, counts: &Counts<'_>, now: i64) -> Option<String> {
    let mine = queue.iter().find(|e| e.id == me);
    if let Some(e) = mine
        && let Some(why) = waiter_capped(e, counts.slots)
            .or_else(|| device_cap_wait(e, (counts.devices)(&e.owner)))
    {
        return Some(why);
    }
    let wants = mine.and_then(|e| e.devices).map(|d| d.mode);
    // The kinds of device the waiters stepping aside so far wait for: a waiter is short of
    // devices, or queued behind one that is and that it competes with. Either waits on devices,
    // and lets a request that doesn't compete for its own go ahead of it.
    let mut held: Vec<Mode> = Vec::new();
    let head = queue.iter().find(|e| {
        if e.id == me {
            return true;
        }
        if capped(e, counts) {
            return false;
        }
        let on_devices = e
            .devices
            .filter(|d| d.short || held.iter().any(|kind| competes(Some(d.mode), *kind)));
        match on_devices {
            Some(d) if !competes(wants, d.mode) => {
                held.push(d.mode);
                false
            }
            _ => true,
        }
    })?;
    (head.id != me).then(|| {
        format!(
            "queued behind {} (waiting since {} min)",
            head.label,
            js_round((now - head.created) as f64 / 60.0)
        )
    })
}

/// A lease as the queue counts it: which slot and how many devices it holds, and for whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    pub id: i64,
    pub project: Option<String>,
    pub slot: Option<u32>,
    /// Phones and emulators.
    pub devices: u32,
    pub worktree: String,
    /// The owner's key, `<pid>:<start>`.
    pub owner: String,
    pub owner_label: String,
}

/// Stack slots one owner holds in one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerCount {
    pub project: String,
    pub owner: String,
    pub owner_label: String,
    pub held: u32,
}

/// Stack slots held per (project, owner), in the order the leases first show them.
pub fn owner_slot_counts(leases: &[Holding]) -> Vec<OwnerCount> {
    let mut out: Vec<OwnerCount> = Vec::new();
    for lease in leases {
        let (Some(project), Some(_)) = (&lease.project, lease.slot) else {
            continue;
        };
        match out
            .iter_mut()
            .find(|c| &c.project == project && c.owner == lease.owner)
        {
            Some(count) => count.held += 1,
            None => out.push(OwnerCount {
                project: project.clone(),
                owner: lease.owner.clone(),
                owner_label: lease.owner_label.clone(),
                held: 1,
            }),
        }
    }
    out
}

/// The counts as a lookup, `held(owner, project)`.
pub fn held_by(leases: &[Holding]) -> impl Fn(&str, &str) -> u32 + use<> {
    let counts = owner_slot_counts(leases);
    move |owner, project| {
        counts
            .iter()
            .find(|c| c.owner == owner && c.project == project)
            .map_or(0, |c| c.held)
    }
}

/// Devices held per owner, phones and emulators together, as a lookup `held(owner)`.
pub fn devices_held_by(leases: &[Holding]) -> impl Fn(&str) -> u32 + use<> {
    let counts: Vec<(String, u32)> = leases
        .iter()
        .map(|l| (l.owner.clone(), l.devices))
        .collect();
    move |owner| {
        counts
            .iter()
            .filter(|(o, _)| o == owner)
            .map(|(_, n)| *n)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // claude:100 holds 2 of peekabus's 3 slots (cap 2); codex:200 holds none.
    fn lead(id: i64) -> QueueEntry {
        QueueEntry {
            id,
            label: format!("claude:100 @ s5-t{id}"),
            created: 0,
            owner: "100:a".to_owned(),
            owner_label: "claude:100".to_owned(),
            worktree: format!("s5-t{id}"),
            lease: None,
            project: Some("peekabus".to_owned()),
            cap: Some(2),
            max_slots: 3,
            devices: None,
        }
    }

    fn other(id: i64) -> QueueEntry {
        QueueEntry {
            id,
            label: "codex:200 @ main".to_owned(),
            created: 0,
            owner: "200:b".to_owned(),
            owner_label: "codex:200".to_owned(),
            worktree: "main".to_owned(),
            lease: None,
            project: Some("peekabus".to_owned()),
            cap: Some(2),
            max_slots: 3,
            devices: None,
        }
    }

    fn held(lead_holds: u32) -> impl Fn(&str, &str) -> u32 {
        move |owner, project| {
            if owner == "100:a" && project == "peekabus" {
                lead_holds
            } else {
                0
            }
        }
    }

    fn no_devices(_: &str) -> u32 {
        0
    }

    fn counts(slots: &dyn Fn(&str, &str) -> u32) -> Counts<'_> {
        Counts {
            slots,
            devices: &no_devices,
        }
    }

    #[test]
    fn an_owner_at_its_cap_waits_and_the_wait_names_the_cap() {
        assert_eq!(
            queue_turn(&[lead(1), other(2)], 1, &counts(&held(2)), 0).as_deref(),
            Some(
                "owner claude:100 already holds 2 of 3 peekabus stack slots; cap 2 (waits for one of its own leases to end)"
            )
        );
    }

    #[test]
    fn another_owner_queued_behind_it_is_served_meanwhile() {
        assert_eq!(
            queue_turn(&[lead(1), other(2)], 2, &counts(&held(2)), 0),
            None
        );
    }

    #[test]
    fn an_owner_under_its_cap_is_served_in_its_turn() {
        assert_eq!(
            queue_turn(&[lead(1), other(2)], 1, &counts(&held(1)), 0),
            None
        );
        assert_eq!(
            queue_turn(&[lead(1), other(2)], 2, &counts(&held(1)), 0).as_deref(),
            Some("queued behind claude:100 @ s5-t1 (waiting since 0 min)")
        );
    }

    #[test]
    fn once_one_of_its_own_leases_ends_the_capped_owner_is_first_again() {
        assert_eq!(
            queue_turn(&[lead(1), other(3)], 1, &counts(&held(1)), 0),
            None
        );
        assert!(
            queue_turn(&[lead(1), other(3)], 3, &counts(&held(1)), 0)
                .is_some_and(|why| why.starts_with("queued behind"))
        );
    }

    #[test]
    fn a_lease_that_asks_for_no_new_slot_or_another_projects_is_never_capped() {
        let top_up = QueueEntry {
            cap: None,
            ..lead(1)
        };
        assert_eq!(
            queue_turn(&[top_up, other(2)], 1, &counts(&held(2)), 0),
            None
        );
        let elsewhere = QueueEntry {
            project: Some("fleet".to_owned()),
            ..lead(1)
        };
        assert_eq!(queue_turn(&[elsewhere], 1, &counts(&held(2)), 0), None);
    }

    #[test]
    fn the_wait_behind_another_says_how_long_it_has_waited_in_rounded_minutes() {
        assert_eq!(
            queue_turn(&[other(1), lead(2)], 2, &counts(&held(0)), 89).as_deref(),
            Some("queued behind codex:200 @ main (waiting since 1 min)")
        );
        assert_eq!(
            queue_turn(&[other(1), lead(2)], 2, &counts(&held(0)), 29).as_deref(),
            Some("queued behind codex:200 @ main (waiting since 0 min)")
        );
    }

    #[test]
    fn counts_per_owner_against_the_cap_for_status() {
        let lease = |project: Option<&str>, slot: Option<u32>, owner: &str, label: &str| Holding {
            id: 0,
            project: project.map(str::to_owned),
            slot,
            devices: 0,
            worktree: "wt".to_owned(),
            owner: owner.to_owned(),
            owner_label: label.to_owned(),
        };
        let leases = [
            lease(Some("peekabus"), Some(0), "100:a", "claude:100"),
            lease(Some("peekabus"), Some(1), "100:a", "claude:100"),
            lease(Some("peekabus"), Some(2), "200:b", "codex:200"),
            lease(Some("peekabus"), None, "300:c", "claude:300"),
            lease(None, None, "300:c", "claude:300"),
        ];
        let count = |owner: &str, label: &str, held| OwnerCount {
            project: "peekabus".to_owned(),
            owner: owner.to_owned(),
            owner_label: label.to_owned(),
            held,
        };
        assert_eq!(
            owner_slot_counts(&leases),
            vec![
                count("100:a", "claude:100", 2),
                count("200:b", "codex:200", 1)
            ]
        );
        let held = held_by(&leases);
        assert_eq!(held("100:a", "peekabus"), 2);
        assert_eq!(held("300:c", "peekabus"), 0);
    }
}
