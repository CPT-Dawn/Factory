//! A project's local Supabase stack, through the CLI its devlab.json names.

use std::cell::RefCell;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process;

/// How long `supabase start` and `supabase stop` may take.
const START_TIMEOUT: Duration = Duration::from_secs(600);
const STOP_TIMEOUT: Duration = Duration::from_secs(180);

/// What one CLI run showed. `code` is -1 when it could not run or outlived its timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub code: i32,
    pub out: String,
    pub err: String,
}

pub trait Stacks {
    /// `<cli> start` in `cwd`, with `env` added to the environment, leaving out the services
    /// `exclude` names (`-x a,b`).
    fn start(&self, cli: &str, cwd: &Path, env: &[(String, String)], exclude: &[String]) -> Ran;
    /// `<cli> stop --project-id <project_id>` in `cwd`.
    fn stop(&self, cli: &str, project_id: &str, cwd: &Path) -> Ran;
}

/// The CLI as devlab.json writes it (`bunx supabase`), split on spaces.
pub struct SupabaseCli;

impl Stacks for SupabaseCli {
    fn start(&self, cli: &str, cwd: &Path, env: &[(String, String)], exclude: &[String]) -> Ran {
        let Some(mut cmd) = command(cli, cwd) else {
            return refused(cli);
        };
        cmd.args(start_args(exclude))
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        ran(cmd, START_TIMEOUT)
    }

    fn stop(&self, cli: &str, project_id: &str, cwd: &Path) -> Ran {
        let Some(mut cmd) = command(cli, cwd) else {
            return refused(cli);
        };
        cmd.args(["stop", "--project-id", project_id]);
        ran(cmd, STOP_TIMEOUT)
    }
}

/// The words after the CLI that start a stack: `start`, with `-x a,b` when it leaves services out.
pub fn start_args(exclude: &[String]) -> Vec<String> {
    let mut args = vec!["start".to_owned()];
    if !exclude.is_empty() {
        args.push("-x".to_owned());
        args.push(exclude.join(","));
    }
    args
}

fn command(cli: &str, cwd: &Path) -> Option<Command> {
    let mut words = cli.split(' ');
    let mut cmd = Command::new(words.next().filter(|w| !w.is_empty())?);
    cmd.args(words)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Some(cmd)
}

fn refused(cli: &str) -> Ran {
    Ran {
        code: -1,
        out: String::new(),
        err: format!("no supabase CLI in {cli:?}"),
    }
}

fn ran(cmd: Command, timeout: Duration) -> Ran {
    match process::run(cmd, timeout) {
        Ok(out) => Ran {
            code: out.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(e) => Ran {
            code: -1,
            out: String::new(),
            err: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_leaves_out_the_excluded_services_with_one_x() {
        assert_eq!(start_args(&[]), vec!["start"]);
        let exclude = ["studio".to_owned(), "edge-runtime".to_owned()];
        assert_eq!(
            start_args(&exclude),
            vec!["start", "-x", "studio,edge-runtime"]
        );
    }
}

/// Stacks that answer with the scripted result, recording each call as its words.
#[derive(Debug)]
pub struct FakeStacks {
    pub answer: Ran,
    pub calls: RefCell<Vec<Vec<String>>>,
}

impl Stacks for FakeStacks {
    fn start(&self, cli: &str, _cwd: &Path, _env: &[(String, String)], exclude: &[String]) -> Ran {
        let mut call = vec![cli.to_owned()];
        call.extend(start_args(exclude));
        self.calls.borrow_mut().push(call);
        self.answer.clone()
    }

    fn stop(&self, cli: &str, project_id: &str, _cwd: &Path) -> Ran {
        self.calls.borrow_mut().push(vec![
            cli.to_owned(),
            "stop".to_owned(),
            project_id.to_owned(),
        ]);
        self.answer.clone()
    }
}
