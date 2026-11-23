//! Reviews: the label and round rules review.sh had, the review bundle and its prompt, and
//! the report contract every backend's report is read by (spec #1, "Rubric and report
//! contract").

use std::fmt;

use serde_json::Value;

/// A label that would not make a safe file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadLabel(pub String);

impl fmt::Display for BadLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "label may only use letters, digits, _ and -, got: {:?}",
            self.0
        )
    }
}

impl std::error::Error for BadLabel {}

/// Checks that a review label names its files safely: `^[A-Za-z0-9_-]+$`.
pub fn check_label(label: &str) -> Result<(), BadLabel> {
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !label.is_empty() && label.chars().all(safe) {
        Ok(())
    } else {
        Err(BadLabel(label.to_owned()))
    }
}

/// The round a review is: the `-rN` suffix of its label, else 1, and 2 when a round-1 label
/// comes with a previous report.
pub fn round(label: &str, has_previous: bool) -> u32 {
    let suffix = label
        .rsplit_once("-r")
        .and_then(|(_, n)| (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(n))
        .and_then(|n| n.parse().ok());
    match suffix {
        Some(1) | None if has_previous => 2,
        Some(n) => n,
        None => 1,
    }
}

/// The issue number a spec names when it is an issue ref: `#12`, `12` or
/// `https://github.com/<owner>/<repo>/issues/12`.
pub fn issue_number(spec: &str) -> Option<u64> {
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    match spec.strip_prefix("https://github.com/") {
        Some(rest) => match rest.split('/').collect::<Vec<_>>()[..] {
            [owner, repo, "issues", n] if !owner.is_empty() && !repo.is_empty() => number(n),
            _ => None,
        },
        None => number(spec.strip_prefix('#').unwrap_or(spec)),
    }
}

/// One named text the bundle carries: a spec, a report, a standards file, an evidence file.
#[derive(Clone, Copy, Debug)]
pub struct Part<'a> {
    pub name: &'a str,
    pub text: &'a str,
}

/// Everything a reviewer reads, gathered before it starts, so it re-runs and re-reads nothing.
#[derive(Clone, Debug)]
pub struct Bundle<'a> {
    pub label: &'a str,
    pub round: u32,
    pub base: &'a str,
    pub base_commit: &'a str,
    pub head: &'a str,
    pub commits: &'a str,
    pub diff_stat: &'a str,
    pub diff: &'a str,
    pub spec: Option<Part<'a>>,
    pub previous: Option<Part<'a>>,
    /// The repo's standards files, which name what tooling already enforces.
    pub standards: Vec<Part<'a>>,
    /// The implementer's record of the gates and checks it ran.
    pub evidence: Option<Part<'a>>,
}

impl Bundle<'_> {
    /// The bundle as one Markdown file, each part fenced so nothing inside it reads as
    /// structure. The diff comes last, being the largest.
    pub fn render(&self) -> String {
        let mut out = format!(
            "# Review bundle: {}\n\nRound {}: the commits from {} ({}) to {}.\n",
            self.label, self.round, self.base, self.base_commit, self.head
        );
        fenced(&mut out, "Commits", self.commits, "");
        fenced(&mut out, "Diff stat", self.diff_stat, "");
        named(
            &mut out,
            "Spec",
            self.spec,
            "None given: skip the spec axis.",
        );
        named(
            &mut out,
            "Previous review",
            self.previous,
            "None: this is a first round.",
        );
        if self.standards.is_empty() {
            absent(
                &mut out,
                "Standards",
                "The repo documents none: apply the smell baseline only.",
            );
        }
        for part in &self.standards {
            named(&mut out, "Standards", Some(*part), "");
        }
        named(
            &mut out,
            "Implementer evidence",
            self.evidence,
            "None given.",
        );
        fenced(&mut out, "Diff", self.diff, "diff");
        out
    }
}

/// Appends `## heading: name` and the part, or `## heading` and `note` when there is none.
fn named(out: &mut String, heading: &str, part: Option<Part<'_>>, note: &str) {
    match part {
        Some(part) => fenced(out, &format!("{heading}: {}", part.name), part.text, ""),
        None => absent(out, heading, note),
    }
}

/// Appends `## heading` and `text` in a fence longer than any backtick run inside it, with
/// `info` as the fence's language.
fn fenced(out: &mut String, heading: &str, text: &str, info: &str) {
    let fence = "`".repeat(longest_backtick_run(text).max(2) + 1);
    out.push_str(&format!("\n## {heading}\n\n{fence}{info}\n{text}"));
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{fence}\n"));
}

fn absent(out: &mut String, heading: &str, note: &str) {
    out.push_str(&format!("\n## {heading}\n\n{note}\n"));
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// What every backend's report must end with, so the factory can read it.
pub const REPORT_CONTRACT: &str = r#"End the report with its findings ledger, then the FINDINGS and VERDICT lines:

## Findings ledger

```json
{"findings": [{"severity": "blocking", "axis": "spec", "tag": "partial-requirement", "location": "src/lib.rs:12", "summary": "one line"}]}
```

One entry per open finding (an empty list when none is open): severity is blocking, advisory or question; axis is standards or spec; tag is one from the rubric's list."#;

/// The prompt a backend gets: the bundle is its whole input.
pub fn prompt(base: &str, bundle: &str, spec: &str, previous: Option<&str>) -> String {
    let mut out = format!(
        "$code-review since {base}.\n\
         The review bundle is your whole input: {bundle}. It holds the commits, the diff stat, \
         the full diff, the spec, the previous review, the repo's standards (skip what they say \
         tooling enforces) and the implementer's evidence. Open other files only to check a \
         specific claim. Do not run tests, builds, gh or devlab: the sandbox cannot.\n\
         Spec: {spec}\n"
    );
    if let Some(previous) = previous {
        out.push_str(&format!("Previous review: {previous}\n"));
    }
    out.push_str("Non-interactive run: nobody can answer questions.\n\n");
    out.push_str(REPORT_CONTRACT);
    out.push('\n');
    out
}

/// How much a finding holds a review back: only blocking findings do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Blocking,
    Advisory,
    Question,
}

impl Severity {
    fn parse(word: &str) -> Option<Severity> {
        match word {
            "blocking" => Some(Severity::Blocking),
            "advisory" => Some(Severity::Advisory),
            "question" => Some(Severity::Question),
            _ => None,
        }
    }
}

/// One entry of a report's findings ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// `standards` or `spec`.
    pub axis: String,
    pub tag: String,
    /// Where it is, as `path:line`.
    pub location: String,
    pub summary: String,
}

impl Finding {
    /// The finding as the one line a caller reads.
    pub fn line(&self) -> String {
        format!(
            "- [{}/{}] {}: {}",
            self.axis, self.tag, self.location, self.summary
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing blocking is open.
    Clean,
    Findings,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Clean => "clean",
            Verdict::Findings => "findings",
        })
    }
}

/// Open findings by severity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub blocking: u64,
    pub advisory: u64,
    pub question: u64,
}

/// What a report says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub verdict: Verdict,
    pub counts: Counts,
    /// The report's findings ledger; none when it holds no readable one.
    pub findings: Option<Vec<Finding>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportError {
    /// Neither a findings ledger nor a VERDICT line.
    NoVerdict,
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::NoVerdict => {
                f.write_str("the report has no findings ledger and no VERDICT line")
            }
        }
    }
}

impl std::error::Error for ReportError {}

impl Reading {
    /// Reads a report. Its findings ledger, the last fenced `json` block, decides the verdict
    /// and the counts.
    pub fn of(report: &str) -> Result<Reading, ReportError> {
        let findings = ledger(report).ok_or(ReportError::NoVerdict)?;
        let mut counts = Counts::default();
        for finding in &findings {
            match finding.severity {
                Severity::Blocking => counts.blocking += 1,
                Severity::Advisory => counts.advisory += 1,
                Severity::Question => counts.question += 1,
            }
        }
        let verdict = if counts.blocking == 0 {
            Verdict::Clean
        } else {
            Verdict::Findings
        };
        Ok(Reading {
            verdict,
            counts,
            findings: Some(findings),
        })
    }

    /// The blocking findings, one line each.
    pub fn blocking_lines(&self) -> Vec<String> {
        self.findings
            .iter()
            .flatten()
            .filter(|finding| finding.severity == Severity::Blocking)
            .map(Finding::line)
            .collect()
    }

    /// The line a caller sees: `review <label>: <verdict> (n blocking, n advisory, n question) → <report>`.
    pub fn summary(&self, label: &str, report: &str) -> String {
        let Counts {
            blocking,
            advisory,
            question,
        } = self.counts;
        format!(
            "review {label}: {} ({blocking} blocking, {advisory} advisory, {question} question) → {report}",
            self.verdict
        )
    }
}

/// The findings in the report's last fenced `json` block, when it reads as a ledger.
fn ledger(report: &str) -> Option<Vec<Finding>> {
    let start = report.rfind("```json\n")? + "```json\n".len();
    let body = &report[start..];
    let body = &body[..body.find("```")?];
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("findings")?
        .as_array()?
        .iter()
        .map(finding)
        .collect()
}

fn finding(value: &Value) -> Option<Finding> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Some(Finding {
        severity: Severity::parse(value.get("severity")?.as_str()?)?,
        axis: text("axis"),
        tag: text("tag"),
        location: text("location"),
        summary: text("summary"),
    })
}
