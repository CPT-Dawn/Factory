//! Today's TypeScript devlab, which `factory cutover` retires (#20): asked what it holds, and
//! whether it refuses once cut over.

use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::process;

/// How long one call may take: `status` asks adb, systemd and docker about each device and slot.
const TIMEOUT: Duration = Duration::from_secs(120);

/// What one call showed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    /// None when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Why a call could not be made, or did not end in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevlabError(pub String);

impl fmt::Display for DevlabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DevlabError {}

/// Today's devlab, as the cutover asks it.
pub trait OldDevlab {
    /// Runs `devlab <args>` to its end.
    fn call(&self, args: &[&str]) -> Result<Answer, DevlabError>;
}

/// The devlab at `program`: the launcher script the `devlab` name linked to.
pub struct TsDevlab {
    pub program: PathBuf,
}

impl OldDevlab for TsDevlab {
    fn call(&self, args: &[&str]) -> Result<Answer, DevlabError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = process::run(cmd, TIMEOUT).map_err(|e| {
            DevlabError(format!(
                "{} {}: {e}",
                self.program.display(),
                args.join(" ")
            ))
        })?;
        Ok(Answer {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A devlab that answers each call from a script, and records the calls.
#[derive(Debug, Default)]
pub struct FakeDevlab {
    /// The answer to a call, by its arguments joined with spaces; a call with none fails.
    pub answers: Vec<(String, Answer)>,
    pub calls: Mutex<Vec<String>>,
}

impl OldDevlab for FakeDevlab {
    fn call(&self, args: &[&str]) -> Result<Answer, DevlabError> {
        let call = args.join(" ");
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call.clone());
        self.answers
            .iter()
            .find(|(asked, _)| *asked == call)
            .map(|(_, answer)| answer.clone())
            .ok_or_else(|| DevlabError(format!("fake devlab: no answer for: {call}")))
    }
}

#[cfg(test)]
mod tests {
    use super::{Answer, FakeDevlab, OldDevlab};

    #[test]
    fn the_fake_answers_its_script_and_records_each_call() {
        let answer = Answer {
            code: Some(0),
            stdout: "{}".to_owned(),
            stderr: String::new(),
        };
        let fake = FakeDevlab {
            answers: vec![("status --json".to_owned(), answer.clone())],
            ..FakeDevlab::default()
        };

        assert_eq!(fake.call(&["status", "--json"]), Ok(answer));
        assert!(fake.call(&["gc"]).is_err());
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec!["status --json".to_owned(), "gc".to_owned()]
        );
    }
}
