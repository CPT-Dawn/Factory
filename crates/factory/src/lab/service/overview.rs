//! The lab as `factory status` shows it: leases, slots and queue from the lease store alone.
//! Unlike the lab's own status table it never asks adb, so a call runs no command.

use factory_core::status::{self, Queued};

use super::{Answer, Lab, store};
use crate::lab::reservations;

impl Lab {
    /// The leases, slots and queue now, after the same housekeeping every lab call does (a
    /// dead owner's lease is dropped first).
    pub async fn overview(self: &std::sync::Arc<Self>) -> Answer<status::Lab> {
        let lab = std::sync::Arc::clone(self);
        tokio::task::spawn_blocking(move || lab.overview_now())
            .await
            .unwrap_or_else(|e| Err(super::failed(&e)))
    }

    fn overview_now(&self) -> Answer<status::Lab> {
        self.write(|c, _| {
            let leases = store::leases(c)?;
            let held = reservations::all(c)?;
            let slots = store::slots(c)?
                .into_iter()
                .map(|row| status::Slot {
                    lease: leases
                        .iter()
                        .find(|lease| {
                            lease.project.as_deref() == Some(row.project.as_str())
                                && lease.slot == Some(row.slot)
                        })
                        .map(|lease| lease.id),
                    reservation: held
                        .iter()
                        .find(|r| r.project == row.project && r.slot == row.slot)
                        .map(|r| r.id),
                    project: row.project,
                    slot: row.slot,
                    last_worktree: row.last_worktree,
                    stack_ours: row.stack_ours,
                    stopping: row.stopping,
                    idle_since: row.idle_since,
                })
                .collect();
            let queue = store::waiters(c)?
                .into_iter()
                .map(|waiter| Queued {
                    id: waiter.id,
                    label: waiter.label,
                    project: waiter.project,
                    created: waiter.created,
                    deadline_ms: waiter.deadline_ms,
                })
                .collect();
            let leases = leases
                .into_iter()
                .map(|lease| status::Lease {
                    id: lease.id,
                    project: lease.project,
                    slot: lease.slot,
                    worktree: lease.worktree,
                    owner: lease.owner_label,
                    created: lease.created,
                    touched: lease.touched,
                    devices: lease.devices.into_iter().map(|d| d.serial).collect(),
                })
                .collect();
            let reservations = held
                .into_iter()
                .map(|r| status::Reservation {
                    id: r.id,
                    project: r.project,
                    slot: r.slot,
                    worktree: r.worktree,
                    owner: r.owner_label,
                    created: r.created,
                })
                .collect();
            Ok(status::Lab {
                leases,
                slots,
                reservations,
                queue,
            })
        })
    }
}
