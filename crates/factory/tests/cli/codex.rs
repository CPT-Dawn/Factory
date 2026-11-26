//! The Codex plan's quota: the daemon asks `codex app-server` for the windows, at most once in
//! 5 minutes; `factory quota codex`, `factory status` and `factory bar` show them, and reviews
//! hold off Codex at `[codex] hold_at_percent`. The app-server is the harness's fake
//! [`APP_SERVER`].

use serde_json::{Value, json};

use crate::claude::{HOUR, now_secs};
use crate::review::codex_writes;
use crate::router::{CLEAN_REPORT, claude_writes, codex_out_of_quota, configure, gate, open};
use crate::support::{self, APP_SERVER, Run, Sandbox, world};

/// The app-server's reply to `account/rateLimits/read`: a 5-hour window `five` percent spent
/// and a weekly one `weekly` percent spent, on the plus plan with two reset credits.
fn reply(five: f64, weekly: f64) -> String {
    json!({"id": 2, "result": {
        "rateLimits": {
            "primary": {"usedPercent": five, "windowDurationMins": 300, "resetsAt": now_secs() + 2 * HOUR},
            "secondary": {"usedPercent": weekly, "windowDurationMins": 10080, "resetsAt": now_secs() + 48 * HOUR},
            "planType": "plus"
        },
        "rateLimitResetCredits": {"availableCount": 2, "credits": [
            {"id": "a", "status": "available"}, {"id": "b", "status": "available"}
        ]}
    }})
    .to_string()
}

/// An app-server that reads its three messages, chatters like the real one, answers with
/// `reply` and then runs on until it is killed.
fn answers(sandbox: &Sandbox, reply: &str) {
    sandbox.fake(
        APP_SERVER,
        &format!(
            "read a; read b; read c\n\
             echo '{{\"method\":\"remoteControl/status/changed\",\"params\":{{}}}}'\n\
             echo '{{\"id\":1,\"result\":{{}}}}'\n\
             echo 'not json'\n\
             echo '{reply}'\n\
             exec sleep 60"
        ),
    );
}

fn quota(sandbox: &Sandbox) -> Run {
    sandbox.factory(&["quota", "codex"])
}

fn status_rows(sandbox: &Sandbox) -> Vec<String> {
    let out = sandbox.factory(&["status"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    out.stdout_lines().iter().map(|l| (*l).to_owned()).collect()
}

fn codex_row(sandbox: &Sandbox) -> String {
    status_rows(sandbox)
        .into_iter()
        .find(|row| row.starts_with("codex"))
        .unwrap_or_default()
}

fn bar_document(sandbox: &Sandbox) -> Value {
    // The bar starts no daemon: the first look wakes it.
    assert_eq!(sandbox.factory(&["monitor", "status"]).code, 0);
    let out = sandbox.factory(&["bar", "--json"]);
    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    serde_json::from_str(&out.stdout).unwrap_or(Value::Null)
}

fn chip<'a>(feed: &'a Value, id: &str) -> &'a Value {
    feed["glance"]
        .as_array()
        .and_then(|chips| chips.iter().find(|chip| chip["id"] == id))
        .unwrap_or(&Value::Null)
}

fn section<'a>(feed: &'a Value, id: &str) -> &'a Value {
    feed["sections"]
        .as_array()
        .and_then(|sections| sections.iter().find(|section| section["id"] == id))
        .unwrap_or(&Value::Null)
}

#[test]
fn a_codex_with_room_says_its_windows_its_plan_and_its_reset_credits() {
    let sandbox = Sandbox::new();
    answers(&sandbox, &reply(18.0, 40.0));

    let out = quota(&sandbox);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    assert_eq!(out.stdout_lines().len(), 1, "{out:?}");
    let line = out.stdout_lines()[0];
    assert!(line.starts_with("codex: 5h 18%"), "{out:?}");
    assert!(line.contains("weekly 40%"), "{out:?}");
    assert!(line.contains("plan plus, 2 reset credits"), "{out:?}");
    assert!(line.ends_with("; dispatch"), "{out:?}");
    let json = sandbox.factory(&["quota", "codex", "--json"]);
    let shown: Value = serde_json::from_str(&json.stdout).unwrap_or(Value::Null);
    assert_eq!(shown["verdict"], "go", "{shown}");
    assert_eq!(shown["plan"], "plus", "{shown}");
    assert_eq!(shown["reset_credits"], 2, "{shown}");
    assert_eq!(
        shown["windows"].as_array().map(Vec::len),
        Some(2),
        "{shown}"
    );
}

#[test]
fn a_second_look_within_five_minutes_makes_no_new_codex_call() {
    let sandbox = Sandbox::new();
    answers(&sandbox, &reply(18.0, 40.0));

    let first = codex_row(&sandbox);
    let _ = bar_document(&sandbox);
    let _ = bar_document(&sandbox);
    let _ = quota(&sandbox);
    let again = codex_row(&sandbox);

    assert!(first.starts_with("codex: 5h 18%"), "{first}");
    assert_eq!(first, again);
    assert_eq!(
        sandbox.calls(APP_SERVER).len(),
        1,
        "one ask of Codex for all of them"
    );
}

#[test]
fn a_week_past_the_hold_mark_holds_reviews_and_status_and_the_quota_say_why() {
    let sandbox = Sandbox::new();
    answers(&sandbox, &reply(18.0, 92.0));

    let out = quota(&sandbox);

    assert_eq!(out.code, 75, "{out:?}");
    assert_eq!(out.stdout, "", "{out:?}");
    assert!(
        out.stderr
            .starts_with("factory: codex: hold: weekly 92%; hold new reviews on codex until "),
        "{out:?}"
    );
    let row = codex_row(&sandbox);
    assert!(row.starts_with("codex: hold: weekly 92% until "), "{row}");
}

#[test]
fn the_hold_mark_is_the_configs() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[codex]\nhold_at_percent = 30\n");
    answers(&sandbox, &reply(18.0, 40.0));

    let out = quota(&sandbox);

    assert_eq!(out.code, 75, "{out:?}");
    assert!(
        out.stderr.starts_with("factory: codex: hold: weekly 40%"),
        "{out:?}"
    );
}

#[test]
fn a_bad_codex_config_is_a_precondition() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[codex]\nhold_at_percent = 120\n");

    let out = quota(&sandbox);

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stderr.contains("[codex] hold_at_percent is 120"),
        "{out:?}"
    );
}

/// An app-server that fails some way: the status and the bar say it could not be read, never 0%.
fn unreadable(sandbox: &Sandbox, why: &str) {
    let row = codex_row(sandbox);
    assert!(
        row.starts_with(&format!("codex usage unreadable: {why}")),
        "{row}"
    );
    assert!(!row.contains("0%"), "{row}");
    let out = quota(sandbox);
    assert_eq!(out.code, 0, "an unreadable Codex holds nothing: {out:?}");
    assert!(
        out.stdout
            .starts_with(&format!("codex usage unreadable: {why}")),
        "{out:?}"
    );
    let feed = bar_document(sandbox);
    let codex = chip(&feed, "codex");
    assert_eq!(codex["tone"], "muted", "{feed}");
    assert_eq!(codex["text"], "?", "{feed}");
    let card = section(&feed, "codex");
    assert!(
        card["error"]
            .as_str()
            .is_some_and(|error| error.contains(&format!("codex usage unreadable: {why}"))),
        "{card}"
    );
}

#[test]
fn a_codex_that_exits_is_unreadable_not_zero() {
    let sandbox = Sandbox::new();
    sandbox.fake(APP_SERVER, "read a; exit 3");

    unreadable(&sandbox, "it exited with status 3 before it answered");
}

#[test]
fn a_codex_that_is_not_installed_is_unreadable_not_zero() {
    let sandbox = Sandbox::new();
    std::fs::remove_file(sandbox.path("bin/codex")).unwrap_or_default();

    unreadable(&sandbox, "codex is not installed");
}

#[test]
fn a_garbled_answer_is_unreadable_not_zero() {
    let sandbox = Sandbox::new();
    sandbox.fake(
        APP_SERVER,
        "read a; read b; read c\necho '{\"id\":2,\"result\":{\"nonsense\":true}}'\nexec sleep 60",
    );

    unreadable(&sandbox, "its answer has no rate limits");
}

#[test]
fn output_that_is_no_json_before_the_server_ends_is_unreadable_not_zero() {
    let sandbox = Sandbox::new();
    sandbox.fake(
        APP_SERVER,
        "read a; read b; read c\necho 'Segmentation of the usage'\necho '{\"id\":2,\"result\":'\nexit 0",
    );

    unreadable(&sandbox, "it exited with status 0 before it answered");
}

#[test]
fn a_hung_codex_is_killed_at_its_timeout_and_the_looks_still_answer() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[codex]\ntimeout_seconds = 1\n");
    sandbox.fake(APP_SERVER, &with_a_child("exec sleep 60"));

    unreadable(&sandbox, "it did not answer within 1s");
    assert!(
        world::poll_until(|| pids(&sandbox).iter().all(|&pid| gone(pid))),
        "the hung server and its child were killed: {:?}",
        pids(&sandbox)
    );
    assert_eq!(
        sandbox.calls(APP_SERVER).len(),
        1,
        "a failed ask is not pressed again within 5 minutes"
    );
}

#[test]
fn the_chip_shows_the_worst_window_and_the_card_has_a_meter_for_each_and_the_plan() {
    let sandbox = Sandbox::new();
    answers(&sandbox, &reply(18.0, 92.0));

    let feed = bar_document(&sandbox);

    let codex = chip(&feed, "codex");
    assert_eq!(codex["text"], "92%", "{feed}");
    assert_eq!(codex["tone"], "critical", "{feed}");
    let card = section(&feed, "codex");
    let rows = card["rows"].as_array().cloned().unwrap_or_default();
    let meters: Vec<(&str, &Value)> = rows
        .iter()
        .filter(|row| row["kind"] == "meter")
        .map(|row| (row["label"].as_str().unwrap_or_default(), &row["value"]))
        .collect();
    assert_eq!(
        meters,
        [("5h", &json!(0.18)), ("weekly", &json!(0.92))],
        "{card}"
    );
    let text = card.to_string();
    assert!(text.contains("plus"), "{card}");
    assert!(text.contains("reset credits"), "{card}");
}

#[test]
fn a_held_codex_passes_a_review_to_claude_and_status_says_why() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"codex\"\n\n[[review.backend]]\ncli = \"claude\"\nmodel = \"opus\"\neffort = \"high\"\n",
    );
    answers(&sandbox, &reply(18.0, 92.0));
    sandbox.fake("codex", &codex_out_of_quota("1:00 AM"));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "--json", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    let shown: Value = serde_json::from_str(&out.stdout).unwrap_or(Value::Null);
    assert_eq!(shown["backend"], "claude", "{shown}");
    assert_eq!(shown["cooled"], json!([]), "a hold is no cooldown: {shown}");
    assert!(sandbox.calls("codex").is_empty(), "Codex was not run");
    let rows = status_rows(&sandbox);
    let at = rows.iter().position(|row| row.starts_with("  codex "));
    let held = at
        .and_then(|at| rows.get(at + 1))
        .cloned()
        .unwrap_or_default();
    assert!(
        rows[at.unwrap_or(0)].contains("cooling, resets "),
        "{rows:?}"
    );
    assert_eq!(held, "    held at weekly 92%", "{rows:?}");
}

#[test]
fn a_held_codex_with_no_other_backend_refuses_the_review_with_75() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(&sandbox, "[[review.backend]]\ncli = \"codex\"\n");
    answers(&sandbox, &reply(93.0, 20.0));
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 75, "{out:?}");
    assert!(
        out.stderr
            .contains("every review backend is out of quota (codex until "),
        "{out:?}"
    );
    assert!(out.stderr.contains("held at 5h 93%"), "{out:?}");
    assert!(sandbox.calls("codex").is_empty(), "Codex was not run");
}

#[test]
fn an_unreadable_codex_does_not_stop_a_review() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(&sandbox, "[[review.backend]]\ncli = \"codex\"\n");
    sandbox.fake(APP_SERVER, "exit 1");
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(sandbox.calls("codex").len(), 1);
}

#[test]
fn a_reading_outlives_a_daemon_restart_and_is_not_asked_for_again() {
    let sandbox = Sandbox::new();
    answers(&sandbox, &reply(18.0, 92.0));
    let before = codex_row(&sandbox);
    assert!(
        before.starts_with("codex: hold: weekly 92% until "),
        "{before}"
    );
    crate::router::kill_daemon(&sandbox);
    sandbox.fake(APP_SERVER, "exit 1");

    let row = codex_row(&sandbox);

    // The reading is five minutes old at most, restart or not: no new ask, so no new failure.
    assert_eq!(row, before);
    assert_eq!(quota(&sandbox).code, 75, "the stored windows still hold");
    assert_eq!(
        sandbox.calls(APP_SERVER).len(),
        1,
        "only the first daemon asked"
    );
}

#[test]
fn a_review_that_claude_takes_never_asks_codex_about_its_plan() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[[review.backend]]\ncli = \"claude\"\nmodel = \"opus\"\neffort = \"high\"\n\n[[review.backend]]\ncli = \"codex\"\n",
    );
    answers(&sandbox, &reply(18.0, 92.0));
    sandbox.fake("claude", &claude_writes(CLEAN_REPORT));

    let out = sandbox.factory_in(&repo, &["review", "main", "t1-r1"]);

    assert_eq!(out.code, 0, "{out:?}");
    assert!(sandbox.calls(APP_SERVER).is_empty(), "Codex was not asked");
}

#[test]
fn a_review_waits_out_an_ask_that_a_look_gives_up_on() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    configure(
        &sandbox,
        "[codex]\ntimeout_seconds = 60\n\n[[review.backend]]\ncli = \"codex\"\n",
    );
    // Codex answers only once the gate opens, after a look has given up on the ask.
    let gate = gate(&sandbox);
    sandbox.fake(
        APP_SERVER,
        &format!(
            "read a; read b; read c\nread go < '{}'\necho '{}'\nexec sleep 60",
            gate.display(),
            reply(18.0, 92.0)
        ),
    );
    sandbox.fake("codex", &codex_writes(CLEAN_REPORT));
    let mut cmd = sandbox.factory_command(&["review", "main", "t1-r1"]);
    cmd.current_dir(&repo);
    let mut review = support::spawn(cmd);
    assert!(
        world::poll_until(|| sandbox.calls(APP_SERVER).len() == 1),
        "the review never asked about the plan"
    );

    let row = codex_row(&sandbox);

    assert!(
        row.starts_with("codex usage unreadable: it is still being asked"),
        "{row}"
    );
    assert!(
        matches!(review.try_wait(), Ok(None)),
        "the review did not wait for the answer"
    );
    open(&gate);
    let out = support::finish(review);
    assert_eq!(out.code, 75, "{out:?}");
    assert!(out.stderr.contains("held at weekly 92%"), "{out:?}");
    assert!(sandbox.calls("codex").is_empty(), "Codex was not run");
    assert_eq!(sandbox.calls(APP_SERVER).len(), 1, "one ask for both");
}

#[test]
fn a_failed_ask_is_not_repeated_by_a_restart_within_five_minutes() {
    let sandbox = Sandbox::new();
    sandbox.fake(APP_SERVER, "exit 1");
    assert!(
        codex_row(&sandbox).starts_with("codex usage unreadable: "),
        "the first ask fails"
    );
    crate::router::kill_daemon(&sandbox);
    answers(&sandbox, &reply(18.0, 40.0));

    let row = codex_row(&sandbox);

    assert!(
        !row.contains("5h 18%"),
        "{row}: Codex was not pressed again"
    );
    assert_eq!(sandbox.calls(APP_SERVER).len(), 1);
}

/// `body`, with a child of its own beside it; it notes both pids in `codex-app-server.pids`.
fn with_a_child(body: &str) -> String {
    format!("sleep 60 &\necho $! $$ > \"$0.pids\"\n{body}")
}

/// The pids `with_a_child` noted.
fn pids(sandbox: &Sandbox) -> Vec<u32> {
    std::fs::read_to_string(sandbox.path(format!("bin/{APP_SERVER}.pids")))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// Whether process `pid` has ended: it is gone, or a zombie no one has reaped yet.
fn gone(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z'))
    })
}

#[test]
fn the_exchange_is_three_messages_with_no_jsonrpc_field() {
    let sandbox = Sandbox::new();
    sandbox.fake(
        APP_SERVER,
        &format!(
            "read -r a; read -r b; read -r c\n\
             printf '%s\\n%s\\n%s\\n' \"$a\" \"$b\" \"$c\" > \"$0.received\"\n\
             echo '{}'\nexec sleep 60",
            reply(18.0, 40.0)
        ),
    );

    assert_eq!(quota(&sandbox).code, 0);

    let received = std::fs::read_to_string(sandbox.path(format!("bin/{APP_SERVER}.received")))
        .unwrap_or_default();
    let lines: Vec<&str> = received.lines().collect();
    assert_eq!(lines.len(), 3, "{received}");
    let first: Value = serde_json::from_str(lines[0]).unwrap_or(Value::Null);
    assert_eq!(
        (&first["id"], &first["method"]),
        (&json!(1), &json!("initialize")),
        "{received}"
    );
    assert_eq!(
        first["params"]["clientInfo"]["name"], "factory",
        "{received}"
    );
    assert_eq!(lines[1], r#"{"method":"initialized"}"#);
    assert_eq!(lines[2], r#"{"id":2,"method":"account/rateLimits/read"}"#);
    assert!(!received.contains("jsonrpc"), "{received}");
    assert_eq!(sandbox.calls(APP_SERVER), [vec!["app-server".to_owned()]]);
}

#[test]
fn the_server_and_its_children_are_killed_once_it_has_answered() {
    let sandbox = Sandbox::new();
    sandbox.fake(
        APP_SERVER,
        &with_a_child(&format!(
            "read a; read b; read c\necho '{}'\nexec sleep 60",
            reply(18.0, 40.0)
        )),
    );

    let out = quota(&sandbox);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(pids(&sandbox).len(), 2, "the fake noted its pids");
    assert!(
        world::poll_until(|| pids(&sandbox).iter().all(|&pid| gone(pid))),
        "the answered server and its child were killed: {:?}",
        pids(&sandbox)
    );
}

#[test]
fn a_server_that_never_stops_chattering_is_killed_at_its_timeout() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[codex]\ntimeout_seconds = 1\n");
    sandbox.fake(
        APP_SERVER,
        "read a; read b; read c\nwhile :; do echo '{\"method\":\"account/updated\"}'; done",
    );

    unreadable(&sandbox, "it did not answer within 1s");
}

#[test]
fn a_request_of_the_servers_own_with_id_2_is_not_the_answer() {
    let sandbox = Sandbox::new();
    configure(&sandbox, "[codex]\ntimeout_seconds = 1\n");
    sandbox.fake(
        APP_SERVER,
        "read a; read b; read c\necho '{\"id\":2,\"method\":\"item/tool/requestUserInput\"}'\nexec sleep 60",
    );

    unreadable(&sandbox, "it did not answer within 1s");
}

#[test]
fn a_store_from_before_the_ask_was_kept_holds_beside_a_failed_ask() {
    // A database an earlier build made: its `codex_quota` has no `asked_ms`, and holds a week
    // at 92% read ten minutes ago.
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.path("state/factory")).unwrap_or_default();
    {
        let db = rusqlite::Connection::open(sandbox.path("state/factory/factory.db"))
            .unwrap_or_else(|e| panic!("make an old store: {e}"));
        let made = db.execute_batch(
            "CREATE TABLE codex_quota (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 used_percent REAL,
                 resets_ms INTEGER,
                 seen_ms INTEGER,
                 weekly_used_percent REAL,
                 weekly_resets_ms INTEGER,
                 weekly_seen_ms INTEGER,
                 plan TEXT,
                 reset_credits INTEGER NOT NULL DEFAULT 0
             );
             PRAGMA user_version = 6;",
        );
        assert_eq!(made, Ok(()));
        let seeded = db.execute(
            "INSERT INTO codex_quota (id, weekly_used_percent, weekly_resets_ms, weekly_seen_ms, plan)
             VALUES (1, 92.0, ?1, ?2, 'plus')",
            [(now_secs() + 48 * HOUR) * 1000, (now_secs() - 600) * 1000],
        );
        assert_eq!(seeded, Ok(1));
    }
    sandbox.fake(APP_SERVER, "exit 1");

    let out = quota(&sandbox);

    assert_eq!(out.code, 75, "the stored week still holds: {out:?}");
    assert!(
        out.stderr
            .starts_with("factory: codex: hold: weekly 92%; hold new reviews on codex until "),
        "{out:?}"
    );
    assert!(
        out.stderr
            .contains("; codex usage unreadable: it exited with status 1 before it answered"),
        "{out:?}"
    );
    let row = codex_row(&sandbox);
    assert!(
        row.starts_with(
            "codex usage unreadable: it exited with status 1 before it answered; held: "
        ),
        "{row}"
    );
    assert_eq!(
        sandbox.calls(APP_SERVER).len(),
        1,
        "the old reading was asked about once"
    );
    // The ask the upgraded store now keeps outlives a restart.
    crate::router::kill_daemon(&sandbox);
    assert_eq!(quota(&sandbox).code, 75);
    assert_eq!(
        sandbox.calls(APP_SERVER).len(),
        1,
        "no new ask after the restart"
    );
}
