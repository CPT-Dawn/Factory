//! A stack slot's layout. Slot 0 is the project's own config; slot n shifts every port by
//! n * stride and runs its own Supabase project id from its own workdir.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::config::ProjectConfig;

/// The Supabase ports a slot-0 `config.toml` declares, as `<section>.<key>` in file order.
pub type SupaPorts = Vec<(String, u32)>;

const PORT_SECTIONS: [&str; 8] = [
    "api",
    "db",
    "db.pooler",
    "studio",
    "inbucket",
    "local_smtp",
    "analytics",
    "edge_runtime",
];
const PORT_KEYS: [&str; 5] = [
    "port",
    "shadow_port",
    "smtp_port",
    "pop3_port",
    "inspector_port",
];

/// A fixed pattern; the tests below show each one compiles, and one that didn't would match
/// nothing.
type Pattern = LazyLock<Option<Regex>>;

static HEADER: Pattern = LazyLock::new(|| Regex::new(r"^\s*\[([^\]]+)\]").ok());
static PORT_LINE: Pattern =
    LazyLock::new(|| Regex::new(r"^\s*([A-Za-z0-9_]+)\s*=\s*([0-9]+)\s*(#.*)?$").ok());
static PATCH_LINE: Pattern =
    LazyLock::new(|| Regex::new(r"^(\s*)([A-Za-z0-9_]+)(\s*=\s*)([0-9]+)(.*)$").ok());
static PROJECT_ID: Pattern =
    LazyLock::new(|| Regex::new(r#"^(\s*project_id\s*=\s*")([^"]+)(".*)$"#).ok());
static PROJECT_ID_OF: Pattern =
    LazyLock::new(|| Regex::new(r#"^\s*project_id\s*=\s*"([^"]+)""#).ok());
static URL_PORT: Pattern = LazyLock::new(|| Regex::new(r#"(//[^/:"\s]+):([0-9]+)"#).ok());

fn captures<'a>(pattern: &Pattern, line: &'a str) -> Option<Captures<'a>> {
    pattern.as_ref()?.captures(line)
}

fn is_port(section: &str, key: &str) -> bool {
    PORT_SECTIONS.contains(&section) && PORT_KEYS.contains(&key)
}

/// The ports a config declares, by section.
pub fn read_supabase_ports(toml: &str) -> SupaPorts {
    let mut ports: SupaPorts = Vec::new();
    let mut section = String::new();
    for line in toml.split('\n') {
        if let Some(h) = captures(&HEADER, line) {
            section = h[1].trim().to_owned();
            continue;
        }
        let Some(m) = captures(&PORT_LINE, line) else {
            continue;
        };
        let (Some(port), true) = (m[2].parse::<u32>().ok(), is_port(&section, &m[1])) else {
            continue;
        };
        set(&mut ports, format!("{section}.{}", &m[1]), port);
    }
    ports
}

/// The config's `project_id`, if it has one.
pub fn project_id_of(toml: &str) -> Option<String> {
    toml.split('\n')
        .find_map(|line| captures(&PROJECT_ID_OF, line).map(|m| m[1].to_owned()))
}

/// Slot n's Supabase project id: the base for slot 0, else `<base>_s<n>`.
pub fn slot_project_id(base: &str, slot: u32) -> String {
    if slot == 0 {
        base.to_owned()
    } else {
        format!("{base}_s{slot}")
    }
}

/// Slot n's `config.toml`: its own project_id (so its own containers), every local port moved by
/// n * stride, and URLs that point at a project port (an auth hook calling the API) moved too.
/// Commented lines stay as written.
pub fn patch_supabase_config(toml: &str, slot: u32, stride: u32, project_ports: &[u32]) -> String {
    if slot == 0 {
        return toml.to_owned();
    }
    let off = slot * stride;
    let mut section = String::new();
    let lines: Vec<String> = toml
        .split('\n')
        .map(|line| {
            if let Some(h) = captures(&HEADER, line) {
                section = h[1].trim().to_owned();
                return line.to_owned();
            }
            if line.trim_start().starts_with('#') {
                return line.to_owned();
            }
            if let Some(m) = captures(&PROJECT_ID, line) {
                return format!("{}{}{}", &m[1], slot_project_id(&m[2], slot), &m[3]);
            }
            if let Some(m) = captures(&PATCH_LINE, line)
                && is_port(&section, &m[2])
                && let Ok(port) = m[4].parse::<u32>()
            {
                return format!("{}{}{}{}{}", &m[1], &m[2], &m[3], port + off, &m[5]);
            }
            shift_url_ports(line, off, project_ports)
        })
        .collect();
    lines.join("\n")
}

/// `//host:port` followed by `/` or `"`, where the port is a project port, moved by `off`.
fn shift_url_ports(line: &str, off: u32, project_ports: &[u32]) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    let Some(url_port) = URL_PORT.as_ref() else {
        return line.to_owned();
    };
    for m in url_port.captures_iter(line) {
        let (Some(all), Some(host), Some(port)) = (m.get(0), m.get(1), m.get(2)) else {
            continue;
        };
        let followed = matches!(line[all.end()..].chars().next(), Some('/' | '"'));
        let Ok(number) = port.as_str().parse::<u32>() else {
            continue;
        };
        if followed && project_ports.contains(&number) {
            out.push_str(&line[last..all.start()]);
            out.push_str(host.as_str());
            out.push(':');
            out.push_str(&(number + off).to_string());
            last = all.end();
        }
    }
    out.push_str(&line[last..]);
    out
}

/// A slot's stack, as its environment needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackInfo {
    pub ports: SupaPorts,
    pub project_id: String,
    /// Where slot n (n > 0) runs from; none for slot 0, which runs from the worktree.
    pub workdir: Option<String>,
}

/// Exports in order; setting a name again keeps its place.
pub type Env = Vec<(String, String)>;

fn set<T>(env: &mut Vec<(String, T)>, key: String, value: T) {
    match env.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 = value,
        None => env.push((key, value)),
    }
}

fn get<'a>(env: &'a Env, key: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Every exported value for a slot: project ports, the stack's ports and URLs, then the
/// project's own templates.
pub fn slot_env(p: &ProjectConfig, slot: u32, supa: Option<&StackInfo>) -> Env {
    let off = slot * p.slot_stride;
    let mut env: Env = vec![
        ("DEVLAB_PROJECT".to_owned(), p.name.clone()),
        ("DEVLAB_SLOT".to_owned(), slot.to_string()),
    ];
    for (name, port) in &p.ports {
        set(&mut env, name.clone(), (port + off).to_string());
    }
    if let Some(supa) = supa {
        let port = |key: &str| {
            supa.ports
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v + off)
                .filter(|v| *v != 0)
        };
        if let Some(api) = port("api.port") {
            set(&mut env, "SUPABASE_API_PORT".to_owned(), api.to_string());
            set(
                &mut env,
                "SUPABASE_URL".to_owned(),
                format!("http://127.0.0.1:{api}"),
            );
        }
        if let Some(db) = port("db.port") {
            set(&mut env, "SUPABASE_DB_PORT".to_owned(), db.to_string());
            set(
                &mut env,
                "SUPABASE_DB_URL".to_owned(),
                format!("postgresql://postgres:postgres@127.0.0.1:{db}/postgres"),
            );
        }
        if let Some(studio) = port("studio.port") {
            set(
                &mut env,
                "SUPABASE_STUDIO_PORT".to_owned(),
                studio.to_string(),
            );
        }
        if let Some(mail) = port("inbucket.port").or_else(|| port("local_smtp.port")) {
            set(&mut env, "SUPABASE_MAIL_PORT".to_owned(), mail.to_string());
        }
        set(
            &mut env,
            "SUPABASE_PROJECT_ID".to_owned(),
            supa.project_id.clone(),
        );
        set(
            &mut env,
            "SUPABASE_DB_CONTAINER".to_owned(),
            format!("supabase_db_{}", supa.project_id),
        );
        if let Some(workdir) = &supa.workdir {
            set(&mut env, "SUPABASE_WORKDIR".to_owned(), workdir.clone());
        }
    }
    for (name, template) in &p.env {
        let value = expand(template, &env);
        set(&mut env, name.clone(), value);
    }
    env
}

/// `${NAME}` replaced by NAME's exported value, or nothing.
fn expand(template: &str, env: &Env) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(at) = rest.find("${") {
        let after = &rest[at + 2..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        if name_len > 0 && after[name_len..].starts_with('}') {
            out.push_str(&rest[..at]);
            out.push_str(get(env, &after[..name_len]).unwrap_or_default());
            rest = &after[name_len + 1..];
        } else {
            out.push_str(&rest[..at + 2]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The host ports slot n would use: the project's own, and its stack's.
pub fn slot_host_ports(
    project_ports: &[u32],
    stride: u32,
    slot: u32,
    stack_ports: Option<&[u32]>,
) -> (Vec<u32>, Vec<u32>) {
    let off = slot * stride;
    (
        project_ports.iter().map(|p| p + off).collect(),
        stack_ports
            .unwrap_or_default()
            .iter()
            .map(|p| p + off)
            .collect(),
    )
}

/// What [`pick_slot`] knows about one slot of the project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlotView {
    /// A lease holds it.
    pub held: bool,
    /// It last ran this worktree's stack (its migrations are already in).
    pub last_is_mine: bool,
    /// Its stack's database container runs.
    pub stack_running: bool,
    /// Its host ports: the project's own, and its stack's.
    pub own_ports: Vec<u32>,
    pub stack_ports: Vec<u32>,
}

/// The slot to hand out, or why none can be: the free slots tried in order (the same worktree as
/// last time, then a running stack, then the lowest), skipping one whose ports something else
/// listens on. `has_stack` is whether the project runs a stack.
pub fn pick_slot(
    project: &str,
    slots: &[SlotView],
    listening: &BTreeSet<u32>,
    has_stack: bool,
) -> Result<u32, String> {
    let score = |view: &SlotView| {
        if view.last_is_mine {
            0
        } else if has_stack && view.stack_running {
            1
        } else {
            2
        }
    };
    let mut free: Vec<(u32, &SlotView)> =
        (0u32..).zip(slots).filter(|(_, view)| !view.held).collect();
    free.sort_by_key(|(slot, view)| (score(view), *slot));
    let mut busy = Vec::new();
    for (slot, view) in free {
        let squat: Vec<String> = view
            .own_ports
            .iter()
            .filter(|p| listening.contains(p))
            .map(u32::to_string)
            .collect();
        if !squat.is_empty() {
            busy.push(format!("slot {slot}: port {} in use", squat.join(",")));
            continue;
        }
        if has_stack
            && view.stack_ports.iter().any(|p| listening.contains(p))
            && !view.stack_running
        {
            busy.push(format!("slot {slot}: stack ports in use by something else"));
            continue;
        }
        return Ok(slot);
    }
    let blocked = if busy.is_empty() {
        String::new()
    } else {
        format!(" or blocked ({})", busy.join("; "))
    };
    Err(format!(
        "all {} stack slots of {project} are held{blocked}",
        slots.len()
    ))
}

/// A value as a shell word: bare when it is plainly safe, else single-quoted.
pub fn shq(v: &str) -> String {
    let plain = !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c));
    if plain {
        v.to_owned()
    } else {
        format!("'{}'", v.replace('\'', "'\\''"))
    }
}

/// The env as `export NAME=value` lines, without a final newline.
pub fn export_lines(env: &Env) -> String {
    env.iter()
        .map(|(k, v)| format!("export {k}={}", shq(v)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::config::Supabase;

    const TOML: &str = r#"project_id = "PeekaBus"

[api]
enabled = true
port = 54321

[db]
port = 54322
shadow_port = 54320

[db.pooler]
port = 54329

[studio]
port = 54323
api_url = "http://127.0.0.1"

[inbucket]
port = 54324
# smtp_port = 54325

[auth]
site_url = "http://127.0.0.1:3000"

[auth.email.smtp]
port = 587

[auth.hook.send_sms]
uri = "http://host.docker.internal:8787/v1/hooks/send-sms"

[edge_runtime]
inspector_port = 8083

[analytics]
port = 54327
"#;

    fn project() -> ProjectConfig {
        ProjectConfig {
            name: "peekabus".to_owned(),
            max_slots: 3,
            max_slots_per_owner: None,
            slot_stride: 100,
            ports: vec![
                ("API_PORT".to_owned(), 8787),
                ("DRIVER_METRO_PORT".to_owned(), 8871),
            ],
            reverse: Vec::new(),
            supabase: Some(Supabase {
                dir: "supabase".to_owned(),
                cli: "bunx supabase".to_owned(),
            }),
            env: vec![(
                "EXPO_PUBLIC_API_URL".to_owned(),
                "http://127.0.0.1:${API_PORT}".to_owned(),
            )],
        }
    }

    fn values(env: &Env, key: &str) -> Option<String> {
        get(env, key).map(str::to_owned)
    }

    #[test]
    fn every_pattern_compiles() {
        for pattern in [
            &HEADER,
            &PORT_LINE,
            &PATCH_LINE,
            &PROJECT_ID,
            &PROJECT_ID_OF,
            &URL_PORT,
        ] {
            assert!(pattern.is_some());
        }
    }

    #[test]
    fn slot_0_is_the_config_as_written() {
        assert_eq!(patch_supabase_config(TOML, 0, 100, &[8787, 8871]), TOML);
    }

    #[test]
    fn slot_2_gets_its_own_project_id_local_ports_and_hook_url_and_keeps_the_rest() {
        let out = patch_supabase_config(TOML, 2, 100, &[8787, 8871]);
        assert!(out.contains(r#"project_id = "PeekaBus_s2""#));
        for port in [
            "port = 54521",
            "port = 54522",
            "shadow_port = 54520",
            "port = 54529",
            "port = 54523",
            "port = 54524",
            "inspector_port = 8283",
            "port = 54527",
        ] {
            assert!(out.contains(port), "{port}");
        }
        assert!(out.contains("port = 587"));
        assert!(out.contains("# smtp_port = 54325"));
        assert!(out.contains("host.docker.internal:8987/v1/hooks/send-sms"));
        assert!(out.contains(r#"site_url = "http://127.0.0.1:3000""#));
        assert_eq!(out.lines().count(), TOML.lines().count());
    }

    #[test]
    fn ports_are_read_by_section() {
        let want: SupaPorts = [
            ("api.port", 54321),
            ("db.port", 54322),
            ("db.shadow_port", 54320),
            ("db.pooler.port", 54329),
            ("studio.port", 54323),
            ("inbucket.port", 54324),
            ("edge_runtime.inspector_port", 8083),
            ("analytics.port", 54327),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        assert_eq!(read_supabase_ports(TOML), want);
        assert_eq!(project_id_of(TOML).as_deref(), Some("PeekaBus"));
        assert_eq!(project_id_of("[api]\nport = 1\n"), None);
    }

    #[test]
    fn slot_1_has_offset_ports_its_stack_url_and_container_and_expanded_templates() {
        let supa = StackInfo {
            ports: read_supabase_ports(TOML),
            project_id: "PeekaBus_s1".to_owned(),
            workdir: Some("/x/slot-1".to_owned()),
        };
        let env = slot_env(&project(), 1, Some(&supa));
        for (key, value) in [
            ("DEVLAB_SLOT", "1"),
            ("API_PORT", "8887"),
            ("DRIVER_METRO_PORT", "8971"),
            ("SUPABASE_URL", "http://127.0.0.1:54421"),
            ("SUPABASE_DB_PORT", "54422"),
            ("SUPABASE_DB_CONTAINER", "supabase_db_PeekaBus_s1"),
            ("SUPABASE_WORKDIR", "/x/slot-1"),
            ("EXPO_PUBLIC_API_URL", "http://127.0.0.1:8887"),
        ] {
            assert_eq!(values(&env, key).as_deref(), Some(value), "{key}");
        }
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "DEVLAB_PROJECT",
                "DEVLAB_SLOT",
                "API_PORT",
                "DRIVER_METRO_PORT",
                "SUPABASE_API_PORT",
                "SUPABASE_URL",
                "SUPABASE_DB_PORT",
                "SUPABASE_DB_URL",
                "SUPABASE_STUDIO_PORT",
                "SUPABASE_MAIL_PORT",
                "SUPABASE_PROJECT_ID",
                "SUPABASE_DB_CONTAINER",
                "SUPABASE_WORKDIR",
                "EXPO_PUBLIC_API_URL"
            ]
        );
    }

    #[test]
    fn slot_0_sets_no_workdir() {
        let supa = StackInfo {
            ports: read_supabase_ports(TOML),
            project_id: "PeekaBus".to_owned(),
            workdir: None,
        };
        assert_eq!(
            values(&slot_env(&project(), 0, Some(&supa)), "SUPABASE_WORKDIR"),
            None
        );
    }

    #[test]
    fn a_template_naming_nothing_expands_to_nothing() {
        let env: Env = vec![("A".to_owned(), "1".to_owned())];
        assert_eq!(expand("x${A}y${B}z${", &env), "x1yz${");
    }

    #[test]
    fn values_are_quoted_for_the_shell_only_when_they_need_it() {
        assert_eq!(shq("http://127.0.0.1:1/x"), "http://127.0.0.1:1/x");
        assert_eq!(shq(""), "''");
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("it's"), "'it'\\''s'");
    }

    fn view(held: bool, last_is_mine: bool, stack_running: bool, slot: u32) -> SlotView {
        let (own_ports, stack_ports) = slot_host_ports(&[8787], 100, slot, Some(&[54321]));
        SlotView {
            held,
            last_is_mine,
            stack_running,
            own_ports,
            stack_ports,
        }
    }

    #[test]
    fn a_free_slot_goes_to_the_worktree_that_last_ran_it_then_to_a_running_stack_then_the_lowest() {
        let none = BTreeSet::new();
        let slots = [
            view(false, false, false, 0),
            view(false, false, true, 1),
            view(false, true, false, 2),
        ];
        assert_eq!(pick_slot("p", &slots, &none, true), Ok(2));
        assert_eq!(pick_slot("p", &slots[..2], &none, true), Ok(1));
        assert_eq!(pick_slot("p", &slots[..2], &none, false), Ok(0));
    }

    #[test]
    fn a_slot_whose_ports_something_else_holds_is_skipped_and_named() {
        let slots = [view(false, false, false, 0), view(true, false, false, 1)];
        let listening = BTreeSet::from([8787]);
        assert_eq!(
            pick_slot("p", &slots, &listening, true),
            Err("all 2 stack slots of p are held or blocked (slot 0: port 8787 in use)".to_owned())
        );
        let stack = BTreeSet::from([54321]);
        assert_eq!(
            pick_slot("p", &slots, &stack, true),
            Err(
                "all 2 stack slots of p are held or blocked (slot 0: stack ports in use by something else)"
                    .to_owned()
            )
        );
        let ours = [view(false, false, true, 0)];
        assert_eq!(pick_slot("p", &ours, &stack, true), Ok(0));
        assert_eq!(
            pick_slot("p", &[view(true, false, false, 0)], &BTreeSet::new(), true),
            Err("all 1 stack slots of p are held".to_owned())
        );
    }
}
