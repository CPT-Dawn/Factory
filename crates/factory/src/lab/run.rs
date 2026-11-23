//! Running a command inside a lease: its env on top of the caller's, its stdio the caller's.
//!
//! The command is a child of this process, which stays alive until it ends: target-repo scripts
//! find the `devlab run` above them by walking their ancestors. It gets no time limit; it ends
//! when it ends.

use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus};

use super::cli::note;
use super::context::Ctx;
use super::lease::lease_env;
use super::wire::Lease;
use crate::output::Failure;

/// Exit code when the command can't be run, as a shell says it.
const CANNOT_RUN: u8 = 127;

/// Runs `argv` in the lease and returns its exit code (128 + the signal that ended it);
/// [`CANNOT_RUN`] when it can't be run.
pub fn exec_in(ctx: &Ctx, lease: &Lease, argv: &[String]) -> Result<u8, Failure> {
    let env = lease_env(lease, ctx.project.as_ref())?;
    let Some((program, args)) = argv.split_first() else {
        return Err(Failure::usage("devlab exec -- <cmd...>"));
    };
    let spawned = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .spawn();
    match spawned {
        Ok(child) => Ok(exit_code(&wait_for(child))),
        Err(e) => {
            note(&format!("cannot run {program}: {e}"));
            Ok(CANNOT_RUN)
        }
    }
}

/// Waits until the child has ended. The one place this process idles while the command runs.
fn wait_for(mut child: Child) -> ExitStatus {
    loop {
        match child.wait() {
            Ok(status) => return status,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return ExitStatus::from_raw(1 << 8),
        }
    }
}

/// What the shell would call the status: its code, or 128 + the signal that ended it.
fn exit_code(status: &ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
        (None, Some(signal)) => u8::try_from((128 + signal) & 0xff).unwrap_or(1),
        (None, None) => 1,
    }
}
