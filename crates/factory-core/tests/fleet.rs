//! The watched CI as the bar and the status screen show it: the chip's mark, the card's rows,
//! how a failed read shows, and the rules that sort runners, jobs and pull requests.

use factory_core::bar::{self, Snapshot};
use factory_core::ci::CheckRun;
use factory_core::fleet::{
    Ci, Job, Mark, Pull, PullState, RepoCi, Runner, RunnerState, github_repo, is_factory_branch,
    is_repo,
};
use factory_core::status::{self, Claude, Leads, Status};
use factory_core::timestamp::Timestamp;
use serde_json::{Value, json};

/// 2026-11-23 04:13:00 UTC, in milliseconds.
fn now() -> i64 {
    Timestamp::parse("2026-11-23T04:13:00Z").map_or(0, Timestamp::unix_millis)
}

fn minutes_ago(minutes: i64) -> i64 {
    now() - minutes * 60_000
}

fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
    CheckRun {
        id: 1,
        name: name.to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        url: String::new(),
        app: Some("github-actions".to_owned()),
    }
}

fn runner(id: u64, state: RunnerState) -> Runner {
    Runner {
        id,
        name: format!("ci-{id}"),
        state,
    }
}

fn job(id: u64, status: &str, queued: i64) -> Job {
    Job {
        id,
        run_id: 7,
        name: "test".to_owned(),
        status: status.to_owned(),
        conclusion: None,
        runner: None,
        queued: Some(queued),
        started: None,
        finished: None,
    }
}

fn pull(number: u64, state: PullState) -> Pull {
    Pull {
        number,
        branch: format!("factory/s1-t{number}"),
        url: format!("https://github.com/acme/shop/pull/{number}"),
        state,
    }
}

/// acme/shop, read a minute ago: 2 of 3 runners busy, one offline, against 4 expected.
fn shop() -> RepoCi {
    RepoCi {
        repo: "acme/shop".to_owned(),
        seen: Some(minutes_ago(1)),
        error: None,
        runners: vec![
            runner(1, RunnerState::Busy),
            runner(2, RunnerState::Busy),
            runner(3, RunnerState::Offline),
        ],
        jobs: Vec::new(),
        pulls: Vec::new(),
        finished: Vec::new(),
    }
}

fn ci(repo: RepoCi) -> Ci {
    Ci {
        expected: 4,
        error: None,
        repos: vec![repo],
        next: None,
    }
}

fn no_leads() -> Leads {
    Leads {
        error: None,
        monitor: false,
        leads: Vec::new(),
    }
}

fn snapshot(ci: Ci) -> Snapshot {
    Snapshot {
        now: now(),
        claude: Claude::default(),
        codex: Default::default(),
        agy: Default::default(),
        waits: Vec::new(),
        leads: no_leads(),
        ci,
        lab: Default::default(),
    }
}

fn feed(ci: Ci) -> Value {
    serde_json::to_value(bar::feed(&snapshot(ci))).unwrap_or(Value::Null)
}

fn find<'a>(feed: &'a Value, list: &str, id: &str) -> &'a Value {
    feed[list]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == id))
        .unwrap_or(&Value::Null)
}

#[test]
fn a_pull_request_fails_as_soon_as_one_check_failed_even_while_others_run() {
    let cases: [(&[CheckRun], PullState); 5] = [
        (&[], PullState::Pending),
        (&[run("lint", "in_progress", None)], PullState::Pending),
        (
            &[
                run("lint", "in_progress", None),
                run("test", "completed", Some("failure")),
            ],
            PullState::Failing,
        ),
        (
            &[
                run("lint", "completed", Some("success")),
                run("docs", "completed", Some("skipped")),
            ],
            PullState::Passing,
        ),
        (
            &[run("test", "completed", Some("cancelled"))],
            PullState::Failing,
        ),
    ];
    for (runs, state) in cases {
        assert_eq!(PullState::of(runs), state, "{runs:?}");
    }
}

#[test]
fn runners_are_busy_idle_or_offline() {
    assert_eq!(RunnerState::of("online", true), RunnerState::Busy);
    assert_eq!(RunnerState::of("online", false), RunnerState::Idle);
    assert_eq!(RunnerState::of("offline", true), RunnerState::Offline);
}

#[test]
fn only_factory_branches_and_owner_name_repos_count() {
    assert!(is_factory_branch("factory/q67-t74"));
    assert!(!is_factory_branch("feature/factory/x"));
    assert!(is_repo("acme/shop.rs"));
    for bad in ["acme", "acme/", "/shop", "acme/shop/x", "acme/sh op"] {
        assert!(!is_repo(bad), "{bad}");
    }
}

#[test]
fn the_card_counts_busy_runners_against_the_expected_ones_and_the_queue_by_its_oldest_wait() {
    let mut repo = shop();
    repo.jobs = vec![
        job(70, "queued", minutes_ago(12)),
        job(71, "queued", minutes_ago(3)),
        job(72, "in_progress", minutes_ago(20)),
    ];
    repo.pulls = vec![pull(12, PullState::Pending)];
    let feed = feed(ci(repo));

    let card = find(&feed, "sections", "ci");
    assert_eq!(
        card["rows"],
        json!([
            {"kind": "meter", "label": "runners", "value": 0.5,
             "text": "2/4 busy, 1 offline, 1 missing", "tone": "warn"},
            {"kind": "text", "label": "queue", "text": "2 queued, oldest 12m", "tone": "busy"},
            {"kind": "text", "label": "shop#12", "text": "pending", "tone": "busy",
             "url": "https://github.com/acme/shop/pull/12"},
        ]),
        "{card}"
    );
    assert_eq!(card["stale_since"], Value::Null);
    let chip = find(&feed, "glance", "ci");
    assert_eq!(
        (&chip["text"], &chip["tone"]),
        (&json!("busy"), &json!("busy"))
    );
}

#[test]
fn the_chip_is_a_cross_while_any_factory_pr_fails_and_a_tick_once_all_pass() {
    let mut failing = shop();
    failing.pulls = vec![pull(12, PullState::Passing), pull(13, PullState::Failing)];
    let chip = find(&feed(ci(failing.clone())), "glance", "ci").clone();
    assert_eq!(chip["text"], "✗");
    assert_eq!(chip["tone"], "critical");
    assert_eq!(chip["label"], bar::glyph::CI_FAILING.char.to_string());
    assert_eq!(ci(failing).mark(), Mark::Failing);

    let mut passing = shop();
    passing.pulls = vec![pull(12, PullState::Passing)];
    let snapshot = snapshot(ci(passing));
    let chip = find(
        &serde_json::to_value(bar::feed(&snapshot)).unwrap_or(Value::Null),
        "glance",
        "ci",
    )
    .clone();
    assert_eq!((&chip["text"], &chip["tone"]), (&json!("✓"), &json!("ok")));
    assert_eq!(
        bar::line(&snapshot),
        "claude ?/? · codex ?/? · agy ?/? · 0 leads · lab 0/0 · CI ✓"
    );
}

#[test]
fn without_watched_repos_the_bar_has_no_ci_chip_card_or_line_mark() {
    let snapshot = snapshot(Ci::default());
    let feed = bar::feed(&snapshot);
    assert!(feed.glance.iter().all(|chip| chip.id != "ci"));
    assert!(feed.sections.iter().all(|section| section.id != "ci"));
    assert_eq!(
        bar::line(&snapshot),
        "claude ?/? · codex ?/? · agy ?/? · 0 leads · lab 0/0"
    );
}

#[test]
fn a_failed_read_keeps_the_numbers_stale_since_the_last_good_answer() {
    let mut repo = shop();
    repo.error = Some("gh: API rate limit exceeded (HTTP 403)".to_owned());
    let feed = feed(ci(repo));
    let card = find(&feed, "sections", "ci");
    assert_eq!(
        card["stale_since"],
        Timestamp::from_unix_millis(minutes_ago(1)).to_string()
    );
    assert_eq!(card["error"], Value::Null, "the numbers still stand");
    assert_eq!(card["rows"][0]["text"], "2/4 busy, 1 offline, 1 missing");
}

#[test]
fn a_repo_that_never_answered_shows_why_rather_than_no_runners() {
    let repo = RepoCi {
        repo: "acme/shop".to_owned(),
        error: Some("gh: Bad credentials (HTTP 401)".to_owned()),
        ..RepoCi::default()
    };
    let feed = feed(ci(repo));
    let card = find(&feed, "sections", "ci");
    assert_eq!(card["error"], "acme/shop: gh: Bad credentials (HTTP 401)");
    assert_eq!(card["rows"], json!([]));
    assert_eq!(find(&feed, "glance", "ci")["text"], "?");
}

#[test]
fn the_status_screen_sums_the_ci_in_one_line() {
    let mut repo = shop();
    repo.jobs = vec![job(70, "queued", minutes_ago(12))];
    repo.pulls = vec![pull(12, PullState::Failing), pull(13, PullState::Passing)];
    repo.error = Some("timeout".to_owned());
    let status = Status {
        now: now(),
        lab: status::Lab::default(),
        review: Vec::new(),
        claude: Claude::default(),
        codex: Default::default(),
        agy: Default::default(),
        waits: Vec::new(),
        leads: no_leads(),
        ci: ci(repo),
    };
    let screen = status::screen(&status);
    let at = screen
        .iter()
        .position(|row| row.starts_with("ci: "))
        .unwrap_or(0);
    let row = &screen[at];
    assert!(
        row.starts_with("ci: stale 1m · PRs 1 failing, 1 passing · 2/4 busy, 1 offline, 1 missing"),
        "the failing pull request before the queue the screen cuts: {screen:#?}"
    );
    assert!(row.chars().count() <= 80, "{row}");
    assert!(
        !screen[at + 1..].iter().any(|row| row.starts_with("  ")),
        "one row: {screen:#?}"
    );
}

#[test]
fn a_remote_on_github_names_its_repo_in_each_way_git_writes_one() {
    for url in [
        "git@github.com:acme/shop.git",
        "git@github.com:acme/shop",
        "https://github.com/acme/shop.git",
        "https://github.com/acme/shop/",
        "ssh://git@github.com/acme/shop.git",
        "https://user@github.com/acme/shop",
    ] {
        assert_eq!(github_repo(url).as_deref(), Some("acme/shop"), "{url}");
    }
    for url in [
        "git@gitlab.com:acme/shop.git",
        "https://github.com/acme",
        "https://github.com/acme/shop/tree/main",
        "/srv/git/shop.git",
        "",
    ] {
        assert_eq!(github_repo(url), None, "{url}");
    }
}
