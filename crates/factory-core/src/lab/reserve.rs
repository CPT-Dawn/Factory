//! Reservations (#17): a lead sets slots aside for the implementers it dispatches, so they never
//! queue inside their own context.
//!
//! A reservation holds one slot for its owner, counts against the owner's per-owner cap like a
//! lease, and is handed to one child through the environment; the child's acquire turns it into
//! its own lease. [`plan` ](plan) decides what a request is given; the rest reads a reserve
//! command's options and words its answers.

use super::args::wait_arg;
use super::grant::{Ask, Board, Decision, ProjectAsk, decide};
use super::queue::{Holding, held_by};

/// The slots a typical lease runs for when no lease of the project has ended yet.
pub const DEFAULT_LEASE_SECS: i64 = 30 * 60;
/// No ETA is shorter than this: a slot due now may still be a minute from the next grant.
pub const MIN_ETA_SECS: i64 = 60;
/// How long a reservation lasts after it was made or last used (#59).
pub const UNUSED_SECS: i64 = 2 * 3600;

/// How long a reservation no lease holds stays set aside: one left idle goes back to the pool
/// so an owner that waits gets its slot (the 2026-11-25 retro: a lead kept two idle for 1.2 h).
pub const IDLE_SECS: i64 = 20 * 60;

/// When a reservation made at `created` and last used at `used` (when the lease made from it
/// ended) was last in use: the later of the two. A reservation is only a row while no lease
/// holds its slot, so it has been idle since then.
fn idle_since(created: i64, used: Option<i64>) -> i64 {
    used.map_or(created, |used| used.max(created))
}

/// A reservation made at `created` and last used at `used` has expired at `now`: nobody used
/// it for [`UNUSED_SECS`]. Seconds since the epoch.
pub fn expired(created: i64, used: Option<i64>, now: i64) -> bool {
    now - idle_since(created, used) >= UNUSED_SECS
}

/// When a reservation made at `created` and last used at `used` lapses for idling
/// [`IDLE_SECS`], unless a lease uses it first. Seconds since the epoch.
pub fn lapses_at(created: i64, used: Option<i64>) -> i64 {
    idle_since(created, used) + IDLE_SECS
}

/// One reserve request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserve {
    /// Its entry in the queue.
    pub waiter: i64,
    pub project: ProjectAsk,
    /// The owner's key, `<pid>:<start>`.
    pub owner: String,
    pub owner_label: String,
    pub want: u32,
}

/// An owner with no room left under its cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverCap {
    pub owner_label: String,
    pub project: String,
    pub held: u32,
    pub cap: u32,
    pub max_slots: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The slots set aside, lowest first.
    Granted(Vec<u32>),
    /// No slot yet, and why.
    Wait(String),
    OverCap(OverCap),
}

/// What `ask` is given now: its slots, up to what is left under its cap, or why it waits, or that
/// no room is left. `board.leases` holds the reservations beside the leases, and `board.queue`
/// the request's own entry. `now` is seconds since the epoch.
pub fn plan(ask: &Reserve, board: &Board, now: i64) -> Plan {
    let held = held_by(&board.leases)(&ask.owner, &ask.project.name);
    if held >= ask.project.cap {
        return Plan::OverCap(OverCap {
            owner_label: ask.owner_label.clone(),
            project: ask.project.name.clone(),
            held,
            cap: ask.project.cap,
            max_slots: ask.project.max_slots,
        });
    }
    let room = ask.want.min(ask.project.cap - held);
    let mut board = board.clone();
    let mut slots = Vec::new();
    for _ in 0..room {
        let one = Ask {
            waiter: ask.waiter,
            project: Some(ask.project.clone()),
            existing: None,
            devices: None,
            devices_wait: None,
        };
        match decide(&one, &board, now) {
            Decision::Grant {
                slot: Some(slot), ..
            } => {
                board.leases.push(Holding {
                    id: 0,
                    worktree: String::new(),
                    project: Some(ask.project.name.clone()),
                    slot: Some(slot),
                    devices: 0,
                    owner: ask.owner.clone(),
                    owner_label: ask.owner_label.clone(),
                });
                slots.push(slot);
            }
            Decision::Grant { slot: None, .. } => break,
            Decision::Wait(why) if slots.is_empty() => return Plan::Wait(why),
            Decision::Wait(_) | Decision::Refuse(_) => break,
        }
    }
    Plan::Granted(slots)
}

/// Seconds until one of an owner's holdings is likely to end: the soonest, over holdings of
/// the ages `ages`, to reach the typical (median) length of the `past` leases, in seconds. A
/// project with no past lease takes [`DEFAULT_LEASE_SECS`]; none answers under [`MIN_ETA_SECS`].
pub fn eta_secs(past: &[i64], ages: &[i64]) -> i64 {
    let mut sorted = past.to_vec();
    sorted.sort_unstable();
    let typical = match sorted.len() {
        0 => DEFAULT_LEASE_SECS,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2,
    };
    ages.iter()
        .map(|age| typical - age)
        .min()
        .unwrap_or(typical)
        .max(MIN_ETA_SECS)
}

/// The one line that refuses an owner with no room left, with its ETA in seconds.
pub fn over_cap_line(over: &OverCap, eta: i64) -> String {
    format!(
        "owner {} already holds {} of {} {} stack slots (cap {}); a slot of its own frees in ~{} min: release a lease or reservation, or retry then",
        over.owner_label,
        over.held,
        over.max_slots,
        over.project,
        over.cap,
        (eta + 30) / 60
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReserveOpts {
    pub slots: u32,
    pub wait_sec: Option<f64>,
    pub json: bool,
    pub release: bool,
}

/// The options of `reserve`: `--slots N` (default 1), `--wait SEC`, `--json`, `--release`.
pub fn parse_reserve(args: &[String]) -> Result<ReserveOpts, String> {
    let mut opts = ReserveOpts {
        slots: 1,
        wait_sec: None,
        json: false,
        release: false,
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--slots" => {
                opts.slots = rest
                    .next()
                    .and_then(|n| n.trim().parse::<u32>().ok())
                    .filter(|n| *n >= 1)
                    .ok_or("--slots takes a whole number of 1 or more")?;
            }
            "--wait" => opts.wait_sec = Some(wait_arg(rest.next())?),
            "--json" => opts.json = true,
            "--release" => opts.release = true,
            other => {
                return Err(format!(
                    "unknown option {other}; factory lab reserve --help"
                ));
            }
        }
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::queue::QueueEntry;
    use crate::lab::slots::SlotView;

    const OWNER: &str = "100:a";

    fn holding(owner: &str, slot: u32) -> Holding {
        Holding {
            id: 0,
            worktree: String::new(),
            project: Some("demo".to_owned()),
            slot: Some(slot),
            devices: 0,
            owner: owner.to_owned(),
            owner_label: format!("claude:{}", owner.split(':').next().unwrap_or("")),
        }
    }

    fn project(cap: u32, max_slots: u32) -> ProjectAsk {
        ProjectAsk {
            name: "demo".to_owned(),
            max_slots,
            cap,
            has_stack: false,
            stack_need_mb: None,
        }
    }

    fn ask(want: u32, cap: u32, max_slots: u32) -> Reserve {
        Reserve {
            waiter: 1,
            project: project(cap, max_slots),
            owner: OWNER.to_owned(),
            owner_label: "claude:100".to_owned(),
            want,
        }
    }

    fn board(leases: Vec<Holding>, max_slots: u32) -> Board {
        Board {
            leases,
            queue: vec![QueueEntry {
                id: 1,
                label: "claude:100 @ lead".to_owned(),
                worktree: "lead".to_owned(),
                lease: None,
                created: 0,
                owner: OWNER.to_owned(),
                owner_label: "claude:100".to_owned(),
                project: Some("demo".to_owned()),
                cap: Some(2),
                max_slots,
                devices: None,
            }],
            slots: (0..max_slots)
                .map(|s| SlotView {
                    own_ports: vec![27000 + s * 100],
                    ..SlotView::default()
                })
                .collect(),
            listening: std::collections::BTreeSet::new(),
            mem_available_mb: 0,
        }
    }

    #[test]
    fn a_lead_gets_the_slots_it_asks_for_up_to_its_cap() {
        assert_eq!(
            plan(&ask(2, 2, 4), &board(vec![], 4), 0),
            Plan::Granted(vec![0, 1])
        );
    }

    #[test]
    fn a_request_past_the_cap_is_granted_only_up_to_it() {
        assert_eq!(
            plan(&ask(3, 2, 4), &board(vec![], 4), 0),
            Plan::Granted(vec![0, 1])
        );
        assert_eq!(
            plan(&ask(3, 2, 4), &board(vec![holding(OWNER, 0)], 4), 0),
            Plan::Granted(vec![1])
        );
    }

    #[test]
    fn slots_the_owner_already_holds_or_reserved_count_against_the_cap() {
        let b = board(vec![holding(OWNER, 0), holding(OWNER, 1)], 4);
        assert_eq!(
            plan(&ask(1, 2, 4), &b, 0),
            Plan::OverCap(OverCap {
                owner_label: "claude:100".to_owned(),
                project: "demo".to_owned(),
                held: 2,
                cap: 2,
                max_slots: 4,
            })
        );
    }

    #[test]
    fn other_owners_slots_do_not_count_against_the_cap() {
        let b = board(vec![holding("200:b", 0)], 4);
        assert_eq!(plan(&ask(2, 2, 4), &b, 0), Plan::Granted(vec![1, 2]));
    }

    #[test]
    fn a_request_with_no_slot_free_waits_and_says_why() {
        let b = board(vec![holding("200:b", 0), holding("300:c", 1)], 2);
        assert_eq!(
            plan(&ask(1, 2, 2), &b, 0),
            Plan::Wait("all 2 stack slots of demo are held".to_owned())
        );
    }

    #[test]
    fn what_is_free_is_granted_when_less_than_asked_is() {
        let b = board(vec![holding("200:b", 0)], 2);
        assert_eq!(plan(&ask(2, 2, 2), &b, 0), Plan::Granted(vec![1]));
    }

    #[test]
    fn a_request_behind_another_in_the_queue_waits_its_turn() {
        let mut b = board(vec![], 2);
        b.queue.insert(
            0,
            QueueEntry {
                id: 0,
                label: "claude:200 @ other".to_owned(),
                worktree: "other".to_owned(),
                lease: None,
                created: 0,
                owner: "200:b".to_owned(),
                owner_label: "claude:200".to_owned(),
                project: Some("demo".to_owned()),
                cap: Some(2),
                max_slots: 2,
                devices: None,
            },
        );
        assert_eq!(
            plan(&ask(1, 2, 2), &b, 0),
            Plan::Wait("queued behind claude:200 @ other (waiting since 0 min)".to_owned())
        );
    }

    #[test]
    fn the_eta_is_the_soonest_a_holding_reaches_a_typical_lease_length() {
        // Past leases ran 10, 20 and 40 minutes: the typical one is 20.
        let past = [600, 2400, 1200];
        assert_eq!(eta_secs(&past, &[300, 900]), 300);
        assert_eq!(eta_secs(&past, &[0]), 1200);
    }

    #[test]
    fn the_eta_is_never_under_a_minute_and_has_a_default() {
        assert_eq!(eta_secs(&[600], &[5000]), MIN_ETA_SECS);
        assert_eq!(eta_secs(&[], &[0]), DEFAULT_LEASE_SECS);
        assert_eq!(eta_secs(&[600, 1200], &[0]), 900);
    }

    #[test]
    fn the_over_cap_line_names_the_cap_and_an_eta() {
        let over = OverCap {
            owner_label: "claude:100".to_owned(),
            project: "demo".to_owned(),
            held: 2,
            cap: 2,
            max_slots: 4,
        };
        assert_eq!(
            over_cap_line(&over, 1500),
            "owner claude:100 already holds 2 of 4 demo stack slots (cap 2); a slot of its own frees in ~25 min: release a lease or reservation, or retry then"
        );
    }

    fn args(a: &[&str]) -> Result<ReserveOpts, String> {
        let a: Vec<String> = a.iter().map(|s| (*s).to_owned()).collect();
        parse_reserve(&a)
    }

    #[test]
    fn reserve_reads_its_slots_wait_and_json() {
        assert_eq!(
            args(&["--slots", "3", "--wait", "5", "--json"]),
            Ok(ReserveOpts {
                slots: 3,
                wait_sec: Some(5.0),
                json: true,
                release: false
            })
        );
        assert_eq!(
            args(&[]),
            Ok(ReserveOpts {
                slots: 1,
                wait_sec: None,
                json: false,
                release: false
            })
        );
        assert!(args(&["--release"]).unwrap().release);
        assert_eq!(args(&["--wait", "9m"]).unwrap().wait_sec, Some(540.0));
    }

    #[test]
    fn reserve_refuses_what_it_cannot_read() {
        for bad in ["0", "-1", "two", "1.5", ""] {
            assert_eq!(
                args(&["--slots", bad]),
                Err("--slots takes a whole number of 1 or more".to_owned()),
                "{bad}"
            );
        }
        assert_eq!(
            args(&["--slots"]),
            Err("--slots takes a whole number of 1 or more".to_owned())
        );
        assert_eq!(
            args(&["--wait", "soon"]),
            Err(crate::lab::args::WAIT_REFUSAL.to_owned())
        );
        assert_eq!(
            args(&["--lots", "2"]),
            Err("unknown option --lots; factory lab reserve --help".to_owned())
        );
    }

    #[test]
    fn an_idle_reservation_lapses_20_minutes_after_it_was_made_or_last_used() {
        const NOW: i64 = 1_800_000_000;
        assert_eq!(lapses_at(NOW, None), NOW + 20 * 60);
        assert_eq!(lapses_at(NOW - 3600, Some(NOW)), NOW + 20 * 60);
        // A use stamped before it was made (a clock step) counts from when it was made.
        assert_eq!(lapses_at(NOW, Some(NOW - 3600)), NOW + 20 * 60);
    }

    #[test]
    fn the_gc_frees_a_reservation_nobody_used_for_two_hours() {
        const NOW: i64 = 1_800_000_000;
        const H: i64 = 3600;
        // (made, last used, expired): last used is when a run gave its slot back.
        let cases = [
            (NOW, None, false),
            (NOW - H, None, false),
            (NOW - 2 * H + 1, None, false),
            (NOW - 2 * H, None, true),
            (NOW - 3 * H, None, true),
            (NOW - 3 * H, Some(NOW - H), false),
            (NOW - 3 * H, Some(NOW - 3 * H), true),
            (NOW - 5 * H, Some(NOW - 3 * H), true),
            // A use stamped before it was made (a clock step) counts from when it was made.
            (NOW - H, Some(NOW - 3 * H), false),
        ];
        for (made, used, gone) in cases {
            assert_eq!(expired(made, used, NOW), gone, "{made} {used:?}");
        }
    }
}
