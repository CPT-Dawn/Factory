//! The host's memory as a grant counts it (#84): what `/proc/meminfo` calls available, less
//! the **CI headroom**, what the CI runners' systemd slice may still grow by before it reaches
//! its `memory.max`. A grant made on memory the runners take back moments later froze the PC.

/// The CI runners' slice when the machine config names none (`ciSlice`).
pub const DEFAULT_CI_SLICE: &str = "peekabus-ci.slice";

/// How far, in MB, a cgroup v2 slice may still grow: its `memory.max` less its
/// `memory.current`, as those files read (bytes; `max` for no limit). None when either is
/// unread: the slice doesn't exist, which counts as no headroom. A slice with no limit gives no
/// number to set aside, and one past its limit has none left: both are 0.
pub fn ci_headroom_mb(max: Option<&str>, current: Option<&str>) -> Option<i64> {
    let (max, current) = (max?.trim(), current?.trim());
    let current: i64 = current.parse().ok()?;
    if max == "max" {
        return Some(0);
    }
    let max: i64 = max.parse().ok()?;
    Some((max - current).max(0) / (1024 * 1024))
}

/// The host's memory for a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Memory {
    /// `MemAvailable`, in MB.
    pub available_mb: i64,
    /// The CI slice's headroom, in MB; none when the slice doesn't exist.
    pub ci_headroom_mb: Option<i64>,
}

impl Memory {
    /// The headroom set aside for CI, in MB: 0 without a slice.
    pub fn headroom_mb(self) -> i64 {
        self.ci_headroom_mb.unwrap_or(0).max(0)
    }

    /// What a grant may use, in MB: available memory less CI's headroom.
    pub fn free_mb(self) -> i64 {
        self.available_mb - self.headroom_mb()
    }
}

/// Where systemd keeps the slice `slice` in the cgroup tree, relative to the manager that owns
/// it: a slice nests in the slice each dash of its name names, so `a-b-c.slice` is
/// `a.slice/a-b.slice/a-b-c.slice` (systemd.slice(5)). A unit that isn't a slice is the
/// manager's direct child.
pub fn slice_path(slice: &str) -> String {
    let Some(stem) = slice
        .strip_suffix(".slice")
        .filter(|s| !s.is_empty() && *s != "-")
    else {
        return slice.to_owned();
    };
    let mut parents = Vec::new();
    let mut name = String::new();
    for part in stem.split('-') {
        if !name.is_empty() {
            name.push('-');
        }
        name.push_str(part);
        parents.push(format!("{name}.slice"));
    }
    parents.join("/")
}

/// MB as GB with one decimal: `12.3`.
pub fn gb(mb: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let gb = mb as f64 / 1024.0;
    format!("{gb:.1}")
}

/// What a wait for memory adds when CI's headroom took part of the free memory it names:
/// empty when it took none, so the reason reads as it always did.
pub fn after_ci(headroom_mb: i64) -> String {
    if headroom_mb > 0 {
        format!(": too little memory after CI's headroom ({headroom_mb} MB)")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1024 * 1024 * 1024;

    #[test]
    fn headroom_is_the_slice_limit_less_what_it_uses() {
        let max = (8 * GIB).to_string();
        let current = (3 * GIB).to_string();
        assert_eq!(
            ci_headroom_mb(Some(&format!("{max}\n")), Some(&format!("{current}\n"))),
            Some(5 * 1024)
        );
    }

    #[test]
    fn a_missing_slice_has_none_and_an_unlimited_or_full_one_has_zero() {
        assert_eq!(ci_headroom_mb(None, None), None);
        assert_eq!(ci_headroom_mb(Some("max\n"), None), None);
        assert_eq!(ci_headroom_mb(Some("max\n"), Some("4096\n")), Some(0));
        assert_eq!(ci_headroom_mb(Some("1048576"), Some("9999999")), Some(0));
    }

    #[test]
    fn free_memory_is_what_is_available_after_ci() {
        let m = Memory {
            available_mb: 12 * 1024,
            ci_headroom_mb: Some(2048),
        };
        assert_eq!(m.free_mb(), 10 * 1024);
        let none = Memory {
            available_mb: 12 * 1024,
            ci_headroom_mb: None,
        };
        assert_eq!(none.free_mb(), 12 * 1024);
    }

    #[test]
    fn a_slice_nests_in_the_slices_its_dashes_name() {
        for (slice, path) in [
            ("peekabus-ci.slice", "peekabus.slice/peekabus-ci.slice"),
            ("a-b-c.slice", "a.slice/a-b.slice/a-b-c.slice"),
            ("ci.slice", "ci.slice"),
            ("-.slice", "-.slice"),
            ("runner.service", "runner.service"),
        ] {
            assert_eq!(slice_path(slice), path, "{slice}");
        }
    }

    #[test]
    fn a_wait_names_cis_headroom_only_when_there_is_some() {
        assert_eq!(after_ci(0), "");
        assert_eq!(
            after_ci(2048),
            ": too little memory after CI's headroom (2048 MB)"
        );
    }
}
