//! Reviews: the label and round rules review.sh had, the review bundle and its prompt, and
//! the report contract every backend's report is read by (spec #1, "Rubric and report
//! contract").

use std::collections::{BTreeMap, BTreeSet};
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

/// The issue `gh issue view` should fetch when a spec is an issue ref: the number of `#12` or
/// `12`, or a whole `https://github.com/<owner>/<repo>/issues/12`, which may name another repo.
pub fn issue_ref(spec: &str) -> Option<&str> {
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match spec.strip_prefix("https://github.com/") {
        Some(rest) => match rest.split('/').collect::<Vec<_>>()[..] {
            [owner, repo, "issues", n] if !owner.is_empty() && !repo.is_empty() && number(n) => {
                Some(spec)
            }
            _ => None,
        },
        None => Some(spec.strip_prefix('#').unwrap_or(spec)).filter(|n| number(n)),
    }
}

/// The lines a fix touched, read from its diff: the new-side lines it added or changed, and
/// the line after each deletion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fix {
    touched: BTreeMap<String, BTreeSet<u32>>,
}

impl Fix {
    /// Reads a unified diff of any amount of context.
    pub fn of_diff(diff: &str) -> Fix {
        let mut touched: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
        let mut path: Option<&str> = None;
        // The old and new lines left in the hunk being read, and the new line number.
        let (mut old_left, mut new_left, mut line) = (0u32, 0u32, 0u32);
        for text in diff.lines() {
            if old_left > 0 || new_left > 0 {
                let mark = |touched: &mut BTreeMap<String, BTreeSet<u32>>, line: u32| {
                    if let Some(path) = path {
                        touched.entry(path.to_owned()).or_default().insert(line);
                    }
                };
                match text.as_bytes().first() {
                    Some(b'+') => {
                        mark(&mut touched, line);
                        (new_left, line) = (new_left.saturating_sub(1), line + 1);
                    }
                    Some(b'-') => {
                        mark(&mut touched, line);
                        old_left = old_left.saturating_sub(1);
                    }
                    Some(b'\\') => {}
                    _ => {
                        old_left = old_left.saturating_sub(1);
                        (new_left, line) = (new_left.saturating_sub(1), line + 1);
                    }
                }
            } else if let Some(file) = text.strip_prefix("+++ ") {
                path = file.strip_prefix("b/");
            } else if let Some(hunk) = text.strip_prefix("@@ -")
                && let Some((old, new)) = hunk.split_once(" +")
            {
                let (_, old_len) = span(old);
                let (start, new_len) = span(new.split(' ').next().unwrap_or_default());
                (old_left, new_left, line) = (old_len, new_len, start);
            }
        }
        Fix { touched }
    }

    /// Whether a finding's `path:line`, `path:from-to` or bare `path` lies on lines the fix
    /// touched. A location naming no file the fix changed does not.
    pub fn touches(&self, location: &str) -> bool {
        let location = location.trim().trim_matches('`');
        let location = location.strip_prefix("./").unwrap_or(location);
        let (path, lines) = match location
            .match_indices(':')
            .find(|(i, _)| location[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        {
            Some((i, _)) => (&location[..i], Some(&location[i + 1..])),
            None => (location, None),
        };
        let Some(touched) = self.touched.get(path) else {
            return false;
        };
        let Some(lines) = lines else {
            return true;
        };
        let Some((from, rest)) = number(lines) else {
            return true;
        };
        let to = rest
            .strip_prefix('-')
            .and_then(number)
            .map_or(from, |(to, _)| to);
        touched.range(from..=to.max(from)).next().is_some()
    }
}

/// The number a text starts with, and what follows it.
fn number(text: &str) -> Option<(u32, &str)> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    text[..digits]
        .parse::<u32>()
        .ok()
        .map(|n| (n, &text[digits..]))
}

/// The start and length of a hunk header's `start,len` (length 1 when it names none).
fn span(range: &str) -> (u32, u32) {
    let range = range.trim_start_matches(['-', '+']);
    let (start, len) = range.split_once(',').unwrap_or((range, "1"));
    (start.parse().unwrap_or(0), len.parse().unwrap_or(1))
}

/// What a fix round takes from the previous round's findings ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Previous {
    /// The head that round reviewed: where this round's diff starts.
    pub head: String,
    /// That round's findings that are still open, numbered `f1`, `f2`... for the reviewer to
    /// mark; none when its report held no readable findings ledger.
    pub open: Option<Vec<Finding>>,
}

impl Previous {
    /// Reads a review's findings ledger (`<label>.findings.json`); none without a head.
    pub fn of_ledger(ledger: &str) -> Option<Previous> {
        let value: Value = serde_json::from_str(ledger).ok()?;
        let head = value
            .get("head")?
            .as_str()
            .filter(|head| !head.is_empty())?;
        let open = value.get("findings")?.as_array().and_then(|entries| {
            let findings: Option<Vec<Finding>> = entries.iter().map(finding).collect();
            Some(
                findings?
                    .into_iter()
                    .filter(|finding| finding.status != Status::Fixed)
                    .enumerate()
                    .map(|(i, finding)| Finding {
                        id: format!("f{}", i + 1),
                        status: Status::New,
                        ..finding
                    })
                    .collect(),
            )
        });
        Some(Previous {
            head: head.to_owned(),
            open,
        })
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
    /// On a fix round, the head the previous round reviewed: the diff starts there, not at
    /// the base.
    pub fix_from: Option<&'a str>,
    /// On a fix round, the previous round's open findings, which the reviewer marks fixed or
    /// still present.
    pub open: Option<&'a [Finding]>,
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
        let mut out = match self.fix_from {
            Some(from) => format!(
                "# Review bundle: {}\n\nRound {}, the fix only: the commits from {} (the head round {} reviewed) to {}.\n",
                self.label,
                self.round,
                from,
                self.round - 1,
                self.head
            ),
            None => format!(
                "# Review bundle: {}\n\nRound {}: the commits from {} ({}) to {}.\n",
                self.label, self.round, self.base, self.base_commit, self.head
            ),
        };
        fenced(&mut out, "Commits", self.commits, "");
        fenced(&mut out, "Diff stat", self.diff_stat, "");
        named(
            &mut out,
            "Spec",
            self.spec,
            "None given: skip the spec axis.",
        );
        match (self.open, self.previous) {
            (Some(open), previous) => previous_findings(&mut out, open, previous.map(|p| p.name)),
            (None, previous) => named(
                &mut out,
                "Previous review",
                previous,
                "None: this is a first round.",
            ),
        }
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

/// Appends the previous round's open findings as JSON, with what a fix round does with them.
fn previous_findings(out: &mut String, open: &[Finding], report: Option<&str>) {
    let ledger =
        serde_json::json!({"findings": open.iter().map(Finding::to_value).collect::<Vec<_>>()});
    let mut text = String::from(
        "The previous round's open findings. Mark each in your ledger by its id as fixed or still_present; report a new finding only on a line this diff touches (a security problem or a hard standards violation anywhere).\n",
    );
    if let Some(report) = report {
        text.push_str(&format!(
            "Its report, to open only to check a claim: {report}\n"
        ));
    }
    out.push_str(&format!("\n## Previous findings\n\n{text}"));
    fenced(out, "", &format!("{ledger:#}"), "json");
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

/// What a fix round's reviewer is told on top of the report contract.
pub const FIX_ROUND: &str = r#"This is a fix round: the bundle's diff is only the fix, and its "Previous findings" are the open findings of the round before. Put each of them in your findings ledger again, whole and with its "id", and add `"status": "fixed"` or `"status": "still_present"`. Report a new finding (no id, `"status": "new"`) only on a line the fix touched, except a hard Standards violation or a security problem, which you report wherever it is. The factory drops any other new finding."#;

/// What a headless Claude reviewer is told beside its prompt: it stands in for the others, so
/// it reads the reviewer skill they run as a file and follows it.
pub const STANDIN_REVIEWER: &str = "You are the factory's code reviewer, standing in while the \
     other review backends are out of quota. Your steps are the review skill at \
     ~/.agents/skills/code-review/SKILL.md (a `code-review` skill in your listing is the \
     dispatcher, not the reviewer). Read it first and follow it exactly: both axes, the report \
     format, and the closing FINDINGS and VERDICT lines. The reviewer rules in \
     ~/.codex/AGENTS.md apply as well.";

/// The prompt a backend gets: the bundle is its whole input. `opening` is the backend's own
/// first line, which sets it reviewing the changes since the base.
pub fn prompt(
    opening: &str,
    bundle: &str,
    spec: &str,
    previous: Option<&str>,
    fix_round: bool,
) -> String {
    let mut out = format!(
        "{opening}\n\
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
    if fix_round {
        out.push('\n');
        out.push_str(FIX_ROUND);
        out.push('\n');
    }
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
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocking => "blocking",
            Severity::Advisory => "advisory",
            Severity::Question => "question",
        }
    }

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
    /// How a fix round numbered it for the reviewer to answer; empty on a first round.
    pub id: String,
    pub status: Status,
}

/// What a fix round's reviewer says of a finding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// Raised this round.
    #[default]
    New,
    /// An earlier finding the fix settled.
    Fixed,
    /// An earlier finding the fix left open.
    StillPresent,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Fixed => "fixed",
            Status::StillPresent => "still_present",
        }
    }

    fn parse(word: &str) -> Option<Status> {
        match word {
            "new" => Some(Status::New),
            "fixed" => Some(Status::Fixed),
            "still_present" => Some(Status::StillPresent),
            _ => None,
        }
    }
}

impl Finding {
    /// A finding a fix round keeps wherever it lies: a hard Standards violation or a security
    /// problem.
    pub fn is_exempt(&self) -> bool {
        self.tag == "security" || (self.axis == "standards" && self.severity == Severity::Blocking)
    }

    /// The finding as the one line a caller reads.
    pub fn line(&self) -> String {
        format!(
            "- [{}/{}] {}: {}",
            self.axis, self.tag, self.location, self.summary
        )
    }

    /// The finding as its findings-ledger entry.
    pub fn to_value(&self) -> Value {
        let mut value = serde_json::json!({
            "severity": self.severity.as_str(),
            "axis": self.axis,
            "tag": self.tag,
            "location": self.location,
            "summary": self.summary,
        });
        if !self.id.is_empty() {
            value["id"] = self.id.clone().into();
        }
        if self.status != Status::New {
            value["status"] = self.status.as_str().into();
        }
        value
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
    /// On a fix round, the earlier findings the fix settled and the ones it left open.
    pub fixed: u64,
    pub still_present: u64,
    /// On a fix round, the new findings dropped for lying outside the fix's lines.
    pub dropped: Vec<Finding>,
    /// The `key=value` words of its FINDINGS line, which the review event carries on.
    pub tallies: Vec<String>,
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
    /// and the counts; without one, its VERDICT and FINDINGS lines do.
    pub fn of(report: &str) -> Result<Reading, ReportError> {
        let tallies = findings_line(report);
        if let Some(findings) = ledger(report) {
            return Ok(Reading::counted(findings, Vec::new(), tallies));
        }
        let verdict = verdict_line(report).ok_or(ReportError::NoVerdict)?;
        let tally = |keys: &[&str]| {
            keys.iter()
                .filter_map(|key| {
                    tallies
                        .iter()
                        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .sum()
        };
        let counts = Counts {
            blocking: tally(&["standards_hard", "spec_findings"]),
            advisory: tally(&["standards_judgement", "spec_advisory"]),
            question: tally(&["questions"]),
        };
        Ok(Reading {
            verdict,
            counts,
            findings: None,
            fixed: tally(&["fixed"]),
            still_present: tally(&["still_present"]),
            dropped: Vec::new(),
            tallies,
        })
    }

    /// A reading of `findings`: the open ones (not fixed) decide the counts and the verdict.
    fn counted(findings: Vec<Finding>, dropped: Vec<Finding>, tallies: Vec<String>) -> Reading {
        let mut counts = Counts::default();
        let (mut fixed, mut still_present) = (0, 0);
        for finding in &findings {
            match finding.status {
                Status::Fixed => {
                    fixed += 1;
                    continue;
                }
                Status::StillPresent => still_present += 1,
                Status::New => {}
            }
            match finding.severity {
                Severity::Blocking => counts.blocking += 1,
                Severity::Advisory => counts.advisory += 1,
                Severity::Question => counts.question += 1,
            }
        }
        Reading {
            verdict: if counts.blocking == 0 {
                Verdict::Clean
            } else {
                Verdict::Findings
            },
            counts,
            findings: Some(findings),
            fixed,
            still_present,
            dropped,
            tallies,
        }
    }

    /// Holds a fix round's reading to the round's rules. Each of the `previous` round's open
    /// findings the reviewer did not mark by its id stays open, as still present: nothing
    /// shows it fixed. A new finding off the lines `fix` touched is dropped, unless it is hard
    /// or a security problem. A reading with no findings ledger is returned as it is.
    pub fn within(self, previous: &[Finding], fix: &Fix) -> Reading {
        let Some(mut findings) = self.findings else {
            return self;
        };
        let marked = |id: &str| {
            findings
                .iter()
                .any(|f| f.status != Status::New && f.id == id)
        };
        let unmarked: Vec<Finding> = previous
            .iter()
            .filter(|p| !marked(&p.id))
            .map(|p| Finding {
                status: Status::StillPresent,
                ..p.clone()
            })
            .collect();
        let (kept, dropped): (Vec<Finding>, Vec<Finding>) = findings
            .drain(..)
            .partition(|f| f.status != Status::New || f.is_exempt() || fix.touches(&f.location));
        let kept = kept.into_iter().chain(unmarked).collect();
        Reading::counted(kept, dropped, self.tallies)
    }

    /// The blocking findings, one line each.
    pub fn blocking_lines(&self) -> Vec<String> {
        self.findings
            .iter()
            .flatten()
            .filter(|finding| {
                finding.severity == Severity::Blocking && finding.status != Status::Fixed
            })
            .map(Finding::line)
            .collect()
    }

    /// The line a caller sees: `review <label>: <verdict> (n blocking, n advisory, n question) → <report>`.
    pub fn summary(&self, label: &str, report: &str) -> String {
        let fix = crate::finished::Fixes {
            fixed: self.fixed,
            still_present: self.still_present,
            dropped: self.dropped.len() as u64,
        };
        crate::finished::summary(label, &self.verdict.to_string(), self.counts, fix, report)
    }
}

/// The words after the report's last `FINDINGS:`, the one beside its VERDICT line.
fn findings_line(report: &str) -> Vec<String> {
    report
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("FINDINGS:"))
        .map(|rest| rest.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The verdict of the report's last `VERDICT:` line.
fn verdict_line(report: &str) -> Option<Verdict> {
    let rest = report
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("VERDICT:"))?;
    match rest
        .trim_start()
        .split(|c: char| !c.is_ascii_lowercase())
        .next()?
    {
        "clean" => Some(Verdict::Clean),
        "findings" => Some(Verdict::Findings),
        _ => None,
    }
}
/// One run of a review, as its `review` event in the ledger records it.
#[derive(Clone, Debug)]
pub struct Record<'a> {
    pub repo: &'a str,
    pub label: &'a str,
    pub round: u32,
    pub base: &'a str,
    pub head: &'a str,
    pub spec: &'a str,
    /// The backend that ran the review, logged as `reviewer` too, the name factory-stats reads.
    pub backend: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub duration_s: u64,
    /// The backends that ran out of quota before this one took the review, comma-joined.
    pub fallback_from: &'a str,
    /// The backends passed over without a try (agy with no usage answer or no active
    /// sandbox), comma-joined.
    pub skipped: &'a str,
    pub report: &'a str,
    pub stream: &'a str,
    /// The run's findings ledger file.
    pub findings: &'a str,
    /// On a fix round, the head the previous round reviewed, where its diff started.
    pub from: &'a str,
    /// What the report says, or why the run failed.
    pub result: Result<&'a Reading, &'a str>,
}

impl Record<'_> {
    /// The event's `key=value` pairs, as `factory log review` takes them: the run, then its
    /// counts, then the report's FINDINGS words. Empty values are dropped when they are parsed.
    pub fn pairs(&self) -> Vec<String> {
        let (outcome, verdict, reason) = match self.result {
            Ok(reading) => ("ok", reading.verdict.to_string(), ""),
            Err(reason) => ("error", String::new(), reason),
        };
        let mut pairs = vec![
            format!("repo={}", self.repo),
            format!("label={}", self.label),
            format!("round={}", self.round),
            format!("base={}", self.base),
            format!("head={}", self.head),
            format!("spec={}", self.spec),
            format!("outcome={outcome}"),
            format!("verdict={verdict}"),
            format!("reason={reason}"),
            format!("reviewer={}", self.backend),
            format!("backend={}", self.backend),
            format!("duration_s={}", self.duration_s),
            format!("fallback_from={}", self.fallback_from),
            format!("skipped={}", self.skipped),
            format!("model={}", self.model.unwrap_or_default()),
            format!("effort={}", self.effort.unwrap_or_default()),
            format!("report={}", self.report),
            format!("stream={}", self.stream),
            format!("from={}", self.from),
        ];
        if let Ok(reading) = self.result {
            let Counts {
                blocking,
                advisory,
                question,
            } = reading.counts;
            pairs.push(format!("findings={}", self.findings));
            pairs.push(format!("blocking={blocking}"));
            pairs.push(format!("advisory={advisory}"));
            pairs.push(format!("question={question}"));
            if self.round > 1 {
                pairs.push(format!("fixed={}", reading.fixed));
                pairs.push(format!("still_present={}", reading.still_present));
                pairs.push(format!("dropped={}", reading.dropped.len()));
            }
            // A FINDINGS word joins only when it is `key=value` with a lowercase key the event
            // doesn't already set, so a stray word can neither overwrite a field nor be refused.
            let taken: Vec<String> = pairs
                .iter()
                .filter_map(|pair| Some(pair.split_once('=')?.0.to_owned()))
                .collect();
            let joins = |word: &&String| {
                word.split_once('=').is_some_and(|(key, _)| {
                    !key.is_empty()
                        && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                        && !taken.iter().any(|own| own == key)
                })
            };
            pairs.extend(reading.tallies.iter().filter(joins).cloned());
        }
        pairs
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
        id: text("id"),
        status: match value.get("status").and_then(Value::as_str) {
            Some(word) => Status::parse(word)?,
            None => Status::New,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_is_the_label_suffix_else_1_and_2_for_a_first_label_with_a_previous_report() {
        for (label, has_previous, round) in [
            ("t1", false, 1),
            ("t1", true, 2),
            ("t1-r1", false, 1),
            ("t1-r1", true, 2),
            ("t1-r3", true, 3),
            ("t1-r12", false, 12),
            ("t1-r", false, 1),
            ("t1-rx", false, 1),
            ("t1-r2b", false, 1),
        ] {
            assert_eq!(
                super::round(label, has_previous),
                round,
                "{label} {has_previous}"
            );
        }
    }

    #[test]
    fn a_ledger_names_the_head_it_reviewed_and_the_findings_still_open() {
        let ledger = r#"{"head": "abc123", "findings": [
            {"severity": "blocking", "axis": "spec", "tag": "t", "location": "a:1", "summary": "one", "id": "f1", "status": "fixed"},
            {"severity": "advisory", "axis": "standards", "tag": "u", "location": "b:2", "summary": "two", "id": "f2", "status": "still_present"},
            {"severity": "blocking", "axis": "spec", "tag": "t", "location": "c:3", "summary": "three"}
        ]}"#;
        let previous = Previous::of_ledger(ledger).unwrap();
        assert_eq!(previous.head, "abc123");
        let open: Vec<(&str, &str, Status)> = previous
            .open
            .iter()
            .flatten()
            .map(|f| (f.id.as_str(), f.summary.as_str(), f.status))
            .collect();
        // A fixed finding is not open; the rest are renumbered for this round.
        assert_eq!(
            open,
            [("f1", "two", Status::New), ("f2", "three", Status::New)]
        );
        let bare = Previous::of_ledger(r#"{"head": "abc123", "findings": null}"#).unwrap();
        assert_eq!(bare.open, None);
        for ledger in ["", "not json", "{}", r#"{"head": ""}"#, r#"{"head": 4}"#] {
            assert_eq!(Previous::of_ledger(ledger), None, "{ledger}");
        }
    }

    #[test]
    fn labels_are_letters_digits_underscore_and_dash() {
        for good in ["t1", "t1-r2", "A_b-9"] {
            assert_eq!(check_label(good), Ok(()), "{good}");
        }
        for bad in ["", "t1/r1", "../x", "t 1", "t1.r1", "é"] {
            assert_eq!(check_label(bad), Err(BadLabel(bad.to_owned())), "{bad}");
        }
    }

    #[test]
    fn an_issue_ref_is_a_number_a_hash_number_or_an_issue_url() {
        for (spec, issue) in [
            ("12", Some("12")),
            ("#12", Some("12")),
            (
                "https://github.com/o/r/issues/12",
                Some("https://github.com/o/r/issues/12"),
            ),
            ("spec.md", None),
            ("#", None),
            ("", None),
            ("12a", None),
            ("-12", None),
            ("https://github.com/o/r/pull/12", None),
            ("https://github.com/o/r/issues/12/x", None),
            ("https://github.com//r/issues/12", None),
            ("https://example.com/o/r/issues/12", None),
        ] {
            assert_eq!(issue_ref(spec), issue, "{spec}");
        }
    }

    #[test]
    fn a_bundle_part_is_fenced_longer_than_any_backtick_run_inside_it() {
        let mut out = String::new();
        fenced(&mut out, "Spec", "a ````rust fence\n", "");
        assert_eq!(out, "\n## Spec\n\n`````\na ````rust fence\n`````\n");
        let mut out = String::new();
        fenced(&mut out, "Diff", "no newline", "diff");
        assert_eq!(out, "\n## Diff\n\n```diff\nno newline\n```\n");
    }

    #[test]
    fn the_ledger_decides_the_counts_and_the_verdict() {
        let report = "x\n```json\n{\"findings\": [\n\
            {\"severity\": \"advisory\", \"axis\": \"spec\", \"tag\": \"t\", \"location\": \"a:1\", \"summary\": \"s\"},\n\
            {\"severity\": \"blocking\", \"axis\": \"standards\", \"tag\": \"u\", \"location\": \"b:2\", \"summary\": \"v\"}\n\
            ]}\n```\nFINDINGS: standards_hard=0 tags=t,u\nVERDICT: clean\n";
        let reading = Reading::of(report).unwrap();
        assert_eq!(reading.verdict, Verdict::Findings);
        assert_eq!(
            reading.counts,
            Counts {
                blocking: 1,
                advisory: 1,
                question: 0
            }
        );
        assert_eq!(reading.blocking_lines(), ["- [standards/u] b:2: v"]);
        assert_eq!(reading.tallies, ["standards_hard=0", "tags=t,u"]);
        assert_eq!(
            reading.summary("t1", "r.md"),
            "review t1: findings (1 blocking, 1 advisory, 0 question) → r.md"
        );
    }

    #[test]
    fn an_empty_ledger_is_clean() {
        let reading = Reading::of("```json\n{\"findings\": []}\n```\n").unwrap();
        assert_eq!(reading.verdict, Verdict::Clean);
        assert_eq!(reading.findings, Some(vec![]));
    }

    #[test]
    fn without_a_readable_ledger_the_findings_and_verdict_lines_decide() {
        for (report, verdict, blocking, advisory, question) in [
            (
                "FINDINGS: standards_hard=1 standards_judgement=2 spec_findings=3 spec_advisory=4 questions=5\nVERDICT: findings\n",
                Verdict::Findings,
                4,
                6,
                5,
            ),
            ("VERDICT: clean.\n", Verdict::Clean, 0, 0, 0),
            (
                "```json\nnot json\n```\nVERDICT: findings\nVERDICT: clean\n",
                Verdict::Clean,
                0,
                0,
                0,
            ),
        ] {
            let reading = Reading::of(report).unwrap();
            assert_eq!(reading.verdict, verdict, "{report}");
            assert_eq!(
                reading.counts,
                Counts {
                    blocking,
                    advisory,
                    question
                },
                "{report}"
            );
            assert_eq!(reading.findings, None, "{report}");
        }
        for report in ["", "prose\n", "VERDICT: maybe\n"] {
            assert_eq!(Reading::of(report), Err(ReportError::NoVerdict), "{report}");
        }
    }

    fn entry(severity: &str, axis: &str, tag: &str, location: &str, extra: &str) -> String {
        format!(
            r#"{{"severity": "{severity}", "axis": "{axis}", "tag": "{tag}", "location": "{location}", "summary": "s"{extra}}}"#
        )
    }

    fn report_of(entries: &[String]) -> String {
        format!(
            "```json\n{{\"findings\": [{}]}}\n```\nVERDICT: findings\n",
            entries.join(",")
        )
    }

    #[test]
    fn a_fixed_finding_is_not_open_and_a_fix_rounds_ledger_counts_what_it_settled() {
        let report = report_of(&[
            entry(
                "blocking",
                "spec",
                "t",
                "a:1",
                r#", "id": "f1", "status": "fixed""#,
            ),
            entry(
                "blocking",
                "spec",
                "t",
                "b:2",
                r#", "id": "f2", "status": "still_present""#,
            ),
            entry("advisory", "standards", "u", "c:3", r#", "status": "new""#),
            entry("advisory", "standards", "u", "c:4", ""),
        ]);
        let reading = Reading::of(&report).unwrap();
        assert_eq!(reading.verdict, Verdict::Findings);
        assert_eq!(
            reading.counts,
            Counts {
                blocking: 1,
                advisory: 2,
                question: 0
            }
        );
        assert_eq!((reading.fixed, reading.still_present), (1, 1));
        assert_eq!(reading.blocking_lines(), ["- [spec/t] b:2: s"]);
        let only_fixed = Reading::of(&report_of(&[entry(
            "blocking",
            "spec",
            "t",
            "a:1",
            r#", "id": "f1", "status": "fixed""#,
        )]))
        .unwrap();
        assert_eq!(only_fixed.verdict, Verdict::Clean);
        assert_eq!(only_fixed.blocking_lines(), Vec::<String>::new());
    }

    const FIX: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 1..2 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,4 +10,5 @@ fn x() {
 context
-gone
+new one
+new two
 context
 context
diff --git a/old.rs b/old.rs
deleted file mode 100644
--- a/old.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-x
+y
";

    #[test]
    fn a_location_is_touched_when_the_fix_changed_or_deleted_a_line_there() {
        let fix = Fix::of_diff(FIX);
        for (location, touched) in [
            ("src/lib.rs:11", true),
            ("src/lib.rs:12", true),
            ("src/lib.rs:13", false),
            ("src/lib.rs:10", false),
            ("src/lib.rs:9-11", true),
            ("src/lib.rs:12-30", true),
            ("src/lib.rs:13-30", false),
            ("src/lib.rs:12:5", true),
            ("./src/lib.rs:12", true),
            ("`src/lib.rs:12`", true),
            ("src/lib.rs", true),
            ("README.md:1", true),
            ("README.md:2", false),
            ("old.rs:1", false),
            ("other.rs:1", false),
            ("", false),
            ("spec", false),
        ] {
            assert_eq!(fix.touches(location), touched, "{location}");
        }
    }

    #[test]
    fn a_new_finding_off_the_fix_is_dropped_unless_hard_or_security() {
        let previous = Previous::of_ledger(&format!(
            r#"{{"head": "h", "findings": [{}, {}, {}]}}"#,
            entry("blocking", "spec", "t", "a:1", ""),
            entry("blocking", "spec", "t", "b:2", ""),
            entry("advisory", "standards", "u", "c:3", "")
        ))
        .unwrap();
        let open = previous.open.unwrap();
        let report = report_of(&[
            entry(
                "blocking",
                "spec",
                "t",
                "a:1",
                r#", "id": "f1", "status": "fixed""#,
            ),
            // f2 is not answered: it stays open. f3 is answered.
            entry(
                "advisory",
                "standards",
                "u",
                "c:3",
                r#", "id": "f3", "status": "still_present""#,
            ),
            entry(
                "blocking",
                "spec",
                "partial-requirement",
                "src/lib.rs:11",
                "",
            ),
            entry(
                "blocking",
                "spec",
                "partial-requirement",
                "src/lib.rs:90",
                "",
            ),
            entry(
                "advisory",
                "standards",
                "mysterious-name",
                "elsewhere.rs:1",
                "",
            ),
            entry(
                "blocking",
                "standards",
                "missing-test",
                "elsewhere.rs:2",
                "",
            ),
            entry("blocking", "spec", "security", "elsewhere.rs:3", ""),
            entry("question", "spec", "spec-conflict", "", ""),
        ]);

        let reading = Reading::of(&report)
            .unwrap()
            .within(&open, &Fix::of_diff(FIX));

        let kept: Vec<&str> = reading
            .findings
            .iter()
            .flatten()
            .map(|f| f.location.as_str())
            .collect();
        assert_eq!(
            kept,
            [
                "a:1",
                "c:3",
                "src/lib.rs:11",
                "elsewhere.rs:2",
                "elsewhere.rs:3",
                "b:2"
            ]
        );
        let dropped: Vec<&str> = reading
            .dropped
            .iter()
            .map(|f| f.location.as_str())
            .collect();
        assert_eq!(dropped, ["src/lib.rs:90", "elsewhere.rs:1", ""]);
        assert_eq!((reading.fixed, reading.still_present), (1, 2));
        assert_eq!(
            reading.counts,
            Counts {
                blocking: 4,
                advisory: 1,
                question: 0
            }
        );
        assert_eq!(reading.verdict, Verdict::Findings);
        let carried = reading.findings.unwrap().pop().unwrap();
        assert_eq!(
            (carried.id.as_str(), carried.status),
            ("f2", Status::StillPresent)
        );
    }

    fn record<'a>(result: Result<&'a Reading, &'a str>) -> Record<'a> {
        Record {
            repo: "repo",
            label: "t1-r1",
            round: 1,
            base: "main",
            head: "abc",
            spec: "none",
            backend: "codex",
            model: Some("m"),
            effort: None,
            duration_s: 7,
            fallback_from: "",
            skipped: "agy",
            report: "r.md",
            stream: "r.jsonl",
            findings: "r.findings.json",
            from: "",
            result,
        }
    }

    #[test]
    fn an_ok_record_carries_its_counts_and_the_findings_words() {
        let reading =
            Reading::of("FINDINGS: standards_hard=9\nFINDINGS: standards_hard=1 junk =x outcome=lie Bad=1 tags=x\nVERDICT: findings\n").unwrap();
        let pairs = record(Ok(&reading)).pairs();
        for pair in [
            "outcome=ok",
            "verdict=findings",
            "reason=",
            "reviewer=codex",
            "backend=codex",
            "model=m",
            "effort=",
            "duration_s=7",
            "skipped=agy",
            "findings=r.findings.json",
            "blocking=1",
            "standards_hard=1",
            "tags=x",
        ] {
            assert!(pairs.iter().any(|p| p == pair), "{pair} in {pairs:?}");
        }
        for dropped in ["junk", "=x", "outcome=lie", "Bad=1", "standards_hard=9"] {
            assert!(
                !pairs.iter().any(|p| p == dropped),
                "{dropped} in {pairs:?}"
            );
        }
    }

    #[test]
    fn a_failed_record_carries_its_reason_and_no_verdict() {
        let pairs = record(Err("codex exited 1")).pairs();
        assert!(pairs.iter().any(|p| p == "outcome=error"), "{pairs:?}");
        assert!(pairs.iter().any(|p| p == "verdict="), "{pairs:?}");
        assert!(
            pairs.iter().any(|p| p == "reason=codex exited 1"),
            "{pairs:?}"
        );
        assert!(
            !pairs.iter().any(|p| p.starts_with("blocking=")),
            "{pairs:?}"
        );
    }
}
