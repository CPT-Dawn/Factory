//! Who holds a lease. Agents run each command in a fresh shell, so a lease belongs to the agent
//! process above that shell (claude, codex), not to the short-lived devlab process.

use super::host::{Proc, proc};
use super::wire::Owner;

/// How far up the process tree the owner is looked for.
const MAX_HOPS: usize = 64;

/// `DEVLAB_OWNER_PID` when it names a live process; else the nearest ancestor whose name is
/// one of `names`; else, for a person's terminal, the session leader (then the parent, then
/// this process), marked not found.
pub fn find_owner(names: &[String]) -> Owner {
    let forced = std::env::var("DEVLAB_OWNER_PID")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|pid| *pid != 0);
    if let Some(p) = forced.and_then(proc) {
        return owner(&p, true);
    }
    let me = proc(i64::from(std::process::id()));
    let from = me.as_ref().map_or(0, |p| p.ppid);
    let mut pid = from;
    for _ in 0..MAX_HOPS {
        if pid <= 1 {
            break;
        }
        let Some(p) = proc(pid) else { break };
        if names.contains(&p.comm) {
            return owner(&p, true);
        }
        pid = p.ppid;
    }
    let leader = me
        .as_ref()
        .and_then(|p| proc(p.session))
        .or_else(|| proc(from))
        .or(me);
    match leader {
        Some(p) => owner(&p, false),
        None => Owner {
            pid: i64::from(std::process::id()),
            start: String::new(),
            label: format!("devlab:{}", std::process::id()),
            found: false,
        },
    }
}

fn owner(p: &Proc, found: bool) -> Owner {
    Owner {
        pid: p.pid,
        start: p.start.clone(),
        label: format!("{}:{}", p.comm, p.pid),
        found,
    }
}
