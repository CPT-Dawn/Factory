//! The status screen's layout: what each section shows, what it cuts, and that the lab the
//! ticket names (three slots, three backends, five leads) fits one terminal screen.

use factory_core::fleet::Ci;
use factory_core::status::{
    Agy, Backend, COLUMNS, Claude, ClaudeState, Codex, Lab, Lead, Leads, Lease, Queued, QuotaState,
    ROWS, Reservation, Slot, Status, Subagent, Wait, screen,
};
use factory_core::timestamp::Timestamp;
use factory_core::usage::{GroupWindow, QuotaGroup, UsageReading, WindowKind, WindowState};

/// 2026-11-23 04:13:00 UTC, in seconds.
fn now() -> i64 {
    Timestamp::parse("2026-11-23T04:13:00Z").map_or(0, Timestamp::unix_millis) / 1000
}

fn idle() -> Status {
    Status {
        now: now() * 1000,
        lab: Lab::default(),
        review: Vec::new(),
        claude: Claude::default(),
        codex: Codex::default(),
        agy: Agy::default(),
        waits: Vec::new(),
        leads: Leads {
            error: None,
            monitor: false,
            leads: Vec::new(),
        },
        ci: Ci::default(),
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
        reason: None,
        running,
        tiers: Vec::new(),
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
            "claude: no plan window seen",
            "codex: no plan window seen",
            "agy: no reading yet",
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
fn a_backend_held_by_a_plan_says_why_under_its_row() {
    let mut status = idle();
    let mut held = backend(
        "codex",
        QuotaState::Cooling,
        Some("2026-11-23T06:13:00Z"),
        0,
        2,
    );
    held.reason = Some("held at weekly 92%".to_owned());
    status.review = vec![held];

    assert_eq!(
        &rows(&status)[2..5],
        [
            "review quota",
            "  codex  cooling, resets 06:13Z (in 2h), 0/2 running",
            "    held at weekly 92%"
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
fn a_backend_names_the_tiers_of_the_reviews_running_on_it() {
    let mut status = idle();
    let mut busy = backend("codex", QuotaState::Available, None, 2, 2);
    busy.tiers = vec!["hard".to_owned(), "spec".to_owned()];
    let mut one = backend("agy", QuotaState::Available, None, 1, 2);
    one.tiers = vec!["spec".to_owned()];
    status.review = vec![
        busy,
        one,
        backend("claude", QuotaState::Available, None, 0, 2),
    ];

    assert_eq!(
        &rows(&status)[2..6],
        [
            "review quota",
            "  codex   available, 2/2 running (hard, spec)",
            "  agy     available, 1/2 running (spec)",
            "  claude  available, 0/2 running",
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
        &rows(&status)[6..10],
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
        &rows(&status)[7..12],
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
        rows(&status)[7],
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
    status.agy = Agy {
        groups: vec![
            gemini(
                used(30, "2026-11-23T07:00:00Z"),
                used(7, "2026-11-30T02:00:00Z"),
            ),
            QuotaGroup {
                name: "Claude and GPT models".to_owned(),
                ..gemini(
                    used(24, "2026-11-23T06:30:00Z"),
                    used(63, "2026-11-30T01:00:00Z"),
                )
            },
        ],
        error: None,
        seen: Some(now() * 1000),
        guard: None,
    };
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

fn reading(window: WindowKind, used: u32, resets: &str, seen: &str) -> UsageReading {
    let at = |t: &str| Timestamp::parse(t).map_or(0, Timestamp::unix_millis);
    UsageReading {
        window,
        used_percent: used,
        resets: at(resets),
        seen: at(seen),
        current: at(resets) > now() * 1000,
    }
}

#[test]
fn the_claude_row_shows_both_windows_or_what_holds() {
    let mut status = idle();
    let at = |t: &str| Timestamp::parse(t).map(Timestamp::unix_millis);
    let five = reading(
        WindowKind::FiveHour,
        41,
        "2026-11-23T07:00:00Z",
        "2026-11-23T04:10:00Z",
    );
    let weekly = |used| {
        reading(
            WindowKind::Weekly,
            used,
            "2026-11-30T02:00:00Z",
            "2026-11-23T04:10:00Z",
        )
    };
    status.claude = Claude {
        verdict: ClaudeState::Go,
        used_percent: Some(41),
        resets: at("2026-11-23T07:00:00Z"),
        until: None,
        hold: None,
        windows: vec![five.clone(), weekly(12)],
    };
    assert_eq!(
        rows(&status)[3],
        "claude: 5h 41% resets 07:00Z, weekly 12% resets 11-30 02:00Z"
    );

    status.claude = Claude {
        verdict: ClaudeState::Hold,
        until: at("2026-11-30T02:00:00Z"),
        hold: Some("weekly 91%".to_owned()),
        windows: vec![five.clone(), weekly(91)],
        ..status.claude.clone()
    };
    assert_eq!(
        rows(&status)[3],
        "claude: hold: weekly 91% until 11-30 02:00Z; 5h 41%, weekly 91%"
    );

    status.claude = Claude {
        verdict: ClaudeState::Hold,
        used_percent: None,
        resets: None,
        until: at("2026-11-24T01:30:00Z"),
        hold: Some("limit hit".to_owned()),
        windows: Vec::new(),
    };
    assert_eq!(
        rows(&status)[3],
        "claude: hold: limit hit until 11-24 01:30Z"
    );
}

#[test]
fn a_window_past_its_reset_shows_as_unknown_with_its_age() {
    let mut status = idle();
    status.claude = Claude {
        verdict: ClaudeState::Go,
        windows: vec![
            reading(
                WindowKind::FiveHour,
                80,
                "2026-11-23T03:00:00Z",
                "2026-11-23T01:13:00Z",
            ),
            reading(
                WindowKind::Weekly,
                12,
                "2026-11-30T02:00:00Z",
                "2026-11-23T04:10:00Z",
            ),
        ],
        ..Claude::default()
    };
    assert_eq!(
        rows(&status)[3],
        "claude: 5h unknown (seen 3h ago), weekly 12% resets 11-30 02:00Z"
    );
}

fn at(text: &str) -> i64 {
    Timestamp::parse(text).map_or(0, Timestamp::unix_millis) / 1000
}

fn codex_window(window: WindowKind, used: u32, resets: &str, current: bool) -> UsageReading {
    UsageReading {
        window,
        used_percent: used,
        resets: at(resets) * 1000,
        seen: now() * 1000 - 180_000,
        current,
    }
}

fn gemini(five: WindowState, weekly: WindowState) -> QuotaGroup {
    QuotaGroup {
        name: "Gemini Models".to_owned(),
        windows: vec![
            GroupWindow {
                window: WindowKind::FiveHour,
                state: five,
            },
            GroupWindow {
                window: WindowKind::Weekly,
                state: weekly,
            },
        ],
    }
}

fn used(percent: u32, resets: &str) -> WindowState {
    WindowState::Used {
        used_percent: percent,
        resets: Timestamp::parse(resets).map_or(0, Timestamp::unix_millis),
    }
}

#[test]
fn the_codex_row_shows_each_window_and_when_it_resets() {
    let mut status = idle();
    status.codex = Codex {
        verdict: ClaudeState::Go,
        windows: vec![
            codex_window(WindowKind::FiveHour, 18, "2026-11-23T07:00:00Z", true),
            codex_window(WindowKind::Weekly, 62, "2026-11-30T02:00:00Z", true),
        ],
        plan: Some("plus".to_owned()),
        reset_credits: 2,
        ..Codex::default()
    };

    assert_eq!(
        rows(&status)[4],
        "codex: 5h 18% resets 07:00Z, weekly 62% resets 11-30 02:00Z"
    );
}

#[test]
fn agys_row_names_each_quota_group_and_its_worst_window() {
    let mut status = idle();
    status.agy = Agy {
        groups: vec![
            gemini(
                used(30, "2026-11-23T07:00:00Z"),
                used(7, "2026-11-30T02:00:00Z"),
            ),
            QuotaGroup {
                name: "Claude and GPT models".to_owned(),
                ..gemini(WindowState::Unused, WindowState::Unknown)
            },
        ],
        error: None,
        seen: Some(now() * 1000),
        guard: None,
    };

    let shown = rows(&status);

    assert!(
        shown.contains(&"agy: Gemini Models 30%; Claude and GPT models 0%".to_owned()),
        "{}",
        shown.join("\n")
    );
    assert_eq!(
        shown.iter().filter(|row| row.starts_with("agy")).count(),
        1,
        "one row, whatever the groups"
    );
}

#[test]
fn a_codex_hold_says_what_holds_and_until_when() {
    let mut status = idle();
    status.codex = Codex {
        verdict: ClaudeState::Hold,
        until: Some(at("2026-11-30T02:00:00Z") * 1000),
        hold: Some("weekly 92%".to_owned()),
        windows: vec![
            codex_window(WindowKind::FiveHour, 18, "2026-11-23T07:00:00Z", true),
            codex_window(WindowKind::Weekly, 92, "2026-11-30T02:00:00Z", true),
        ],
        ..Codex::default()
    };

    assert_eq!(
        rows(&status)[4],
        "codex: hold: weekly 92% until 11-30 02:00Z; 5h 18%, weekly 92%"
    );
}

#[test]
fn agys_unreadable_answer_is_said_in_one_row_never_as_a_share() {
    let mut status = idle();
    status.agy = Agy {
        groups: Vec::new(),
        error: Some("its usage query exited 127".to_owned()),
        seen: Some(now() * 1000),
        guard: None,
    };

    let shown = rows(&status);

    assert!(
        shown.contains(&"agy usage unreadable: its usage query exited 127".to_owned()),
        "{}",
        shown.join("\n")
    );
    assert!(
        shown.iter().all(|row| !row.contains('%')),
        "{}",
        shown.join("\n")
    );
}

#[test]
fn an_unreadable_codex_says_why_and_never_reads_as_zero() {
    let mut status = idle();
    status.codex = Codex {
        unreadable: Some("it did not answer within 20s".to_owned()),
        ..Codex::default()
    };
    assert_eq!(
        rows(&status)[4],
        "codex usage unreadable: it did not answer within 20s"
    );

    // A hold from an earlier reading is not forgotten while Codex can't be read.
    status.codex.verdict = ClaudeState::Hold;
    status.codex.hold = Some("5h 93%".to_owned());
    status.codex.until = Some(at("2026-11-23T07:00:00Z") * 1000);
    assert_eq!(
        rows(&status)[4],
        "codex usage unreadable: it did not answer within 20s; held: 5h 93% until 07:00Z"
    );
}

#[test]
fn a_status_record_from_before_codex_still_reads() {
    let mut value = serde_json::to_value(idle()).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.remove("codex");
    }
    let old: Status = serde_json::from_value(value).unwrap_or_else(|_| idle());
    assert_eq!(old.codex, Codex::default());
}

#[test]
fn agy_with_no_reading_yet_says_so() {
    let shown = rows(&idle());
    assert!(
        shown.contains(&"agy: no reading yet".to_owned()),
        "{}",
        shown.join("\n")
    );
}

const USAGE: &str = r#"{"command":{"data":{"groups":[{"name":"Gemini Models","buckets":[
    {"window":"5h","remaining_fraction":0.7,"reset_time":"2099-01-01T00:00:00Z"},
    {"window":"weekly","remaining_fraction":0.93,"reset_time":"2099-01-07T00:00:00Z"}]}]}}}"#;

#[test]
fn an_answer_that_reads_is_a_reading_of_its_groups_taken_when_it_landed() {
    let agy = Agy::read(Ok(USAGE.to_owned()), 1_700_000_000_000);

    assert_eq!(agy.error, None);
    assert_eq!(agy.seen, Some(1_700_000_000_000));
    assert_eq!(agy.groups.len(), 1);
    assert_eq!(agy.groups[0].name, "Gemini Models");
}

#[test]
fn a_failed_query_and_a_garbled_answer_are_each_a_reading_with_a_reason_and_no_group() {
    let failed = Agy::read(Err("exited 127".to_owned()), 5);
    assert_eq!(failed.error.as_deref(), Some("exited 127"));
    assert!(failed.groups.is_empty());
    assert_eq!(failed.seen, Some(5));

    let garbled = Agy::read(Ok("not json".to_owned()), 5);
    assert_eq!(garbled.error.as_deref(), Some("its answer is not JSON"));
    assert!(garbled.groups.is_empty());
}

#[test]
fn a_reading_is_fresh_for_the_time_it_is_given_from_when_it_was_taken() {
    let agy = Agy::read(Ok(USAGE.to_owned()), 1_000_000);
    let five_minutes = 300_000;

    assert!(agy.fresh(1_000_000, five_minutes));
    assert!(agy.fresh(1_000_000 + five_minutes - 1, five_minutes));
    assert!(!agy.fresh(1_000_000 + five_minutes, five_minutes));
    // A clock that went back does not make an old reading new.
    assert!(!agy.fresh(1_000_000 - 1, five_minutes));
    assert!(!Agy::default().fresh(1_000_000, five_minutes), "none taken");
}
