//! The status screen's layout: what each section shows, what it cuts, and that the lab the
//! ticket names (three slots, three backends, five leads) fits one terminal screen.

use factory_core::status::{
    Backend, COLUMNS, Lab, Lead, Leads, Lease, Queued, QuotaState, ROWS, Reservation, Slot, Status,
    Subagent, Wait, screen,
};
use factory_core::timestamp::Timestamp;

/// 2026-11-23 04:13:00 UTC, in seconds.
fn now() -> i64 {
    Timestamp::parse("2026-11-23T04:13:00Z").map_or(0, Timestamp::unix_millis) / 1000
}

fn idle() -> Status {
    Status {
        now: now() * 1000,
        lab: Lab::default(),
        review: Vec::new(),
        waits: Vec::new(),
        leads: Leads {
            error: None,
            monitor: false,
            leads: Vec::new(),
        },
    }
}

fn lease(id: i64, slot: Option<u32>, worktree: &str, owner: &str, minutes_ago: i64) -> Lease {
    Lease {
        id,
        project: slot.map(|_| "demo".to_owned()),
        slot,
        worktree: format!("/work/{worktree}"),
        owner: owner.to_owned(),
        created: now() - minutes_ago * 60,
        touched: now(),
        devices: Vec::new(),
    }
}

fn slot(n: u32, lease: Option<i64>, last: Option<&str>) -> Slot {
    Slot {
        project: "demo".to_owned(),
        slot: n,
        lease,
        reservation: None,
        last_worktree: last.map(|wt| format!("/work/{wt}")),
        stack_ours: false,
        stopping: false,
        idle_since: None,
    }
}

fn backend(name: &str, quota: QuotaState, resets: Option<&str>, running: u32, of: u32) -> Backend {
    Backend {
        name: name.to_owned(),
        quota,
        resets: resets
            .and_then(Timestamp::parse)
            .map(Timestamp::unix_millis),
        running,
        concurrency: of,
        when_busy: "queue".to_owned(),
    }
}

fn lead(name: &str, status: &str, tokens: Option<u64>) -> Lead {
    Lead {
        name: name.to_owned(),
        state: status.split(':').next().unwrap_or_default().to_owned(),
        status: status.to_owned(),
        pane: "w1:p1".to_owned(),
        report: None,
        session: None,
        tokens,
        asked: false,
        compacting: false,
        subagents: Vec::new(),
    }
}

fn rows(status: &Status) -> Vec<String> {
    screen(status)
}

#[test]
fn an_idle_factory_has_a_dated_title_and_one_line_per_section() {
    assert_eq!(
        rows(&idle()),
        [
            "factory status 2026-11-23 04:13Z",
            "lab: idle",
            "review: no backends configured",
            "waits: none",
            "leads: none, monitor off",
        ]
    );
}

#[test]
fn the_lab_counts_leases_free_slots_and_the_queue_and_lists_each() {
    let mut status = idle();
    status.lab = Lab {
        leases: vec![
            lease(1, Some(0), "demo-a", "claude:123", 4),
            lease(2, None, "demo-b", "codex:77", 90),
        ],
        slots: vec![slot(0, Some(1), None), slot(1, None, Some("demo-c"))],
        queue: vec![Queued {
            id: 9,
            label: "sleep:77 @ demo-d".to_owned(),
            project: Some("demo".to_owned()),
            created: now() - 120,
            deadline_ms: None,
        }],
        ..Lab::default()
    };

    assert_eq!(
        &rows(&status)[1..6],
        [
            "lab: 2 leased, 1 free, 1 queued",
            "  lease 1 demo/0 demo-a claude:123 4m",
            "  lease 2 demo-b codex:77 1.5h",
            "  demo/1 free, last demo-c",
            "  queued sleep:77 @ demo-d 2m",
        ]
    );
}

#[test]
fn a_lease_names_the_devices_it_holds() {
    let mut status = idle();
    let mut held = lease(1, Some(0), "demo-a", "claude:123", 4);
    held.devices = vec!["emulator-5554".to_owned(), "R5CT".to_owned()];
    status.lab = Lab {
        leases: vec![held],
        slots: vec![slot(0, Some(1), None)],
        queue: Vec::new(),
        ..Lab::default()
    };

    assert_eq!(
        rows(&status)[2],
        "  lease 1 demo/0 demo-a claude:123 4m, 2 devices"
    );
}

#[test]
fn a_slot_whose_stack_is_being_stopped_says_so_instead_of_free() {
    let mut status = idle();
    let mut stopping = slot(1, None, Some("demo-c"));
    stopping.stopping = true;
    status.lab.slots = vec![stopping];

    assert_eq!(
        &rows(&status)[1..3],
        [
            "lab: 0 leased, 0 free, 0 queued",
            "  demo/1 stopping, last demo-c"
        ]
    );
}

#[test]
fn a_backend_shows_its_quota_its_reset_and_its_load() {
    let mut status = idle();
    status.review = vec![
        backend(
            "codex",
            QuotaState::Cooling,
            Some("2026-11-23T06:13:00Z"),
            0,
            2,
        ),
        backend("claude", QuotaState::Available, None, 1, 2),
        backend("agy", QuotaState::Unknown, None, 0, 1),
        backend(
            "old",
            QuotaState::Cooling,
            Some("2026-11-24T01:00:00Z"),
            0,
            1,
        ),
    ];

    assert_eq!(
        &rows(&status)[2..7],
        [
            "review quota",
            "  codex   cooling, resets 06:13Z (in 2h), 0/2 running",
            "  claude  available, 1/2 running",
            "  agy     unknown, 0/1 running",
            "  old     cooling, resets 11-24 01:00Z (in 20.8h), 0/1 running",
        ]
    );
}

#[test]
fn a_wait_names_who_waits_and_what_for() {
    let mut status = idle();
    status.waits = vec![
        Wait {
            pane: Some("w1:p1".to_owned()),
            lead: Some("lead-pay".to_owned()),
            kind: "ci".to_owned(),
            on: "o/r#12".to_owned(),
        },
        Wait {
            pane: Some("w9:p2".to_owned()),
            lead: None,
            kind: "lead".to_owned(),
            on: "lead-ship".to_owned(),
        },
        Wait {
            pane: None,
            lead: None,
            kind: "ci".to_owned(),
            on: "o/r#13".to_owned(),
        },
    ];

    assert_eq!(
        &rows(&status)[3..7],
        [
            "waits: 3",
            "  lead-pay -> ci o/r#12",
            "  pane w9:p2 -> lead-ship",
            "  outside herdr -> ci o/r#13",
        ]
    );
}

#[test]
fn a_lead_shows_its_state_context_and_subagents() {
    let mut status = idle();
    status.leads.monitor = true;
    let mut cart = lead("lead-cart", "working", Some(212_000));
    cart.subagents = vec![
        Subagent {
            agent: "agent-a1".to_owned(),
            description: None,
            tokens: Some(170_000),
            told: false,
        },
        Subagent {
            agent: "agent-a2".to_owned(),
            description: None,
            tokens: Some(40_999),
            told: true,
        },
        Subagent {
            agent: "agent-a3".to_owned(),
            description: None,
            tokens: None,
            told: false,
        },
    ];
    let mut pay = lead("lead-pay", "waiting: lead-ship", Some(90_000));
    pay.asked = true;
    let mut ship = lead("lead-ship", "blocked", None);
    ship.compacting = true;
    status.leads.leads = vec![cart, pay, ship, lead("lead-docs", "finished", None)];

    assert_eq!(
        &rows(&status)[4..9],
        [
            "leads: 4, monitor on",
            "  lead-cart  working             212k  3 subs, most 170k",
            "  lead-pay   waiting: lead-ship  90k asked",
            "  lead-ship  blocked             compacting",
            "  lead-docs  finished            -",
        ]
    );
}

#[test]
fn leads_that_herdr_cannot_tell_are_one_line() {
    let mut status = idle();
    status.leads.error = Some("herdr unreachable at /x/herdr.sock: no such file".to_owned());

    assert_eq!(
        rows(&status)[4],
        "leads: herdr unavailable, see factory lead list"
    );
}

#[test]
fn five_leads_three_slots_and_three_backends_fit_one_terminal_screen() {
    let mut status = idle();
    status.lab = Lab {
        leases: (0..3)
            .map(|n| {
                lease(
                    n + 1,
                    Some(u32::try_from(n).unwrap_or(0)),
                    "demo-a-long-worktree",
                    "claude:1234567",
                    125,
                )
            })
            .collect(),
        slots: (0..3)
            .map(|n| slot(n, Some(i64::from(n) + 1), None))
            .collect(),
        queue: (0..2)
            .map(|n| Queued {
                id: n,
                label: "claude:99 @ demo-queued".to_owned(),
                project: Some("demo".to_owned()),
                created: now() - 60,
                deadline_ms: None,
            })
            .collect(),
        ..Lab::default()
    };
    status.review = vec![
        backend(
            "codex",
            QuotaState::Cooling,
            Some("2026-11-23T06:13:00Z"),
            0,
            2,
        ),
        backend("claude", QuotaState::Available, None, 1, 2),
        backend("agy", QuotaState::Unknown, None, 0, 1),
    ];
    status.waits = (0..3)
        .map(|n| Wait {
            pane: Some(format!("w{n}:p1")),
            lead: Some(format!("lead-slug{n}")),
            kind: "ci".to_owned(),
            on: "CPT-Dawn/Factory#123".to_owned(),
        })
        .collect();
    status.leads.monitor = true;
    status.leads.leads = (0..5)
        .map(|n| {
            let mut lead = lead(
                &format!("lead-slug-number-{n}"),
                "waiting: lead-slug-number-9",
                Some(300_000),
            );
            lead.subagents = vec![Subagent {
                agent: "agent-a".to_owned(),
                description: None,
                tokens: Some(99_000),
                told: false,
            }];
            lead
        })
        .collect();

    let shown = rows(&status);

    assert!(
        shown.len() <= ROWS,
        "{} rows:\n{}",
        shown.len(),
        shown.join("\n")
    );
    assert!(
        shown.iter().all(|row| row.chars().count() <= COLUMNS),
        "{}",
        shown.join("\n")
    );
    for name in ["codex", "claude", "agy", "lead-slug-number-4"] {
        assert!(
            shown.iter().any(|row| row.contains(name)),
            "{name}: {}",
            shown.join("\n")
        );
    }
    assert_eq!(
        shown
            .iter()
            .filter(|row| row.starts_with("  lease "))
            .count(),
        3
    );
}

#[test]
fn a_section_with_more_rows_than_its_cap_says_how_many_it_left_out() {
    let mut status = idle();
    status.lab.queue = (0..7)
        .map(|n| Queued {
            id: n,
            label: format!("claude:{n} @ wt"),
            project: None,
            created: now() - 60,
            deadline_ms: None,
        })
        .collect();
    status.leads.leads = (0..9)
        .map(|n| lead(&format!("lead-n{n}"), "working", None))
        .collect();

    let shown = rows(&status);

    assert_eq!(
        shown
            .iter()
            .filter(|row| row.starts_with("  queued "))
            .count(),
        2,
        "{}",
        shown.join("\n")
    );
    assert!(
        shown.contains(&"  +5 more queued".to_owned()),
        "{}",
        shown.join("\n")
    );
    assert!(
        shown.contains(&"  +4 more leads".to_owned()),
        "{}",
        shown.join("\n")
    );
}

#[test]
fn a_row_wider_than_the_screen_ends_in_an_ellipsis() {
    let mut status = idle();
    status.lab.queue = vec![Queued {
        id: 1,
        label: "x".repeat(200),
        project: None,
        created: now(),
        deadline_ms: None,
    }];

    let row = &rows(&status)[2];

    assert_eq!(row.chars().count(), COLUMNS);
    assert!(row.ends_with('…'), "{row}");
}

#[test]
fn a_reservation_holds_its_slot_and_is_listed_with_its_lead() {
    let mut status = idle();
    let mut reserved = slot(1, None, Some("demo-c"));
    reserved.reservation = Some(7);
    status.lab = Lab {
        leases: vec![lease(1, Some(0), "demo-a", "claude:123", 4)],
        slots: vec![slot(0, Some(1), None), reserved, slot(2, None, None)],
        reservations: vec![Reservation {
            id: 7,
            project: "demo".to_owned(),
            slot: 1,
            worktree: "/work/demo-lead".to_owned(),
            owner: "claude:77".to_owned(),
            created: now() - 180,
        }],
        ..Lab::default()
    };

    assert_eq!(
        &rows(&status)[1..5],
        [
            "lab: 1 leased, 1 reserved, 1 free, 0 queued",
            "  lease 1 demo/0 demo-a claude:123 4m",
            "  reserved demo/1 demo-lead claude:77 3m",
            "  demo/2 free",
        ]
    );
}
