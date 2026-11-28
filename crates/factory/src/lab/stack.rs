//! One local Supabase stack per slot. Slot 0 is the project's own config; slot n runs from
//! `<devlab state>/<project>/slot-n/supabase`: the worktree's files linked in, plus a
//! `config.toml` with the slot's own project id and ports. No stack command runs inside a
//! store transaction: these all run in the CLI.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use factory_adapters::docker::{Containers, Docker};
use factory_adapters::supabase::{Stacks, SupabaseCli};
use factory_core::exit::Exit;
use factory_core::lab::config::{MachineConfig, ProjectConfig};
use factory_core::lab::memory::after_ci;
use factory_core::lab::slots::{
    Env, StackInfo, patch_supabase_config, project_id_of, read_supabase_ports, slot_project_id,
};

use super::context::{devlab_state, project_config};
use super::host;
use crate::output::Failure;

/// Files of the worktree's supabase dir a slot's workdir doesn't link.
const SKIP: [&str; 3] = ["config.toml", ".temp", ".branches"];

/// The CLI when devlab.json names none.
const DEFAULT_CLI: &str = "bunx supabase";

/// The slot's stack: none when the project has none.
pub fn info(p: &ProjectConfig, worktree: &str, slot: u32) -> Result<Option<StackInfo>, Failure> {
    let Some(supabase) = &p.supabase else {
        return Ok(None);
    };
    let cfg = format!("{worktree}/{}/config.toml", supabase.dir);
    let Ok(toml) = fs::read_to_string(&cfg) else {
        return Err(Failure::new(
            Exit::Precondition,
            format!("{cfg} not found (devlab.json \"supabase\")"),
        ));
    };
    let Some(base) = project_id_of(&toml) else {
        return Err(Failure::new(
            Exit::Precondition,
            format!("{cfg} has no project_id"),
        ));
    };
    Ok(Some(StackInfo {
        ports: read_supabase_ports(&toml),
        project_id: slot_project_id(&base, slot),
        workdir: (slot != 0).then(|| {
            devlab_state()
                .join(&p.name)
                .join(format!("slot-{slot}"))
                .to_string_lossy()
                .into_owned()
        }),
    }))
}

fn io_failure(what: &Path, e: &std::io::Error) -> Failure {
    Failure::precondition(format!("{}: {e}", what.display()))
}

/// (Re)links slot n's workdir to this worktree's supabase files and writes its config.
pub fn prepare_workdir(p: &ProjectConfig, worktree: &str, slot: u32) -> Result<(), Failure> {
    let Some(supabase) = &p.supabase else {
        return Ok(());
    };
    if slot == 0 {
        return Ok(());
    }
    let src = Path::new(worktree).join(&supabase.dir);
    let dst = devlab_state()
        .join(&p.name)
        .join(format!("slot-{slot}"))
        .join("supabase");
    fs::create_dir_all(&dst).map_err(|e| io_failure(&dst, &e))?;
    let names = |dir: &Path| -> Result<Vec<String>, Failure> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).map_err(|e| io_failure(dir, &e))? {
            let entry = entry.map_err(|e| io_failure(dir, &e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !SKIP.contains(&name.as_str()) {
                out.push(name);
            }
        }
        Ok(out)
    };
    let want = names(&src)?;
    for name in names(&dst)? {
        let at = dst.join(&name);
        let is_link = fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink());
        if is_link && (!want.contains(&name) || fs::read_link(&at).ok() != Some(src.join(&name))) {
            fs::remove_file(&at).map_err(|e| io_failure(&at, &e))?;
        }
    }
    for name in &want {
        let at = dst.join(name);
        if !at.exists() && fs::symlink_metadata(&at).is_err() {
            symlink(src.join(name), &at).map_err(|e| io_failure(&at, &e))?;
        }
    }
    let config = src.join("config.toml");
    let toml = fs::read_to_string(&config).map_err(|e| io_failure(&config, &e))?;
    let ports: Vec<u32> = p.ports.iter().map(|(_, port)| *port).collect();
    let patched = patch_supabase_config(&toml, slot, p.slot_stride, &ports);
    let out = dst.join("config.toml");
    if fs::read_to_string(&out).ok().as_deref() != Some(patched.as_str()) {
        fs::write(&out, patched).map_err(|e| io_failure(&out, &e))?;
    }
    Ok(())
}

/// The newest migration of the worktree's supabase project: of the files named as the supabase
/// CLI names one, `<version>_<name>.sql` with an all-digit version, the one with the greatest
/// version. None when it has none, or when its devlab.json is gone or names no supabase dir.
/// It reads files and runs no command, so the daemon's store reads it inside a transaction.
pub fn migration_head(worktree: &str) -> Option<String> {
    let p = project_config(worktree).ok()??;
    let dir = Path::new(worktree)
        .join(&p.supabase?.dir)
        .join("migrations");
    fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            let (version, rest) = name.split_once('_')?;
            let migration = !version.is_empty()
                && version.bytes().all(|b| b.is_ascii_digit())
                && rest.ends_with(".sql")
                && entry.file_type().ok()?.is_file();
            // Versions compare as numbers: past its leading zeros, a longer one is greater.
            let digits = version.trim_start_matches('0');
            let key = (digits.len(), digits.to_owned());
            migration.then_some((key, name))
        })
        .max()
        .map(|(_, name)| name)
}

/// The project id of slot `slot` of `project`'s stack: as its last worktree's supabase config
/// names it, else as the project's name would make it.
pub fn slot_stack_id(project: &str, slot: u32, last_worktree: Option<&str>) -> String {
    last_worktree
        .and_then(|wt| {
            let p = project_config(wt).ok().flatten()?;
            info(&p, wt, slot).ok().flatten()
        })
        .map(|info| info.project_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| slot_project_id(project, slot))
}

pub fn running(project_id: &str) -> bool {
    Docker.running(&format!("supabase_db_{project_id}"))
}

/// Sets the stack's containers to restart never: docker's `unless-stopped` would bring a stack
/// back after a reboot with no lease on it.
pub fn keep_down_after_reboot(project_id: &str) {
    Docker.no_restart(project_id);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Up {
    Started,
    Running,
}

impl Up {
    pub fn word(self) -> &'static str {
        match self {
            Up::Started => "started",
            Up::Running => "running",
        }
    }
}

/// Starts the slot's stack unless it runs; its output goes to the slot's log.
pub fn up(
    p: &ProjectConfig,
    worktree: &str,
    slot: u32,
    env: &Env,
    cfg: &MachineConfig,
) -> Result<Up, Failure> {
    let Some(info) = info(p, worktree, slot)? else {
        return Err(Failure::precondition(
            "this lease has no stack slot (devlab.json has no supabase)",
        ));
    };
    if running(&info.project_id) {
        keep_down_after_reboot(&info.project_id);
        return Ok(Up::Running);
    }
    let memory = host::memory(&cfg.ci_slice);
    let free = memory.free_mb();
    if free - cfg.stack_ram_mb < cfg.host_floor_mb {
        return Err(Failure::new(
            Exit::Stack,
            format!(
                "too little memory to start a stack ({free} MB free, need {}){}",
                cfg.stack_ram_mb + cfg.host_floor_mb,
                after_ci(memory.headroom_mb())
            ),
        ));
    }
    let logs = devlab_state().join("logs");
    fs::create_dir_all(&logs).map_err(|e| io_failure(&logs, &e))?;
    let log = logs.join(format!("stack-{}-{slot}.log", p.name));
    let cli = p.supabase.as_ref().map_or(DEFAULT_CLI, |s| s.cli.as_str());
    let exclude = p
        .supabase
        .as_ref()
        .map_or(&[][..], |s| s.exclude.as_slice());
    let ran = SupabaseCli.start(cli, Path::new(worktree), env, exclude);
    fs::write(&log, format!("{}{}", ran.out, ran.err)).map_err(|e| io_failure(&log, &e))?;
    if ran.code != 0 {
        return Err(Failure::new(
            Exit::Stack,
            format!(
                "supabase start failed on slot {slot} (exit {}); log: {}",
                ran.code,
                log.display()
            ),
        ));
    }
    keep_down_after_reboot(&info.project_id);
    Ok(Up::Started)
}

pub fn down(p: &ProjectConfig, project_id: &str, cwd: &str) -> Result<(), Failure> {
    let cli = p.supabase.as_ref().map_or(DEFAULT_CLI, |s| s.cli.as_str());
    stop(cli, project_id, Path::new(cwd))
}

/// Stops slot `slot` of `project`'s stack by its project id, as the idle timer does: with the
/// CLI of the worktree it last ran when that worktree's devlab.json is still there, else the
/// default CLI from devlab's own directory, as the CLI needs a directory that exists.
pub fn down_slot(project: &str, slot: u32, last_worktree: Option<&str>) -> Result<(), Failure> {
    let project_id = slot_stack_id(project, slot, last_worktree);
    let config = last_worktree.and_then(|wt| project_config(wt).ok().flatten().map(|p| (wt, p)));
    match config {
        Some((worktree, p)) if Path::new(worktree).is_dir() => down(&p, &project_id, worktree),
        _ => {
            let cwd = devlab_state();
            fs::create_dir_all(&cwd).map_err(|e| io_failure(&cwd, &e))?;
            stop(DEFAULT_CLI, &project_id, &cwd)
        }
    }
}

fn stop(cli: &str, project_id: &str, cwd: &Path) -> Result<(), Failure> {
    let ran = SupabaseCli.stop(cli, project_id, cwd);
    if ran.code != 0 {
        let said = if ran.err.is_empty() {
            &ran.out
        } else {
            &ran.err
        };
        let last = said.trim().rsplit('\n').next().unwrap_or_default();
        return Err(Failure::new(
            Exit::Stack,
            format!("supabase stop --project-id {project_id} failed: {last}"),
        ));
    }
    Ok(())
}
