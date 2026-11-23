//! The FIFO queue acquire waits in. An owner at its per-owner cap steps aside: it can't be served
//! until one of its own leases ends, so it must not hold up the owners behind it.

use super::js_round;

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
    pub project: Option<String>,
    pub cap: Option<u32>,
    pub max_slots: u32,
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

/// Whose turn it is: none when it is waiter `me`'s (devices and slots are checked next), else
/// why it waits. `now` is seconds since the epoch.
pub fn queue_turn(
    queue: &[QueueEntry],
    me: i64,
    held: &dyn Fn(&str, &str) -> u32,
    now: i64,
) -> Option<String> {
    if let Some(why) = queue
        .iter()
        .find(|e| e.id == me)
        .and_then(|e| waiter_capped(e, held))
    {
        return Some(why);
    }
    let head = queue.iter().find(|e| waiter_capped(e, held).is_none())?;
    (head.id != me).then(|| {
        format!(
            "queued behind {} (waiting since {} min)",
            head.label,
            js_round((now - head.created) as f64 / 60.0)
        )
    })
}

/// A lease as the queue counts it: which slot it holds, and for whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    pub project: Option<String>,
    pub slot: Option<u32>,
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
            project: Some("peekabus".to_owned()),
            cap: Some(2),
            max_slots: 3,
        }
    }

    fn other(id: i64) -> QueueEntry {
        QueueEntry {
            id,
            label: "codex:200 @ main".to_owned(),
            created: 0,
            owner: "200:b".to_owned(),
            owner_label: "codex:200".to_owned(),
            project: Some("peekabus".to_owned()),
            cap: Some(2),
            max_slots: 3,
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

    #[test]
    fn an_owner_at_its_cap_waits_and_the_wait_names_the_cap() {
        assert_eq!(
            queue_turn(&[lead(1), other(2)], 1, &held(2), 0).as_deref(),
            Some(
                "owner claude:100 already holds 2 of 3 peekabus stack slots; cap 2 (waits for one of its own leases to end)"
            )
        );
    }

    #[test]
    fn another_owner_queued_behind_it_is_served_meanwhile() {
        assert_eq!(queue_turn(&[lead(1), other(2)], 2, &held(2), 0), None);
    }

    #[test]
    fn an_owner_under_its_cap_is_served_in_its_turn() {
        assert_eq!(queue_turn(&[lead(1), other(2)], 1, &held(1), 0), None);
        assert_eq!(
            queue_turn(&[lead(1), other(2)], 2, &held(1), 0).as_deref(),
            Some("queued behind claude:100 @ s5-t1 (waiting since 0 min)")
        );
    }

    #[test]
    fn once_one_of_its_own_leases_ends_the_capped_owner_is_first_again() {
        assert_eq!(queue_turn(&[lead(1), other(3)], 1, &held(1), 0), None);
        assert!(
            queue_turn(&[lead(1), other(3)], 3, &held(1), 0)
                .is_some_and(|why| why.starts_with("queued behind"))
        );
    }

    #[test]
    fn a_lease_that_asks_for_no_new_slot_or_another_projects_is_never_capped() {
        let top_up = QueueEntry {
            cap: None,
            ..lead(1)
        };
        assert_eq!(queue_turn(&[top_up, other(2)], 1, &held(2), 0), None);
        let elsewhere = QueueEntry {
            project: Some("fleet".to_owned()),
            ..lead(1)
        };
        assert_eq!(queue_turn(&[elsewhere], 1, &held(2), 0), None);
    }

    #[test]
    fn the_wait_behind_another_says_how_long_it_has_waited_in_rounded_minutes() {
        assert_eq!(
            queue_turn(&[other(1), lead(2)], 2, &held(0), 89).as_deref(),
            Some("queued behind codex:200 @ main (waiting since 1 min)")
        );
        assert_eq!(
            queue_turn(&[other(1), lead(2)], 2, &held(0), 29).as_deref(),
            Some("queued behind codex:200 @ main (waiting since 0 min)")
        );
    }

    #[test]
    fn counts_per_owner_against_the_cap_for_status() {
        let lease = |project: Option<&str>, slot: Option<u32>, owner: &str, label: &str| Holding {
            project: project.map(str::to_owned),
            slot,
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
