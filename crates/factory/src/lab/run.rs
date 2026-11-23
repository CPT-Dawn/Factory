//! Running a command inside a lease: its env on top of the caller's, its stdio the caller's.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use super::cli::note;
use super::context::Ctx;
use super::lease::lease_env;
use super::wire::Lease;
use crate::output::Failure;

/// Runs `argv` in the lease and returns its exit code (128 + the signal that ended it); 127
/// when it can't be run.
pub fn exec_in(ctx: &Ctx, lease: &Lease, argv: &[String]) -> Result<u8, Failure> {
    let env = lease_env(lease, ctx.project.as_ref())?;
    let Some((program, args)) = argv.split_first() else {
        return Err(Failure::usage("devlab exec -- <cmd...>"));
    };
    let status = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .status();
    match status {
        Ok(status) => Ok(match (status.code(), status.signal()) {
            (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
            (None, Some(signal)) => u8::try_from((128 + signal) & 0xff).unwrap_or(1),
            (None, None) => 1,
        }),
        Err(e) => {
            note(&format!("cannot run {program}: {e}"));
            Ok(127)
        }
    }
}
