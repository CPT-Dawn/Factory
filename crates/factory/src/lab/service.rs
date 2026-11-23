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
use factory_core::exit::Exit;
use factory_core::lab::grant::{self, Ask, Decision};
use factory_core::lab::js_number;
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
use super::store::{self, Store};
use super::wire::{
    AcquireParams, Board, FindParams, GC, Granted, LEASE, Lease, RELEASE, SLOT, STATUS, SlotParams,
    Waiter,
};
use crate::protocol::{ErrorBody, Outcome, Request, Response};

/// How often a waiting acquire looks again without a change to the board: an owner that exited
/// or a port that freed tells no one (the daemon watches owners itself from #18).
const RECHECK: Duration = Duration::from_secs(3);

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

fn gather(p: &AcquireParams) -> Facts {
    let running = match &p.project {
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
    p.project.as_ref().map(|project| grant::ProjectAsk {
        name: project.name.clone(),
        max_slots: project.max_slots,
        cap: project.cap,
        has_stack: project.stack.is_some(),
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
        let leases = store::leases(c)?;
        let rows = store::slots(c)?;
        let board = grant::Board {
            leases: leases.iter().map(Lease::holding).collect(),
            queue: store::waiters(c)?.iter().map(Waiter::entry).collect(),
            slots: p.project.as_ref().map_or_else(Vec::new, |project| {
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
                                    && r.last_worktree.as_deref() == Some(p.worktree.as_str())
                            }),
                            stack_running: facts
                                .running
                                .get(slot as usize)
                                .copied()
                                .unwrap_or(false),
                            own_ports,
                            stack_ports,
                        }
                    })
                    .collect()
            }),
            listening: facts.listening.clone(),
        };
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
/// then the grant or the error. A client that hangs up leaves the queue.
pub async fn acquire<W, R>(lab: Arc<Lab>, request: Request, writer: &mut W, lines: &mut Lines<R>)
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
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
    let queued = {
        let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
        blocking(move || enqueue(&lab, &p)).await
    };
    let waiter = match queued {
        Ok(waiter) => waiter,
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
    lab.notify();
    let deadline = Instant::now()
        + Duration::try_from_secs_f64(p.wait_sec).unwrap_or(Duration::from_secs(365 * 86400));
    let mut said = String::new();
    loop {
        let mut changed = lab.changed.subscribe();
        let tried = {
            let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
            blocking(move || {
                let facts = gather(&p);
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
                return;
            }
            Ok(Err(why)) => why,
            Err(body) => {
                lab.remove_waiter(waiter);
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
                return;
            }
            said = why;
        }
        tokio::select! {
            _ = changed.changed() => {}
            () = tokio::time::sleep(RECHECK) => {}
            () = tokio::time::sleep_until(deadline) => {}
            line = lines.next_line() => {
                if !matches!(line, Ok(Some(_))) {
                    lab.remove_waiter(waiter);
                    return;
                }
            }
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
