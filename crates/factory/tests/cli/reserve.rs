//! `factory lab reserve` (#17): a lead sets slots aside for its implementers, and each
//! implementer's acquire uses the reservation its environment carries.

use std::path::Path;
use std::process::{Child, Command};

use crate::lab::{await_waiter, devlab, lab, stop_daemon, write_machine_config};
use crate::support::world::{World, poll_until};
use crate::support::{Run, Sandbox, finish, run, spawn};

/// A long-lived process standing in for an agent: the owner of what its calls lease.
struct Agent(Child);

impl Agent {
    fn new() -> Agent {
        match Command::new("sleep").arg("120").spawn() {
            Ok(child) => Agent(child),
            Err(e) => panic!("cannot start an agent: {e}"),
        }
    }

    fn pid(&self) -> String {
        self.0.id().to_string()
    }

    fn label(&self) -> String {
        format!("sleep:{}", self.0.id())
    }

    /// Ends the agent, so its leases and reservations are its owner's no more.
    fn exit(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.exit();
    }
}

fn call(
    sandbox: &Sandbox,
    dir: &Path,
    agent: &Agent,
    env: &[(&str, &str)],
    args: &[&str],
) -> Command {
    let mut cmd = devlab(sandbox, dir, args);
    cmd.env("DEVLAB_OWNER_PID", agent.pid());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

fn as_agent(sandbox: &Sandbox, dir: &Path, agent: &Agent, args: &[&str]) -> Run {
    run(call(sandbox, dir, agent, &[], args))
}

fn slot_of(out: &Run) -> String {
    assert_eq!(out.code, 0, "{out:?}");
    out.stdout
        .lines()
        .find_map(|line| line.strip_prefix("export DEVLAB_SLOT="))
        .unwrap_or_else(|| panic!("no DEVLAB_SLOT in {out:?}"))
        .trim_matches('\'')
        .to_owned()
}

/// The id of the one reservation `reserve` printed.
fn reservation(out: &Run) -> String {
    assert_eq!(out.code, 0, "{out:?}");
    let line = out.stdout.trim();
    let Some((_, after)) = line.split_once("reservations ") else {
        panic!("no reservation ids in {line}");
    };
    after
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// The lead, a second owner holding slot 1 of demo's two, and the lead's worktree. With the
/// default cap (all but one) the lead may reserve one slot.
fn lead_and_a_full_board(sandbox: &Sandbox, world: &World) -> (Agent, Agent, std::path::PathBuf) {
    let (lead, other) = (Agent::new(), Agent::new());
    let wt = world.worktree("demo-lead");
    let held = as_agent(
        sandbox,
        &world.worktree("demo-other"),
        &other,
        &["acquire", "--no-device", "--quiet"],
    );
    assert_eq!(slot_of(&held), "0");
    (lead, other, wt)
}

#[test]
fn reserve_grants_up_to_the_owners_cap_and_says_how_many() {
    let (sandbox, world) = lab();
    let (lead, wt) = (Agent::new(), world.worktree("demo-lead"));

    // The default cap on demo's two slots is one.
    let out = as_agent(&sandbox, &wt, &lead, &["reserve", "--slots", "2"]);
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (
            0,
            "granted 1 of 2 demo slots (cap 1): reservations 1; pass each to an implementer as DEVLAB_RESERVATION=<id>\n",
            ""
        )
    );
}

#[test]
fn reserve_takes_two_when_the_cap_allows_it() {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""maxSlotsPerOwner": 2"#);
    let (lead, wt) = (Agent::new(), world.worktree("demo-lead"));

    let out = as_agent(&sandbox, &wt, &lead, &["reserve", "--slots", "3"]);
    assert_eq!(
        (out.code, out.stdout.as_str()),
        (
            0,
            "granted 2 of 3 demo slots (cap 2): reservations 1, 2; pass each to an implementer as DEVLAB_RESERVATION=<id>\n"
        )
    );
}

#[test]
fn reserve_speaks_json() {
    let (sandbox, world) = lab();
    let (lead, wt) = (Agent::new(), world.worktree("demo-lead"));

    let out = run(call(&sandbox, &wt, &lead, &[], &["reserve", "--json"]));
    let json: serde_json::Value =
        serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        json,
        serde_json::json!({
            "project": "demo", "requested": 1, "granted": 1, "cap": 1,
            "reservations": [{"id": 1, "slot": 0}]
        })
    );
    // The global flag reaches it too.
    let mut cmd = sandbox.factory_command(&["--json", "lab", "reserve", "--release"]);
    cmd.current_dir(&wt).env("DEVLAB_OWNER_PID", lead.pid());
    let released = run(cmd);
    assert_eq!(
        (released.code, released.stdout.as_str()),
        (0, "{\"project\":\"demo\",\"released\":1}\n")
    );
}

#[test]
fn an_owner_with_no_room_left_exits_75_at_once_naming_the_cap_and_an_eta() {
    let (sandbox, world) = lab();
    let (lead, wt) = (Agent::new(), world.worktree("demo-lead"));
    assert_eq!(as_agent(&sandbox, &wt, &lead, &["reserve"]).code, 0);

    // No --wait 0: it must not queue for the default eight minutes.
    let over = as_agent(&sandbox, &wt, &lead, &["reserve"]);
    assert_eq!((over.code, over.stdout.as_str()), (75, ""));
    assert_eq!(
        over.stderr,
        format!(
            "devlab: owner {} already holds 1 of 2 demo stack slots (cap 1); a slot of its own frees in ~30 min: release a lease or reservation, or retry then\n",
            lead.label()
        )
    );
    let json = run(call(&sandbox, &wt, &lead, &[], &["reserve", "--json"]));
    let json: serde_json::Value = serde_json::from_str(&json.stdout).expect("a json object");
    assert_eq!(json["exit"], 75);
    assert!(
        json["error"].as_str().is_some_and(|e| e.contains("cap 1")),
        "{json}"
    );
}

#[test]
fn a_reservation_is_handed_to_a_child_through_the_environment_and_its_acquire_uses_it() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    let id = reservation(&as_agent(&sandbox, &wt, &lead, &["reserve"]));
    let (child, dir) = (Agent::new(), world.worktree("demo-child"));
    let ask = ["acquire", "--no-device", "--quiet", "--wait", "0"];

    // Every slot is held or reserved: without the reservation the child finds nothing free.
    let without = as_agent(&sandbox, &dir, &child, &ask);
    assert_eq!(without.code, 75, "{without:?}");
    let with = run(call(
        &sandbox,
        &dir,
        &child,
        &[("DEVLAB_RESERVATION", &id)],
        &ask,
    ));
    assert_eq!(slot_of(&with), "1");

    // The lease is the child's now: it is in its owner's name, and the lead's cap is free again.
    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(status.contains("demo-child"), "{status}");
    assert!(!status.contains("reserved"), "{status}");
    // A reservation is used once: the same id asks for a slot in the normal way after that.
    let again = run(call(
        &sandbox,
        &world.worktree("demo-child2"),
        &child,
        &[("DEVLAB_RESERVATION", &id)],
        &ask,
    ));
    assert_eq!(again.code, 75, "{again:?}");
}

#[test]
fn a_run_gives_its_slot_back_to_the_reservation_it_came_from_for_the_next_run() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    let id = reservation(&as_agent(&sandbox, &wt, &lead, &["reserve"]));
    let (child, dir) = (Agent::new(), world.worktree("demo-child"));
    let suite = [
        "run",
        "--no-device",
        "--stack",
        "--",
        "sh",
        "-c",
        "echo \"slot $DEVLAB_SLOT\"",
    ];
    let reserved = [("DEVLAB_RESERVATION", id.as_str())];

    let first = run(call(&sandbox, &dir, &child, &reserved, &suite));
    assert_eq!(
        (first.code, first.stdout.as_str()),
        (0, "slot 1\n"),
        "{first:?}"
    );
    // The slot is the lead's reservation again, under the same id: nobody else gets it.
    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(
        status.contains(&format!("reserved by {} (reservation {id})", lead.label())),
        "{status}"
    );
    let outsider = Agent::new();
    let ask = ["acquire", "--no-device", "--quiet", "--wait", "0"];
    let shut_out = as_agent(&sandbox, &world.worktree("demo-outsider"), &outsider, &ask);
    assert_eq!(shut_out.code, 75, "{shut_out:?}");
    // The next run takes it at once.
    let second = run(call(&sandbox, &dir, &child, &reserved, &suite));
    assert_eq!(
        (second.code, second.stdout.as_str()),
        (0, "slot 1\n"),
        "{second:?}"
    );
    assert!(!second.stderr.contains("waiting:"), "{second:?}");
}

#[test]
fn a_run_whose_owner_exits_still_gives_its_slot_back_to_the_reservation() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    let id = reservation(&as_agent(&sandbox, &wt, &lead, &["reserve"]));
    let (mut child, dir) = (Agent::new(), world.worktree("demo-child"));
    let script = "touch started; while [ ! -e go ]; do sleep 0.05; done";
    let suite = [
        "run",
        "--no-device",
        "--stack",
        "--quiet",
        "--",
        "sh",
        "-c",
        script,
    ];
    let running = spawn(call(
        &sandbox,
        &dir,
        &child,
        &[("DEVLAB_RESERVATION", id.as_str())],
        &suite,
    ));
    assert!(
        poll_until(|| dir.join("started").exists()),
        "the command never started"
    );

    // The run's owner exits: the gc ends its lease before the run does.
    child.exit();
    assert_eq!(as_agent(&sandbox, &wt, &lead, &["gc"]).code, 0);
    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(
        status.contains(&format!("reserved by {} (reservation {id})", lead.label())),
        "{status}"
    );

    std::fs::write(dir.join("go"), "").expect("write go");
    assert_eq!(finish(running).code, 0);
    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(
        status.contains(&format!("reserved by {} (reservation {id})", lead.label())),
        "{status}"
    );
}

#[test]
fn status_shows_a_reserved_slot_as_the_leads() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    assert_eq!(as_agent(&sandbox, &wt, &lead, &["reserve"]).code, 0);

    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(
        status.contains(&format!("reserved by {}", lead.label())),
        "{status}"
    );
    assert!(
        status.contains(&format!("{} 1/1 at cap", lead.label())),
        "{status}"
    );
}

#[test]
fn a_waiting_reserve_prints_one_line_when_it_starts_to_wait_and_one_when_it_ends() {
    let (sandbox, world) = lab();
    let (lead, other, wt) = lead_and_a_full_board(&sandbox, &world);
    let third = Agent::new();
    let held = as_agent(
        &sandbox,
        &world.worktree("demo-third"),
        &third,
        &["acquire", "--no-device", "--quiet"],
    );
    assert_eq!(slot_of(&held), "1");

    let waiting = spawn(call(
        &sandbox,
        &wt,
        &lead,
        &[],
        &["reserve", "--wait", "30"],
    ));
    await_waiter(&sandbox, &wt);
    assert_eq!(
        as_agent(
            &sandbox,
            &world.worktree("demo-third"),
            &third,
            &["release"]
        )
        .code,
        0
    );
    let out = finish(waiting);

    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        out.stderr,
        "devlab: waiting: all 2 stack slots of demo are held\n"
    );
    assert_eq!(out.stdout.lines().count(), 1, "{out:?}");
    assert!(
        out.stdout.starts_with("granted 1 of 1 demo slots"),
        "{out:?}"
    );
    drop(other);
}

#[test]
fn a_reservation_outlives_a_daemon_restart() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    let id = reservation(&as_agent(&sandbox, &wt, &lead, &["reserve"]));
    stop_daemon(&sandbox);

    let (child, dir) = (Agent::new(), world.worktree("demo-child"));
    let out = run(call(
        &sandbox,
        &dir,
        &child,
        &[("DEVLAB_RESERVATION", &id)],
        &["acquire", "--no-device", "--quiet", "--wait", "0"],
    ));
    assert_eq!(slot_of(&out), "1");
}

#[test]
fn a_reservation_goes_when_its_owner_exits() {
    let (sandbox, world) = lab();
    let (mut lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    assert_eq!(as_agent(&sandbox, &wt, &lead, &["reserve"]).code, 0);
    let (child, dir) = (Agent::new(), world.worktree("demo-child"));
    let ask = ["acquire", "--no-device", "--quiet", "--wait", "0"];
    assert_eq!(as_agent(&sandbox, &dir, &child, &ask).code, 75);

    lead.exit();
    let out = as_agent(&sandbox, &dir, &child, &ask);
    assert_eq!(slot_of(&out), "1");
}

#[test]
fn reserve_release_gives_the_owners_reservations_back() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    assert_eq!(as_agent(&sandbox, &wt, &lead, &["reserve"]).code, 0);

    let released = as_agent(&sandbox, &wt, &lead, &["reserve", "--release"]);
    assert_eq!(
        (released.code, released.stdout.as_str()),
        (0, "released 1 demo reservation\n")
    );
    let again = as_agent(&sandbox, &wt, &lead, &["reserve", "--wait", "0"]);
    assert_eq!(again.code, 0, "{again:?}");
}

#[test]
fn reserve_needs_a_project_and_a_readable_count() {
    let (sandbox, world) = lab();
    let (lead, bare) = (Agent::new(), world.worktree("bare"));
    let out = as_agent(&sandbox, &bare, &lead, &["reserve"]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr.contains("devlab.json"), "{out:?}");

    let wt = world.worktree("demo-lead");
    let out = as_agent(&sandbox, &wt, &lead, &["reserve", "--slots", "0"]);
    assert_eq!(
        (out.code, out.stderr.as_str()),
        (2, "devlab: --slots takes a whole number of 1 or more\n")
    );
}

#[test]
fn a_reservation_backed_phone_request_that_waits_out_its_time_leaves_the_reservation_and_no_lease()
{
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    let id = reservation(&as_agent(&sandbox, &wt, &lead, &["reserve"]));
    let (child, dir) = (Agent::new(), world.worktree("demo-child"));

    // No phone is plugged in: the request waits for one, and gives up.
    let ask = ["acquire", "--phone", "--quiet", "--wait", "1"];
    let out = run(call(
        &sandbox,
        &dir,
        &child,
        &[("DEVLAB_RESERVATION", &id)],
        &ask,
    ));
    assert_eq!((out.code, out.stdout.as_str()), (75, ""), "{out:?}");

    // The reservation is the lead's still, and nobody holds a lease.
    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(
        status.contains(&format!("reserved by {} (reservation {id})", lead.label())),
        "{status}"
    );
    assert!(!status.contains("demo-child"), "{status}");
    assert_eq!(run(call(&sandbox, &dir, &child, &[], &["env"])).code, 4);
}

#[test]
fn a_reservation_backed_lease_whose_phone_goes_before_its_prep_is_given_back_to_the_reservation() {
    let (sandbox, world) = lab();
    let (lead, _other, wt) = lead_and_a_full_board(&sandbox, &world);
    let id = reservation(&as_agent(&sandbox, &wt, &lead, &["reserve"]));
    let (child, dir) = (Agent::new(), world.worktree("demo-child"));
    for step in [
        "plug FAKEPHONE01 device usb:1-1 model:Pixel_8",
        "yank FAKEPHONE01",
    ] {
        world.apply(step).expect("a world step");
    }

    let ask = ["acquire", "--phone", "--quiet", "--wait", "1"];
    let out = run(call(
        &sandbox,
        &dir,
        &child,
        &[("DEVLAB_RESERVATION", &id)],
        &ask,
    ));
    assert_eq!(out.code, 76, "{out:?}");

    let status = as_agent(&sandbox, &wt, &lead, &["status"]).stdout;
    assert!(
        status.contains(&format!("reserved by {} (reservation {id})", lead.label())),
        "{status}"
    );
    assert_eq!(run(call(&sandbox, &dir, &child, &[], &["env"])).code, 4);
}

#[test]
fn a_run_that_gives_its_slot_back_to_a_reservation_is_its_last_use() {
    let (sandbox, world) = lab();
    write_machine_config(&sandbox, r#""maxSlotsPerOwner": 2"#);
    let (lead, wt) = (Agent::new(), world.worktree("demo-lead"));
    let reserved = as_agent(&sandbox, &wt, &lead, &["reserve", "--slots", "2"]);
    assert_eq!(reserved.code, 0, "{reserved:?}");
    let (child, dir) = (Agent::new(), world.worktree("demo-child"));
    let suite = ["run", "--no-device", "--stack", "--quiet", "--", "true"];
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let ran = run(call(
        &sandbox,
        &dir,
        &child,
        &[("DEVLAB_RESERVATION", "2")],
        &suite,
    ));
    assert_eq!(ran.code, 0, "{ran:?}");

    let mut cmd = sandbox.factory_command(&["lab", "status", "--json"]);
    cmd.current_dir(&wt).env("DEVLAB_OWNER_PID", lead.pid());
    let board = run(cmd);
    let board: serde_json::Value =
        serde_json::from_str(&board.stdout).unwrap_or_else(|e| panic!("{e}: {board:?}"));
    let reservations = &board["reservations"];
    // Reservation 1 was never used; 2 was used when the run ended, so its two hours start again.
    assert_eq!(reservations[0]["id"], 1, "{board}");
    assert!(reservations[0].get("used").is_none(), "{board}");
    assert_eq!(reservations[1]["id"], 2, "{board}");
    assert!(
        reservations[1]["used"]
            .as_u64()
            .is_some_and(|used| used >= before),
        "{board}"
    );
}
