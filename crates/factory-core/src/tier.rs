//! Review tiers: how hard a diff is reviewed, and which chain of reviewers runs it. The caller
//! tags a ticket `hard`, `standard` or `trivial`; a spec's own review (`spec-*`) is the
//! `spec` tier whatever the caller names, with a chain of its own. A small or docs-only diff is
//! trivial whatever else it was tagged, but hard or spec.

use std::fmt;

/// The most lines a docs-and-config-only diff may change and still be trivial.
const LIGHT_LINES: u64 = 400;
/// The most lines any diff without a test may change and still be trivial.
const TINY_LINES: u64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    Hard,
    Standard,
    Trivial,
    /// A spec's own review: as hard as `Hard`, on its own chain. No caller names it.
    Spec,
}

impl Tier {
    /// The tiers a caller may name; `Spec` is read from the label.
    pub const NAMED: [Tier; 3] = [Tier::Hard, Tier::Standard, Tier::Trivial];

    /// Every tier, each with a chain of its own.
    pub const ALL: [Tier; 4] = [Tier::Hard, Tier::Standard, Tier::Trivial, Tier::Spec];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Hard => "hard",
            Tier::Standard => "standard",
            Tier::Trivial => "trivial",
            Tier::Spec => "spec",
        }
    }

    /// The tier a caller names, or none for a word that is no tier.
    pub fn parse(word: &str) -> Option<Tier> {
        Tier::NAMED.into_iter().find(|tier| tier.as_str() == word)
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a review's tier came from, as its event's `tier_source` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A spec review (`spec-*`), which has its own tier.
    Spec,
    /// The caller's `--tier`.
    Caller,
    /// The diff is small or docs only.
    Detected,
    /// No tier was named and the diff is not trivial: standard.
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Spec => "spec",
            Source::Caller => "caller",
            Source::Detected => "detected",
            Source::Default => "default",
        }
    }
}

/// One file of a diff, as `git diff --numstat` counts it (a binary file counts 0 lines).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub added: u64,
    pub deleted: u64,
    pub path: String,
}

/// Reads `git diff --numstat`: one `<added>\t<deleted>\t<path>` per file, `-` for a binary's
/// counts.
pub fn numstat(text: &str) -> Vec<Change> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let count = |field: Option<&str>| field.map(|n| n.parse().unwrap_or(0));
            Some(Change {
                added: count(fields.next())?,
                deleted: count(fields.next())?,
                path: fields.next()?.to_owned(),
            })
        })
        .collect()
}

/// The tier a review runs at: a spec review is spec, whatever the diff or the caller; a
/// caller's `hard` or `trivial` stands; a trivial diff is trivial; else the caller's tier, or standard.
pub fn tier(label: &str, caller: Option<Tier>, diff: &[Change]) -> (Tier, Source) {
    match caller {
        _ if label.starts_with("spec-") => (Tier::Spec, Source::Spec),
        Some(tier @ (Tier::Hard | Tier::Trivial)) => (tier, Source::Caller),
        _ if is_trivial(diff) => (Tier::Trivial, Source::Detected),
        Some(tier) => (tier, Source::Caller),
        None => (Tier::Standard, Source::Default),
    }
}

/// Whether a diff is trivial: only docs and config, at most 400 lines; or at most 5 lines
/// touching no test.
pub fn is_trivial(diff: &[Change]) -> bool {
    let lines: u64 = diff
        .iter()
        .map(|change| change.added + change.deleted)
        .sum();
    let light = diff.iter().all(|change| is_light(&change.path));
    let tests = diff.iter().any(|change| is_test(&change.path));
    (light && lines <= LIGHT_LINES) || (lines <= TINY_LINES && !tests)
}

/// Whether a path is a test: a `.test.`, `.spec.` or `_test.` file, or one under a
/// `test`, `tests`, `__tests__` or `e2e` directory.
pub fn is_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let dirs: Vec<&str> = path.split('/').collect();
    let dirs = &dirs[..dirs.len() - 1];
    [".test.", ".spec.", "_test."]
        .iter()
        .any(|mark| name.contains(mark))
        || dirs
            .iter()
            .any(|dir| matches!(*dir, "test" | "tests" | "__tests__" | "e2e"))
}

/// Whether a path is docs or config, light to review. Prompts and agent steering are
/// behaviour, and tests carry the flake rules (orphan-race-r1: 16 lines of test and flake
/// log, 2 hard findings), so neither is.
pub fn is_light(path: &str) -> bool {
    if is_test(path) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let steering = matches!(name, "SKILL.md" | "CLAUDE.md" | "AGENTS.md" | "REVIEWER.md")
        || path.contains("/.claude/agents/")
        || path.starts_with(".claude/agents/");
    if steering {
        return false;
    }
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    let docs = matches!(extension, Some("md" | "mdx" | "txt" | "rst" | "adoc"));
    let config = matches!(
        extension,
        Some("json" | "jsonc" | "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg")
    );
    let rc = name == ".editorconfig" || (name.starts_with('.') && name.ends_with("rc"));
    docs || config || rc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(files: &[(u64, &str)]) -> Vec<Change> {
        files
            .iter()
            .map(|(lines, path)| Change {
                added: *lines,
                deleted: 0,
                path: (*path).to_owned(),
            })
            .collect()
    }

    #[test]
    fn numstat_reads_counts_and_paths_and_a_binary_counts_nothing() {
        assert_eq!(
            numstat("3\t1\tsrc/a b.rs\n-\t-\tlogo.png\n"),
            vec![
                Change {
                    added: 3,
                    deleted: 1,
                    path: "src/a b.rs".to_owned()
                },
                Change {
                    added: 0,
                    deleted: 0,
                    path: "logo.png".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_docs_or_config_diff_up_to_400_lines_is_trivial() {
        assert!(is_trivial(&diff(&[(100, "notes.md")])));
        assert!(is_trivial(&diff(&[
            (300, "docs/a.md"),
            (100, "Cargo.toml")
        ])));
        assert!(!is_trivial(&diff(&[(401, "docs/a.md")])));
    }

    #[test]
    fn up_to_5_lines_outside_tests_is_trivial_and_6_is_not() {
        assert!(is_trivial(&diff(&[(5, "src.ts")])));
        assert!(!is_trivial(&diff(&[(6, "src.ts")])));
        assert!(!is_trivial(&diff(&[(3, "test/a.test.ts")])));
        assert!(!is_trivial(&diff(&[(3, "crates/x/tests/cli.rs")])));
    }

    #[test]
    fn a_caller_cannot_name_the_spec_tier() {
        assert_eq!(Tier::parse("spec"), None);
        assert_eq!(Tier::parse("hard"), Some(Tier::Hard));
    }

    #[test]
    fn steering_and_tests_are_never_light() {
        for path in [
            "SKILL.md",
            "a/SKILL.md",
            "CLAUDE.md",
            "x/AGENTS.md",
            "agent-skills/REVIEWER.md",
            "h/.claude/agents/i.md",
            "docs/a.test.md",
        ] {
            assert!(!is_light(path), "{path}");
        }
        for path in [
            "README.md",
            "a.toml",
            "b.yml",
            ".bashrc",
            "x/.npmrc",
            ".editorconfig",
            "c.txt",
        ] {
            assert!(is_light(path), "{path}");
        }
        assert!(!is_light("src/main.rs"));
    }

    #[test]
    fn a_spec_review_is_its_own_tier_and_a_callers_hard_or_trivial_stands() {
        let code = diff(&[(40, "src/a.ts")]);
        let tiny = diff(&[(5, "src.ts")]);
        for caller in [
            None,
            Some(Tier::Hard),
            Some(Tier::Standard),
            Some(Tier::Trivial),
        ] {
            assert_eq!(
                tier("spec-r1", caller, &tiny),
                (Tier::Spec, Source::Spec),
                "{caller:?}"
            );
        }
        assert_eq!(Tier::Spec.as_str(), "spec");
        assert_eq!(
            tier("t7-r1", Some(Tier::Hard), &tiny),
            (Tier::Hard, Source::Caller)
        );
        assert_eq!(
            tier("t7-r1", Some(Tier::Trivial), &code),
            (Tier::Trivial, Source::Caller)
        );
    }

    #[test]
    fn a_trivial_diff_is_trivial_over_a_standard_tag_and_else_the_tag_or_standard() {
        let code = diff(&[(40, "src/a.ts")]);
        let tiny = diff(&[(5, "src.ts")]);
        assert_eq!(
            tier("t1-r1", Some(Tier::Standard), &tiny),
            (Tier::Trivial, Source::Detected)
        );
        assert_eq!(
            tier("t1-r1", None, &tiny),
            (Tier::Trivial, Source::Detected)
        );
        assert_eq!(
            tier("t1-r1", Some(Tier::Standard), &code),
            (Tier::Standard, Source::Caller)
        );
        assert_eq!(
            tier("t1-r1", None, &code),
            (Tier::Standard, Source::Default)
        );
    }
}
