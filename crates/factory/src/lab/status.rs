//! One screen of who has what. Short on purpose: agents read it too.

use std::collections::BTreeMap;

use factory_adapters::systemd::{Systemctl, Units};
use factory_core::lab::ago;
use factory_core::lab::config::MachineConfig;
use factory_core::lab::devices::{DeviceRow, device_line};
use factory_core::lab::queue::{Holding, held_by, live_queue, owner_slot_counts, waiter_capped};
use factory_core::lab::slots::slot_project_id;
use serde::Serialize;

use super::context::{self, project_config};
use super::host::mem_available_mb;
use super::lease::base_name;
use super::stack;
use super::wire::{Board, DeviceLine, Lease, Reservation, STATUS, SlotRow, Waiter};
use crate::client;
use crate::output::Failure;
use crate::paths::Paths;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    devices: Vec<DeviceLine>,
    slots: Vec<SlotLine>,
    owners: Vec<OwnerLine>,
    waiting: Vec<String>,
    watcher: String,
    #[serde(rename = "memAvailableMB")]
    mem_available_mb: i64,
}

#[derive(Serialize)]
struct SlotLine {
    project: String,
    slot: u32,
    stack: String,
    holder: String,
    last: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnerLine {
    project: String,
    owner: String,
    held: u32,
    cap: Option<u32>,
    max_slots: Option<u32>,
}

/// The devices adb lists as `device`, in the status table's order.
pub fn ready_serials() -> Result<Vec<String>, Failure> {
    Ok(board()?
        .devices
        .into_iter()
        .filter(|d| d.state == "ready")
        .map(|d| d.serial)
        .collect())
}

/// devlab's status: its screen, or with `json` the screen's JSON.
pub fn render(json: bool) -> Result<String, Failure> {
    let board = board()?;
    if json {
        let report = report(&board, &context::machine_config()?);
        return serde_json::to_string_pretty(&report)
            .map_err(|e| Failure::precondition(format!("cannot write the status: {e}")));
    }
    screen(&board)
}

/// The daemon's board.
pub fn board() -> Result<Board, Failure> {
    client::call(&Paths::from_env()?, STATUS, &())
}

/// The board as devlab's status screen shows it.
pub fn screen(board: &Board) -> Result<String, Failure> {
    let report = report(board, &context::machine_config()?);
    let mut out = Vec::new();
    if report.devices.is_empty() {
        out.push("devices: none attached".to_owned());
    } else {
        out.push("devices".to_owned());
        for d in &report.devices {
            out.push(device_line(&DeviceRow {
                serial: d.serial.clone(),
                kind: d.kind.clone(),
                model: d.model.clone(),
                state: d.state.clone(),
                holder: d.holder.clone(),
            }));
        }
    }
    if !report.slots.is_empty() {
        out.push("stack slots".to_owned());
        for s in &report.slots {
            let last = if !s.last.is_empty() && s.holder == "free" {
                format!(" (last: {})", s.last)
            } else {
                String::new()
            };
            out.push(format!(
                "  {} {:<3} {:<28} {}{last}",
                s.project, s.slot, s.stack, s.holder
            ));
        }
        let mut projects: Vec<&str> = Vec::new();
        for o in &report.owners {
            if !projects.contains(&o.project.as_str()) {
                projects.push(&o.project);
            }
        }
        for project in projects {
            let mine: Vec<&OwnerLine> = report
                .owners
                .iter()
                .filter(|o| o.project == project)
                .collect();
            let cap = mine
                .first()
                .and_then(|o| o.cap.zip(o.max_slots))
                .map_or_else(String::new, |(cap, max)| format!(" (cap {cap} of {max})"));
            let held: Vec<String> = mine
                .iter()
                .map(|o| {
                    let at_cap = if o.cap.is_some_and(|cap| o.held >= cap) {
                        " at cap"
                    } else {
                        ""
                    };
                    let cap = o.cap.map_or_else(|| "?".to_owned(), |c| c.to_string());
                    format!("{} {}/{cap}{at_cap}", o.owner, o.held)
                })
                .collect();
            out.push(format!("  {project} per owner{cap}: {}", held.join(", ")));
        }
    }
    if !report.waiting.is_empty() {
        out.push(format!("waiting: {}", report.waiting.join("; ")));
    }
    #[allow(clippy::cast_precision_loss)]
    let gb = report.mem_available_mb as f64 / 1024.0;
    let enable = if report.watcher == "active" {
        ""
    } else {
        " (systemctl --user enable --now devlab-watch)"
    };
    let watcher = if report.watcher.is_empty() {
        "unknown"
    } else {
        &report.watcher
    };
    out.push(format!("memory {gb:.1} GB free; watcher {watcher}{enable}"));
    Ok(out.join("\n"))
}

fn report(board: &Board, cfg: &MachineConfig) -> Report {
    // A reserved slot that was never leased has no row of its own yet.
    let mut rows: Vec<SlotRow> = board.slots.clone();
    for r in &board.reservations {
        if !rows
            .iter()
            .any(|s| s.project == r.project && s.slot == r.slot)
        {
            rows.push(SlotRow {
                project: r.project.clone(),
                slot: r.slot,
                last_worktree: None,
                stack_ours: false,
                idle_since: None,
                stopping: false,
            });
        }
    }
    rows.sort_by(|a, b| (&a.project, a.slot).cmp(&(&b.project, b.slot)));
    let slots = rows
        .iter()
        .map(|s| {
            let holder = board.leases.iter().find(|l| {
                l.project.as_deref() == Some(s.project.as_str()) && l.slot == Some(s.slot)
            });
            let reserved = board
                .reservations
                .iter()
                .find(|r| r.project == s.project && r.slot == s.slot);
            let id = s
                .last_worktree
                .as_deref()
                .and_then(|wt| {
                    let p = project_config(wt).ok().flatten()?;
                    stack::info(&p, wt, s.slot).ok().flatten()
                })
                .map(|info| info.project_id)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| slot_project_id(&s.project, s.slot));
            SlotLine {
                project: s.project.clone(),
                slot: s.slot,
                stack: if stack::running(&id) {
                    format!("up ({id})")
                } else {
                    "down".to_owned()
                },
                holder: holder.map_or_else(
                    || {
                        reserved.map_or_else(
                            || "free".to_owned(),
                            |r| format!("reserved by {} (reservation {})", r.owner_label, r.id),
                        )
                    },
                    |l| format!("lease {} {}", l.id, base_name(&l.worktree)),
                ),
                last: s
                    .last_worktree
                    .as_deref()
                    .map_or_else(String::new, |wt| base_name(wt).to_owned()),
            }
        })
        .collect();
    // A reservation counts against its owner's cap like a lease.
    let holdings: Vec<Holding> = board
        .leases
        .iter()
        .map(Lease::holding)
        .chain(board.reservations.iter().map(Reservation::holding))
        .collect();
    // Each project's cap, read from the config of a worktree that holds one of its slots.
    let mut caps: BTreeMap<String, Option<(u32, u32)>> = BTreeMap::new();
    let mut cap_of = |project: &str| -> Option<(u32, u32)> {
        *caps.entry(project.to_owned()).or_insert_with(|| {
            let worktree = board
                .leases
                .iter()
                .find(|l| l.project.as_deref() == Some(project))
                .map(|l| &l.worktree)
                .or_else(|| {
                    board
                        .reservations
                        .iter()
                        .find(|r| r.project == project)
                        .map(|r| &r.worktree)
                })?;
            let p = project_config(worktree).ok().flatten()?;
            Some((factory_core::lab::config::owner_cap(cfg, &p), p.max_slots))
        })
    };
    let owners = owner_slot_counts(&holdings)
        .into_iter()
        .map(|o| {
            let c = cap_of(&o.project);
            OwnerLine {
                project: o.project,
                owner: o.owner_label,
                held: o.held,
                cap: c.map(|(cap, _)| cap),
                max_slots: c.map(|(_, max)| max),
            }
        })
        .collect();
    let held = held_by(&holdings);
    let queue: Vec<_> = board.waiters.iter().map(Waiter::entry).collect();
    let waiting = board
        .waiters
        .iter()
        .zip(live_queue(&queue, &holdings))
        .map(|(w, entry)| {
            let capped = if waiter_capped(&entry, &held).is_some() {
                " (owner at its slot cap)"
            } else {
                ""
            };
            format!("{} for {}{capped}", w.label, ago(board.now, w.created))
        })
        .collect();
    Report {
        devices: board.devices.clone(),
        slots,
        owners,
        waiting,
        watcher: Systemctl.is_active("devlab-watch"),
        mem_available_mb: mem_available_mb(),
    }
}
