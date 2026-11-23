//! The lab inside the daemon: the lease store, and acquire's wait in the queue.
//!
//! An acquire gathers what it needs from outside (listening ports, whether each slot's stack
//! runs) first, then decides and writes in one short transaction. It waits on a change to the
//! board (a grant, a release, a waiter leaving) or the re-check tick, never in a sleep loop of
//! the CLI's.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use factory_adapters::docker::{Containers, Docker};
use factory_core::event::{Event, Fields};
use factory_core::exit::Exit;
use factory_core::lab::grant::{self, Ask, Decision};
use factory_core::lab::js_number;
use factory_core::lab::run::{End, Phase};
use factory_core::lab::slots::{SlotView, slot_host_ports, slot_project_id};
use rusqlite::Connection;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, Lines};
use tokio::sync::watch;
use tokio::time::Instant;

use super::host;
use super::lease::base_name;
use super::reservations;
use super::store::{self, Store};
use super::wire::{
    AcquireParams, Board, FindParams, GC, Granted, LEASE, Lease, ProjectAsk, RELEASE, RUN_END,
    RUN_START, RunAsk, RunEnd, SLOT, STATUS, SlotParams, UNRESERVE, Waiter,
};
use crate::daemon::Ledger;
use crate::protocol::{ErrorBody, Outcome, Request, Response};

/// How often a waiting acquire looks again without a change to the board: an owner that exited
/// or a port that freed tells no one (the daemon watches owners itself from #18).
const RECHECK: Duration = Duration::from_secs(3);

mod reserve;
pub use reserve::reserve;

pub struct Lab {
    path: PathBuf,
    store: Mutex<Store>,
    /// Bumped on every change a waiter could be waiting for.
    changed: watch::Sender<u64>,
}

type Answer<T> = Result<T, ErrorBody>;

impl Lab {
    pub fn open(path: &Path) -> Result<Lab, String> {
        let store =
            Store::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Ok(Lab {
            path: path.to_owned(),
            store: Mutex::new(store),
            changed: watch::Sender::new(0),
        })
    }

    /// One write transaction; dead owners' leases are dropped first, telling waiters.
    fn write<T>(&self, work: impl FnOnce(&Connection, i64) -> rusqlite::Result<T>) -> Answer<T> {
        self.write_gc(work).map(|(_, out)| out)
    }

    /// [`Lab::write`], with a line for each lease of an owner that exited.
    fn write_gc<T>(
        &self,
        work: impl FnOnce(&Connection, i64) -> rusqlite::Result<T>,
    ) -> Answer<(Vec<String>, T)> {
        let mut store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = now();
        let (freed, out) = store
            .write(|c| {
                let freed = store::gc(c, now, &host::alive)?;
                Ok((freed, work(c, now)?))
            })
            .map_err(|e| ErrorBody {
                code: Exit::Precondition.code(),
                message: format!("lab store {}: {e}; retry", self.path.display()),
            })?;
        if !freed.is_empty() {
            self.notify();
        }
        Ok((freed, out))
    }

    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn drop_unanswered(&self, lease: &Lease) {
        if self
            .write(|c, now| store::drop_lease(c, lease, now))
            .is_ok()
        {
            self.notify();
        }
    }

    fn remove_waiter(&self, id: i64) {
        if let Ok(true) = self.write(|c, _| store::delete_waiter(c, id)) {
            self.notify();
        }
    }
}

/// Answers a lab method that doesn't wait; none for a method that isn't the lab's.
pub async fn respond(lab: &Arc<Lab>, method: &str, params: Value) -> Option<Outcome> {
    let work: fn(&Lab, Value) -> Answer<Value> = match method {
        LEASE => |lab, p| reply(lab_lease(lab, parse(p)?)),
        RELEASE => |lab, p| reply(release(lab, parse(p)?)),
        GC => |lab, _| reply(lab.write_gc(|_, _| Ok(())).map(|(freed, ())| freed)),
        STATUS => |lab, _| reply(status(lab)),
        SLOT => |lab, p| reply(slot(lab, parse(p)?)),
        UNRESERVE => |lab, p| reply(reserve::unreserve(lab, parse(p)?)),
        _ => return None,
    };
    let lab = Arc::clone(lab);
    let answer = tokio::task::spawn_blocking(move || work(&lab, params))
        .await
        .unwrap_or_else(|e| Err(failed(&e)));
    Some(match answer {
        Ok(value) => Outcome::Result(value),
        Err(body) => Outcome::Error(body),
    })
}

fn parse<T: DeserializeOwned>(params: Value) -> Answer<T> {
    serde_json::from_value(params).map_err(|e| ErrorBody {
        code: Exit::Usage.code(),
        message: format!(
            "bad lab params: {e}; the daemon may be older than this factory: restart it"
        ),
    })
}

fn reply<T: Serialize>(answer: Answer<T>) -> Answer<Value> {
    answer.map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
}

fn failed(e: &dyn std::fmt::Display) -> ErrorBody {
    ErrorBody {
        code: Exit::Precondition.code(),
        message: format!("the lab failed in the daemon: {e}; retry"),
    }
}

fn lab_lease(lab: &Lab, p: FindParams) -> Answer<Option<Lease>> {
    lab.write(|c, now| {
        let lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        if let Some(lease) = &lease {
            store::touch(c, lease.id, now)?;
        }
        Ok(lease)
    })
}

fn release(lab: &Lab, p: FindParams) -> Answer<Option<Lease>> {
    let dropped = lab.write(|c, now| {
        let lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        if let Some(lease) = &lease {
            store::drop_lease(c, lease, now)?;
        }
        Ok(lease)
    })?;
    if dropped.is_some() {
        lab.notify();
    }
    Ok(dropped)
}

fn status(lab: &Lab) -> Answer<Board> {
    lab.write(|c, now| {
        Ok(Board {
            now,
            leases: store::leases(c)?,
            slots: store::slots(c)?,
            waiters: store::waiters(c)?,
            reservations: reservations::all(c)?,
        })
    })
}

fn slot(lab: &Lab, p: SlotParams) -> Answer<()> {
    lab.write(|c, _| store::set_stack_ours(c, &p.project, p.slot, p.stack_ours))
}

/// What a grant needs from outside the store, gathered before its transaction.
struct Facts {
    listening: BTreeSet<u32>,
    /// Whether each slot's stack runs, by slot.
    running: Vec<bool>,
}

fn gather(project: Option<&ProjectAsk>) -> Facts {
    let running = match project {
        Some(project) => match &project.stack {
            Some(stack) => (0..project.max_slots)
                .map(|slot| {
                    Docker.running(&format!(
                        "supabase_db_{}",
                        slot_project_id(&stack.project_id, slot)
                    ))
                })
                .collect(),
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    Facts {
        listening: host::listening_ports(),
        running,
    }
}

fn enqueue(lab: &Lab, p: &AcquireParams) -> Answer<i64> {
    lab.write(|c, now| {
        let existing = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        let ask = project_ask(p);
        let name = base_name(&p.worktree);
        store::insert_waiter(
            c,
            &Waiter {
                id: 0,
                created: now,
                label: format!("{} @ {name}", p.owner.label),
                owner: p.owner.key(),
                owner_label: p.owner.label.clone(),
                project: p.project.as_ref().map(|project| project.name.clone()),
                slot_cap: grant::request_cap(ask.as_ref(), existing.is_some()),
                max_slots: p.project.as_ref().map_or(0, |project| project.max_slots),
            },
        )
    })
}

fn project_ask(p: &AcquireParams) -> Option<grant::ProjectAsk> {
    p.project.as_ref().map(grant_project)
}

fn grant_project(project: &ProjectAsk) -> grant::ProjectAsk {
    grant::ProjectAsk {
        name: project.name.clone(),
        max_slots: project.max_slots,
        cap: project.cap,
        has_stack: project.stack.is_some(),
    }
}

/// The board a request is decided on: the leases and the reservations (both hold a slot of their
/// owner's), the queue, and the asker's project's slots.
fn grant_board(
    c: &Connection,
    project: Option<&ProjectAsk>,
    worktree: &str,
    facts: &Facts,
) -> rusqlite::Result<grant::Board> {
    let mut leases: Vec<_> = store::leases(c)?.iter().map(Lease::holding).collect();
    leases.extend(reservations::all(c)?.iter().map(|r| r.holding()));
    let rows = store::slots(c)?;
    Ok(grant::Board {
        leases,
        queue: store::waiters(c)?.iter().map(Waiter::entry).collect(),
        slots: project.map_or_else(Vec::new, |project| {
            (0..project.max_slots)
                .map(|slot| {
                    let (own_ports, stack_ports) = slot_host_ports(
                        &project.ports,
                        project.stride,
                        slot,
                        project.stack.as_ref().map(|s| s.ports.as_slice()),
                    );
                    SlotView {
                        held: false,
                        last_is_mine: rows.iter().any(|r| {
                            r.project == project.name
                                && r.slot == slot
                                && r.last_worktree.as_deref() == Some(worktree)
                        }),
                        stack_running: facts.running.get(slot as usize).copied().unwrap_or(false),
                        own_ports,
                        stack_ports,
                    }
                })
                .collect()
        }),
        listening: facts.listening.clone(),
    })
}

/// The lease granted, or why the request waits.
fn try_grant(
    lab: &Lab,
    p: &AcquireParams,
    waiter: i64,
    facts: &Facts,
) -> Answer<Result<(Granted, bool), String>> {
    lab.write(|c, now| {
        let lease = store::find_lease(c, &p.worktree, &p.owner, p.lease)?;
        // A reservation of the caller's project is used once: it becomes the caller's lease,
        // without a turn in the queue. One that is gone (used, released or its lead exited)
        // asks for a slot the normal way.
        if lease.is_none()
            && let (Some(id), Some(project)) = (p.reservation, &p.project)
            && let Some(granted) =
                reservations::claim(c, id, &project.name, &p.worktree, &p.owner, now)?
        {
            store::touch(c, granted.lease.id, now)?;
            store::delete_waiter(c, waiter)?;
            return Ok(Ok((granted, true)));
        }
        let board = grant_board(c, p.project.as_ref(), &p.worktree, facts)?;
        let ask = Ask {
            waiter,
            project: project_ask(p),
            existing: lease.as_ref().map(|l| l.slot),
        };
        let (slot, new) = match grant::decide(&ask, &board, now) {
            Decision::Wait(why) => return Ok(Err(why)),
            Decision::Grant { slot, new } => (slot, new),
        };
        let fresh = new || lease.is_none();
        let lease = match lease {
            Some(lease) if !new => lease,
            _ => store::insert_lease(
                c,
                p.project.as_ref().map(|project| project.name.as_str()),
                &p.worktree,
                &p.owner,
                slot,
                now,
            )?,
        };
        let previous = match (&p.project, lease.slot) {
            (Some(project), Some(slot)) => store::claim_slot(c, &project.name, slot, &p.worktree)?,
            _ => None,
        };
        store::touch(c, lease.id, now)?;
        store::delete_waiter(c, waiter)?;
        Ok(Ok((Granted { lease, previous }, fresh)))
    })
}

/// Serves one `lab.acquire`: queues it, writes a note each time the reason it waits changes,
/// then the grant or the error. A client that hangs up leaves the queue. A run's acquire is then
/// followed to its end, which lands in the ledger.
pub async fn acquire<W, R, S>(
    lab: Arc<Lab>,
    ledger: Arc<Ledger>,
    request: Request,
    writer: &mut W,
    lines: &mut Lines<R>,
    stopping: S,
) where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    S: Future<Output = ()>,
{
    let id = Some(request.id);
    let p: AcquireParams = match parse(request.params) {
        Ok(p) => p,
        Err(body) => {
            send(
                writer,
                Response {
                    id,
                    outcome: Outcome::Error(body),
                },
            )
            .await;
            return;
        }
    };
    if !p.owner.found {
        let _ = writeln!(
            std::io::stderr(),
            "factory: lab: warning: the devlab call in {} has no claude or codex ancestor; {} owns its lease",
            p.worktree,
            p.owner.label
        );
    }
    let p = Arc::new(p);
    let mut run = p.run.clone().map(|ask| Run {
        ask,
        worktree: p.worktree.clone(),
        ledger,
        queued: Instant::now(),
        granted: None,
        started: None,
        lease: None,
    });
    let queued = {
        let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
        blocking(move || enqueue(&lab, &p)).await
    };
    let waiter = match queued {
        Ok(waiter) => waiter,
        Err(body) => {
            if let Some(run) = &run {
                run.record(End::Exit(body.code).outcome(), Phase::Queued.word())
                    .await;
            }
            send(
                writer,
                Response {
                    id,
                    outcome: Outcome::Error(body),
                },
            )
            .await;
            return;
        }
    };
    lab.notify();
    let year = Duration::from_secs(365 * 86400);
    let wait = Duration::try_from_secs_f64(p.wait_sec).unwrap_or(year);
    let now = Instant::now();
    let deadline = now
        .checked_add(wait)
        .or_else(|| now.checked_add(year))
        .unwrap_or(now);
    let mut said = String::new();
    loop {
        let mut changed = lab.changed.subscribe();
        let tried = {
            let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
            blocking(move || {
                let facts = gather(p.project.as_ref());
                try_grant(&lab, &p, waiter, &facts)
            })
            .await
        };
        let why = match tried {
            Ok(Ok((granted, fresh))) => {
                lab.notify();
                let lease = granted.lease.clone();
                let value = serde_json::to_value(granted).unwrap_or(Value::Null);
                let answered = send(
                    writer,
                    Response {
                        id,
                        outcome: Outcome::Result(value),
                    },
                )
                .await;
                // A caller gone before its answer never learns of the lease: give it back.
                if !answered && fresh {
                    lab.drop_unanswered(&lease);
                }
                if let Some(mut run) = run {
                    run.granted = Some(Instant::now());
                    if answered {
                        run.lease = Some(lease);
                        run.follow(lines, stopping).await;
                    } else {
                        run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                    }
                }
                return;
            }
            Ok(Err(why)) => why,
            Err(body) => {
                lab.remove_waiter(waiter);
                if let Some(run) = &run {
                    run.record(End::Exit(body.code).outcome(), Phase::Queued.word())
                        .await;
                }
                send(
                    writer,
                    Response {
                        id,
                        outcome: Outcome::Error(body),
                    },
                )
                .await;
                return;
            }
        };
        if Instant::now() >= deadline {
            lab.remove_waiter(waiter);
            let body = ErrorBody {
                code: Exit::Busy.code(),
                message: format!("gave up after {} s: {why}", js_number(p.wait_sec)),
            };
            if let Some(run) = &run {
                run.record(End::Exit(body.code).outcome(), Phase::Queued.word())
                    .await;
            }
            send(
                writer,
                Response {
                    id,
                    outcome: Outcome::Error(body),
                },
            )
            .await;
            return;
        }
        if why != said {
            let note = Response {
                id,
                outcome: Outcome::Note(format!("waiting: {why}")),
            };
            if !send(writer, note).await {
                lab.remove_waiter(waiter);
                if let Some(run) = &run {
                    run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                }
                return;
            }
            said = why;
        }
        tokio::select! {
            _ = changed.changed() => {}
            () = tokio::time::sleep(RECHECK) => {}
            () = tokio::time::sleep_until(deadline) => {}
            line = lines.next_line() => {
                let Ok(Some(line)) = line else {
                    lab.remove_waiter(waiter);
                    if let Some(run) = &run {
                        run.record(End::Lost.outcome(), Phase::Queued.word()).await;
                    }
                    return;
                };
                if let Some(run) = &mut run
                    && let Some(end) = run_end(&line)
                {
                    lab.remove_waiter(waiter);
                    run.record(end.outcome, &end.phase).await;
                    let body = ErrorBody {
                        code: Exit::Precondition.code(),
                        message: "the run was stopped while queued".to_owned(),
                    };
                    send(
                        writer,
                        Response {
                            id,
                            outcome: Outcome::Error(body),
                        },
                    )
                    .await;
                    return;
                }
            }
        }
    }
}

/// A run's end, if `line` is one.
fn run_end(line: &str) -> Option<RunEnd> {
    let request: Request = serde_json::from_str(line).ok()?;
    (request.method == RUN_END)
        .then(|| serde_json::from_value(request.params).ok())
        .flatten()
}

/// A `devlab run` the daemon follows, from its acquire to its end.
struct Run {
    ask: RunAsk,
    worktree: String,
    ledger: Arc<Ledger>,
    queued: Instant,
    granted: Option<Instant>,
    started: Option<Instant>,
    lease: Option<Lease>,
}

impl Run {
    /// Reads the run's connection after the grant: its start, then its end, or the connection
    /// closing without one, which is a lost run. A daemon stopping under a run loses it too: the
    /// run may outlast any drain, and its end would reach no one.
    async fn follow<R, S>(&mut self, lines: &mut Lines<R>, stopping: S)
    where
        R: AsyncBufRead + Unpin,
        S: Future<Output = ()>,
    {
        tokio::pin!(stopping);
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line,
                () = &mut stopping => Ok(None),
            };
            let Ok(Some(line)) = line else {
                let phase = if self.started.is_some() {
                    Phase::Running
                } else {
                    Phase::Queued
                };
                self.record(End::Lost.outcome(), phase.word()).await;
                return;
            };
            if let Some(end) = run_end(&line) {
                self.record(end.outcome, &end.phase).await;
                return;
            }
            if serde_json::from_str::<Request>(&line).is_ok_and(|r| r.method == RUN_START) {
                self.started.get_or_insert_with(Instant::now);
            }
        }
    }

    /// Writes the run's `lab-run` event.
    async fn record(&self, outcome: String, phase: &str) {
        let now = Instant::now();
        let queued_until = self.granted.unwrap_or(now);
        let mut fields = Fields::new();
        fields.insert("worktree".to_owned(), self.worktree.clone().into());
        if let Some(lease) = &self.lease {
            fields.insert("lease".to_owned(), lease.id.into());
            if let Some(slot) = lease.slot {
                fields.insert("slot".to_owned(), slot.into());
            }
        }
        fields.insert("command".to_owned(), self.ask.command.clone().into());
        fields.insert(
            "queue_s".to_owned(),
            queued_until.duration_since(self.queued).as_secs().into(),
        );
        fields.insert(
            "run_s".to_owned(),
            self.started
                .map_or(0, |started| now.duration_since(started).as_secs())
                .into(),
        );
        fields.insert("outcome".to_owned(), outcome.into());
        fields.insert("phase".to_owned(), phase.into());
        if let Some(session) = &self.ask.session {
            fields.insert("session".to_owned(), session.clone().into());
        }
        let Ok(event) = Event::new("lab-run", fields) else {
            return;
        };
        let ledger = Arc::clone(&self.ledger);
        let landed = tokio::task::spawn_blocking(move || ledger.append(&event)).await;
        if !matches!(landed, Ok(Ok(_))) {
            let _ = writeln!(
                std::io::stderr(),
                "factory: lab: cannot record a run of {} in the ledger",
                self.worktree
            );
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Answer<T> + Send + 'static,
) -> Answer<T> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(failed(&e)))
}

/// Writes one response line; false when the client is gone.
async fn send<W: AsyncWrite + Unpin>(writer: &mut W, response: Response) -> bool {
    let Ok(mut line) = serde_json::to_string(&response) else {
        return false;
    };
    line.push('\n');
    writer.write_all(line.as_bytes()).await.is_ok()
}

/// Seconds since the epoch.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
