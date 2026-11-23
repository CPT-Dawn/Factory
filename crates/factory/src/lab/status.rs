//! One screen of who has what. Short on purpose: agents read it too.

use std::collections::BTreeMap;

use factory_adapters::systemd::{Systemctl, Units};
use factory_core::lab::ago;
use factory_core::lab::config::MachineConfig;
use factory_core::lab::queue::{Holding, held_by, owner_slot_counts, waiter_capped};
use factory_core::lab::slots::slot_project_id;
use serde::Serialize;

use super::context::{self, project_config};
use super::host::mem_available_mb;
use super::lease::base_name;
use super::stack;
use super::wire::{Board, Lease, STATUS};
use crate::client;
use crate::output::Failure;
use crate::paths::Paths;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    /// Phones and emulators come with #16.
    devices: Vec<()>,
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

pub fn render(json: bool) -> Result<String, Failure> {
    let board: Board = client::call(&Paths::from_env()?, STATUS, &())?;
    let cfg = context::machine_config()?;
    let report = report(&board, &cfg);
    if json {
        return serde_json::to_string_pretty(&report)
            .map_err(|e| Failure::precondition(format!("cannot write the status: {e}")));
    }
    let mut out = vec!["devices: none attached".to_owned()];
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
    let slots = board
        .slots
        .iter()
        .map(|s| {
            let holder = board.leases.iter().find(|l| {
                l.project.as_deref() == Some(s.project.as_str()) && l.slot == Some(s.slot)
            });
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
                    || "free".to_owned(),
                    |l| format!("lease {} {}", l.id, base_name(&l.worktree)),
                ),
                last: s
                    .last_worktree
                    .as_deref()
                    .map_or_else(String::new, |wt| base_name(wt).to_owned()),
            }
        })
        .collect();
    let holdings: Vec<Holding> = board.leases.iter().map(Lease::holding).collect();
    // Each project's cap, read from the config of a worktree that holds one of its slots.
    let mut caps: BTreeMap<String, Option<(u32, u32)>> = BTreeMap::new();
    let mut cap_of = |project: &str| -> Option<(u32, u32)> {
        *caps.entry(project.to_owned()).or_insert_with(|| {
            let l = board
                .leases
                .iter()
                .find(|l| l.project.as_deref() == Some(project))?;
            let p = project_config(&l.worktree).ok().flatten()?;
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
    let waiting = board
        .waiters
        .iter()
        .map(|w| {
            let capped = if waiter_capped(&w.entry(), &held).is_some() {
                " (owner at its slot cap)"
            } else {
                ""
            };
            format!("{} for {}{capped}", w.label, ago(board.now, w.created))
        })
        .collect();
    Report {
        devices: Vec::new(),
        slots,
        owners,
        waiting,
        watcher: Systemctl.is_active("devlab-watch"),
        mem_available_mb: mem_available_mb(),
    }
}
