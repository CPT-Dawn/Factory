//! GitHub issues, through `gh`.

use std::fmt;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process;

/// How long fetching one issue may take.
const TIMEOUT: Duration = Duration::from_secs(60);
/// An issue as Markdown: its number and title as the heading, then its body.
const TEMPLATE: &str = r#"# #{{.number}} {{.title}}{{"\n\n"}}{{.body}}{{"\n"}}"#;

/// Why an issue could not be fetched: what gh said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhError(pub String);

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GhError {}

/// The issue tracker of the repository in the working directory.
pub trait Issues {
    /// Issue `number` as Markdown: `# #<number> <title>`, a blank line, then its body.
    fn issue(&self, number: u64) -> Result<String, GhError>;
}

/// The issues `gh` on PATH sees from the working directory.
pub struct Gh;

impl Issues for Gh {
    fn issue(&self, number: u64) -> Result<String, GhError> {
        let mut cmd = Command::new("gh");
        cmd.args(["issue", "view", &number.to_string()])
            .args(["--json", "number,title,body", "--template", TEMPLATE])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = process::run(cmd, TIMEOUT).map_err(|e| GhError(e.to_string()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GhError(
                stderr
                    .lines()
                    .next()
                    .unwrap_or("it failed and said nothing")
                    .to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Issues held in memory, for tests of code that fetches them.
#[derive(Clone, Debug, Default)]
pub struct FakeIssues {
    /// Each known issue's number and its Markdown.
    pub issues: Vec<(u64, String)>,
}

impl Issues for FakeIssues {
    fn issue(&self, number: u64) -> Result<String, GhError> {
        self.issues
            .iter()
            .find(|(known, _)| *known == number)
            .map(|(_, text)| text.clone())
            .ok_or_else(|| GhError(format!("no issue {number}")))
    }
}
