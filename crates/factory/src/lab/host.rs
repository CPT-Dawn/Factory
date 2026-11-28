//! What the lab reads from `/proc`: processes, free memory and listening ports. No command
//! runs for any of it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use factory_core::lab::memory::{Memory, ci_headroom_mb};

/// One process, from `/proc/<pid>/stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i64,
    pub comm: String,
    pub ppid: i64,
    pub session: i64,
    /// Field 22, the start time in clock ticks since boot: with the pid, it names one process.
    pub start: String,
    /// It exited and waits for its parent to reap it (state `Z`, or `X` on its way out).
    pub exited: bool,
}

pub fn proc(pid: i64) -> Option<Proc> {
    if pid <= 0 {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_owned();
    // rest[0] is field 3 (state); field n is rest[n - 3].
    let rest: Vec<&str> = stat.get(close + 2..)?.split(' ').collect();
    Some(Proc {
        pid,
        comm,
        ppid: rest.get(1)?.parse().ok()?,
        session: rest.get(3)?.parse().ok()?,
        start: (*rest.get(19)?).to_owned(),
        exited: matches!(*rest.first()?, "Z" | "X"),
    })
}

/// The process recorded as (pid, start) still runs: its pid wasn't reused, and it isn't a
/// zombie its parent has yet to reap.
pub fn alive(pid: i64, start: &str) -> bool {
    proc(pid).is_some_and(|p| p.start == start && !p.exited)
}

/// `MemAvailable` in MB; 0 when it can't be read.
pub fn mem_available_mb() -> i64 {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    text.lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<i64>().ok())
        .map_or(0, |kb| kb / 1024)
}

/// The host's memory for a grant: `MemAvailable`, and the headroom of the CI slice `ci_slice`
/// (a systemd user slice under this user's manager; none for an empty name).
pub fn memory(ci_slice: &str) -> Memory {
    Memory {
        available_mb: mem_available_mb(),
        ci_headroom_mb: ci_headroom(ci_slice),
    }
}

/// The CI slice's headroom in MB, from its cgroup v2 `memory.max` and `memory.current`; none
/// when it doesn't exist.
fn ci_headroom(ci_slice: &str) -> Option<i64> {
    if ci_slice.is_empty() {
        return None;
    }
    let uid = rustix::process::getuid().as_raw();
    let dir = cgroup_root()
        .join(format!("user.slice/user-{uid}.slice/user@{uid}.service"))
        .join(ci_slice);
    let read = |name: &str| fs::read_to_string(dir.join(name)).ok();
    ci_headroom_mb(
        read("memory.max").as_deref(),
        read("memory.current").as_deref(),
    )
}

/// The cgroup v2 tree: `/sys/fs/cgroup`, or `DEVLAB_CGROUP_ROOT` (where tests keep fake files).
fn cgroup_root() -> PathBuf {
    std::env::var_os("DEVLAB_CGROUP_ROOT")
        .map_or_else(|| PathBuf::from("/sys/fs/cgroup"), PathBuf::from)
}

/// TCP ports something listens on, IPv4 and IPv6.
pub fn listening_ports() -> BTreeSet<u32> {
    let mut ports = BTreeSet::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.get(3) != Some(&"0A") {
                continue;
            }
            if let Some(port) = cols
                .get(1)
                .and_then(|local| local.rsplit(':').next())
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            {
                ports.insert(port);
            }
        }
    }
    ports
}
