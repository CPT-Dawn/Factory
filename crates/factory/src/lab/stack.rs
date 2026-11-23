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
use factory_core::lab::slots::{
    Env, StackInfo, patch_supabase_config, project_id_of, read_supabase_ports, slot_project_id,
};

use super::context::devlab_state;
use super::host::mem_available_mb;
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

pub fn running(project_id: &str) -> bool {
    Docker.running(&format!("supabase_db_{project_id}"))
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
        return Ok(Up::Running);
    }
    let free = mem_available_mb();
    if free - cfg.stack_ram_mb < cfg.host_floor_mb {
        return Err(Failure::new(
            Exit::Stack,
            format!(
                "too little memory to start a stack ({free} MB free, need {})",
                cfg.stack_ram_mb + cfg.host_floor_mb
            ),
        ));
    }
    let logs = devlab_state().join("logs");
    fs::create_dir_all(&logs).map_err(|e| io_failure(&logs, &e))?;
    let log = logs.join(format!("stack-{}-{slot}.log", p.name));
    let cli = p.supabase.as_ref().map_or(DEFAULT_CLI, |s| s.cli.as_str());
    let ran = SupabaseCli.start(cli, Path::new(worktree), env);
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
    Ok(Up::Started)
}

pub fn down(p: &ProjectConfig, project_id: &str, cwd: &str) -> Result<(), Failure> {
    let cli = p.supabase.as_ref().map_or(DEFAULT_CLI, |s| s.cli.as_str());
    let ran = SupabaseCli.stop(cli, project_id, Path::new(cwd));
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
