//! Running a command inside a lease: its env on top of the caller's, its stdio the caller's
//! (or its output in a file).

use std::fs::File;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::cli::note;
use super::context::Ctx;
use super::lease::lease_env;
use super::wire::Lease;
use crate::output::Failure;

/// Runs `argv` in the lease and returns its exit code (128 + the signal that ended it); 127
/// when it can't be run. Its stdout and stderr, and why it couldn't run, go to `output` when
/// given, else they are the caller's.
pub fn exec_in(
    ctx: &Ctx,
    lease: &Lease,
    argv: &[String],
    output: Option<&Path>,
) -> Result<u8, Failure> {
    let env = lease_env(lease, ctx.project.as_ref())?;
    let Some((program, args)) = argv.split_first() else {
        return Err(Failure::usage("devlab exec -- <cmd...>"));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut log = None;
    if let Some(path) = output {
        let unwritable = |e: std::io::Error| {
            Failure::precondition(format!(
                "cannot write {}: {e}; check XDG_RUNTIME_DIR",
                path.display()
            ))
        };
        let file = File::create(path).map_err(unwritable)?;
        command.stdout(Stdio::from(file.try_clone().map_err(unwritable)?));
        command.stderr(Stdio::from(file.try_clone().map_err(unwritable)?));
        log = Some(file);
    }
    match command.status() {
        Ok(status) => Ok(match (status.code(), status.signal()) {
            (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
            (None, Some(signal)) => u8::try_from((128 + signal) & 0xff).unwrap_or(1),
            (None, None) => 1,
        }),
        Err(e) => {
            let why = format!("cannot run {program}: {e}");
            match log.as_mut() {
                Some(file) => {
                    let _ = writeln!(file, "{why}");
                }
                None => note(&why),
            }
            Ok(127)
        }
    }
}
