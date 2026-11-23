//! `factory status`: leases, the queue, reviewer quotas, waits, leads and session contexts on one
//! screen, read from the daemon's own state.

use crate::support::Sandbox;

#[test]
fn an_idle_factory_says_so_in_every_section() {
    let sandbox = Sandbox::new();

    let out = sandbox.factory(&["status"]);

    assert_eq!((out.code, out.stderr.as_str()), (0, ""), "{out:?}");
    let lines = out.stdout_lines();
    assert!(lines[0].starts_with("factory status "), "{out:?}");
    let sections: Vec<&str> = lines
        .iter()
        .filter(|line| !line.starts_with(' '))
        .map(|line| line.split([':', ' ']).next().unwrap_or_default())
        .collect();
    assert_eq!(
        sections,
        ["factory", "lab", "review", "waits", "leads"],
        "{out:?}"
    );
    assert!(lines.contains(&"lab: idle"), "{out:?}");
    assert!(lines.contains(&"waits: none"), "{out:?}");
}
