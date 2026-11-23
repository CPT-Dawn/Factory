//! `lab.reserve`: a lead sets slots aside for its implementers, and gives them back.
//!
//! A reserve queues like an acquire, in the same FIFO, but an owner with no room under its cap is
//! refused at once with exit 75 and an ETA instead of waiting for one of its own leases to end.
//! It says one thing while it waits, why, and then its answer.

use std::io::Write;
use std::sync::Arc;

use factory_core::exit::Exit;
use factory_core::lab::args::wait_span;
use factory_core::lab::js_number;
use factory_core::lab::reserve::{Plan, Reserve, eta_secs, over_cap_line, plan};
use rusqlite::Connection;
use tokio::io::{AsyncBufRead, AsyncWrite, Lines};
use tokio::time::Instant;

use super::{
    Answer, Facts, Lab, Present, RECHECK, blocking, gather_stack, grant_board, grant_project,
    lock_ignoring_poison, parse, send,
};
use crate::lab::reservations;
use crate::lab::store;
use crate::lab::wire::{ReserveParams, Reserved, UnreserveParams};
use crate::protocol::{ErrorBody, Outcome, Request, Response};
use factory_core::lab::base_name;

/// What one look at the board came to.
enum Tried {
    Granted(Reserved),
    Wait(String),
    /// The refusal, with its ETA.
    OverCap(String),
}

/// Gives the owner's reservations of a project back; how many there were.
pub(super) fn unreserve(lab: &Lab, p: UnreserveParams) -> Answer<usize> {
    let freed = lab.write(|c, now| reservations::release(c, &p.owner, &p.project, now))?;
    if freed > 0 {
        lab.notify();
    }
    Ok(freed)
}

fn enqueue(lab: &Lab, p: &ReserveParams) -> Answer<i64> {
    lab.write(|c, now| {
        let id = store::insert_waiter(
            c,
            &crate::lab::wire::Waiter {
                id: 0,
                created: now,
                label: format!("{} @ {}", p.owner.label, base_name(&p.worktree)),
                owner: p.owner.key(),
                owner_label: p.owner.label.clone(),
                worktree: p.worktree.clone(),
                lease: None,
                project: Some(p.project.name.clone()),
                slot_cap: Some(p.project.cap),
                max_slots: p.project.max_slots,
                // The reserve call lives and dies with its connection, so it keeps no place.
                deadline_ms: None,
                client_pid: None,
                client_start: None,
            },
        )?;
        lab.callers().insert(id);
        Ok(id)
    })
}

fn try_reserve(lab: &Lab, p: &ReserveParams, waiter: i64, facts: &Facts) -> Answer<Tried> {
    lab.write(|c, now| {
        let stopping = lock_ignoring_poison(&lab.stopping).clone();
        let board = grant_board(c, Some(&p.project), &p.worktree, facts, &stopping)?;
        let ask = Reserve {
            waiter,
            project: grant_project(&p.project),
            owner: p.owner.key(),
            owner_label: p.owner.label.clone(),
            want: p.slots,
        };
        match plan(&ask, &board, now) {
            Plan::Wait(why) => Ok(Tried::Wait(why)),
            Plan::Granted(slots) if slots.is_empty() => {
                Ok(Tried::Wait("no stack slot is free".to_owned()))
            }
            Plan::Granted(slots) => {
                let mut made = Vec::new();
                for slot in slots {
                    made.push(reservations::insert(
                        c,
                        &p.project.name,
                        slot,
                        &p.worktree,
                        &p.owner,
                        now,
                    )?);
                }
                store::delete_waiter(c, waiter)?;
                Ok(Tried::Granted(Reserved {
                    project: p.project.name.clone(),
                    requested: p.slots,
                    cap: p.project.cap,
                    reservations: made,
                }))
            }
            Plan::OverCap(over) => {
                store::delete_waiter(c, waiter)?;
                Ok(Tried::OverCap(over_cap_message(c, &over, p, now)?))
            }
        }
    })
}

/// The refusal line: the cap, and when a slot of the owner's own is likely to free.
fn over_cap_message(
    c: &Connection,
    over: &factory_core::lab::reserve::OverCap,
    p: &ReserveParams,
    now: i64,
) -> rusqlite::Result<String> {
    let key = p.owner.key();
    let mut ages: Vec<i64> = store::leases(c)?
        .iter()
        .filter(|l| l.project.as_deref() == Some(p.project.name.as_str()) && l.owner_key() == key)
        .map(|l| now - l.created)
        .collect();
    ages.extend(
        reservations::all(c)?
            .iter()
            .filter(|r| r.project == p.project.name && r.owner_key() == key)
            .map(|r| now - r.created),
    );
    let eta = eta_secs(&reservations::durations(c, &p.project.name)?, &ages);
    Ok(over_cap_line(over, eta))
}

async fn answer<W: AsyncWrite + Unpin>(writer: &mut W, id: Option<u64>, outcome: Outcome) -> bool {
    send(writer, Response { id, outcome }).await
}

/// Serves one `lab.reserve`. A client that hangs up leaves the queue.
pub async fn reserve<W, R>(lab: Arc<Lab>, request: Request, writer: &mut W, lines: &mut Lines<R>)
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let id = Some(request.id);
    let p: ReserveParams = match parse(request.params) {
        Ok(p) => p,
        Err(body) => {
            answer(writer, id, Outcome::Error(body)).await;
            return;
        }
    };
    if !p.owner.found {
        let _ = writeln!(
            std::io::stderr(),
            "factory: lab: warning: the devlab call in {} has no claude or codex ancestor; {} owns its reservation",
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
            answer(writer, id, Outcome::Error(body)).await;
            return;
        }
    };
    let _attached = Present(Arc::clone(&lab), waiter);
    lab.notify();
    let now = Instant::now();
    let deadline = now.checked_add(wait_span(p.wait_sec)).unwrap_or(now);
    let mut said = false;
    loop {
        let mut changed = lab.changed.subscribe();
        let tried = {
            let (lab, p) = (Arc::clone(&lab), Arc::clone(&p));
            blocking(move || {
                let facts = gather_stack(Some(&p.project));
                try_reserve(&lab, &p, waiter, &facts)
            })
            .await
        };
        let why = match tried {
            Ok(Tried::Granted(reserved)) => {
                lab.notify();
                let ids: Vec<i64> = reserved.reservations.iter().map(|r| r.id).collect();
                let value = serde_json::to_value(reserved).unwrap_or(serde_json::Value::Null);
                // A caller gone before its answer never learns of its reservations: give them back.
                if !answer(writer, id, Outcome::Result(value)).await {
                    let owner = Arc::clone(&lab);
                    let _ = blocking(move || {
                        owner.write(|c, now| {
                            for id in ids {
                                reservations::drop_by_id(c, id, now)?;
                            }
                            Ok(())
                        })
                    })
                    .await;
                    lab.notify();
                }
                return;
            }
            Ok(Tried::OverCap(message)) => {
                lab.notify();
                let body = ErrorBody {
                    code: Exit::Busy.code(),
                    message,
                };
                answer(writer, id, Outcome::Error(body)).await;
                return;
            }
            Ok(Tried::Wait(why)) => why,
            Err(body) => {
                lab.remove_waiter(waiter);
                answer(writer, id, Outcome::Error(body)).await;
                return;
            }
        };
        if Instant::now() >= deadline {
            lab.remove_waiter(waiter);
            let body = ErrorBody {
                code: Exit::Busy.code(),
                message: format!("gave up after {} s: {why}", js_number(p.wait_sec)),
            };
            answer(writer, id, Outcome::Error(body)).await;
            return;
        }
        if !said {
            if !answer(writer, id, Outcome::Note(format!("waiting: {why}"))).await {
                lab.remove_waiter(waiter);
                return;
            }
            said = true;
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
