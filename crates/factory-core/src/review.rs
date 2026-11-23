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
pub fn prompt(opening: &str, bundle: &str, spec: &str, previous: Option<&str>) -> String {
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
}

impl Finding {
    /// The finding as the one line a caller reads.
    pub fn line(&self) -> String {
        format!(
            "- [{}/{}] {}: {}",
            self.axis, self.tag, self.location, self.summary
        )
    }

    /// The finding as its findings-ledger entry.
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "severity": self.severity.as_str(),
            "axis": self.axis,
            "tag": self.tag,
            "location": self.location,
            "summary": self.summary,
        })
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
            return Ok(Reading {
                verdict,
                counts,
                findings: Some(findings),
                tallies,
            });
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
            tallies,
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
    pub report: &'a str,
    pub stream: &'a str,
    /// The run's findings ledger file.
    pub findings: &'a str,
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
            format!("model={}", self.model.unwrap_or_default()),
            format!("effort={}", self.effort.unwrap_or_default()),
            format!("report={}", self.report),
            format!("stream={}", self.stream),
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
            report: "r.md",
            stream: "r.jsonl",
            findings: "r.findings.json",
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
