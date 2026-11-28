//! The guards over a command line's shell text: what would burn the CPU every agent and CI on
//! this host share, what stops on an approval dialog nobody answers, and a JS object run as a
//! command. Each looks at what the line says and lets through what it cannot read: a false
//! positive stops every agent on the machine.

use super::js;
use super::shell::{self, Invocation, Part};

/// The capped scope a CPU-heavy command runs in on this host.
const CAPPED: &str = "systemd-run --user --scope -p CPUQuota=200% -- <cmd>";

/// The refusal for a command line one of the guards stops, with the fix in it.
pub fn shell_trap(command: &str) -> Option<String> {
    trap_in(command, false)
}

/// The refusal for a REPL script whose shell text one of the guards stops, naming its line. Text
/// the script doesn't say, such as a `${{raw: s}}` splice, is let through.
pub fn repl_trap(code: &str) -> Option<String> {
    let calls = js::shell_calls(code);
    if let Some(reason) = repl_waits(&calls) {
        return Some(reason);
    }
    calls.iter().filter(|call| call.exact).find_map(|call| {
        let reason = shell_trap(&call.text)?;
        Some(format!("REPL line {}: {reason}", call.line))
    })
}

/// The time a `git push` needs in this repository's REPL calls: the pre-push hook runs verify.
const PUSH_TIME_MS: u64 = 590_000;

/// The context for a REPL script that pushes with a `.timeout` the pre-push verify outlasts.
pub fn repl_hint(code: &str) -> Option<String> {
    let short = |call: &&js::ShellCall| call.timeout.is_some_and(|ms| ms < PUSH_TIME_MS);
    let pushes = |call: &&js::ShellCall| {
        super::git_actions(&call.text)
            .iter()
            .any(|action| action.push.is_some())
    };
    let call = js::shell_calls(code)
        .into_iter()
        .find(|call| short(&call) && pushes(&call))?;
    Some(format!(
        "REPL line {}: the pre-push hook runs verify (212-290 s); give the push .timeout(590000) and read its output file if it moves to the background.",
        call.line
    ))
}

/// The refusal for a script that runs `factory wait` more than once: from a loop, a callback or a
/// timer, or twice in its text. A wait outlives the REPL call's own time, and the script's
/// result reaches nobody while it waits.
fn repl_waits(calls: &[js::ShellCall]) -> Option<String> {
    let mut seen = 0;
    for call in calls {
        let waits = waits_in(&call.text);
        seen += waits;
        let why = if waits > 0 && call.repeated {
            "runs factory wait from a loop, a callback or a timer"
        } else if seen > 1 {
            "runs a second factory wait"
        } else {
            continue;
        };
        return Some(format!(
            "REPL line {}: this script {why}; one wait per REPL call, or factory wait lead --any.",
            call.line
        ));
    }
    None
}

/// How many `factory wait` commands a command line runs.
fn waits_in(command: &str) -> usize {
    super::commands(command)
        .iter()
        .filter(|words| {
            shell::invocation(words).is_some_and(|call| {
                call.program == "factory" && call.args.first().is_some_and(|a| a == "wait")
            })
        })
        .count()
}

/// [`shell_trap`] over `command`, which is the script of a `bash -c` when `in_script`.
fn trap_in(command: &str, in_script: bool) -> Option<String> {
    let line = shell::line(command);
    let parts = &line.parts;
    if endless_empty_loop(parts) {
        return Some(burns("an endless loop that never waits"));
    }
    for (at, part) in parts.iter().enumerate() {
        let Part::Command(words) = part else {
            continue;
        };
        if words[0].starts_with("[object") {
            return Some(OBJECT.to_owned());
        }
        let Some(call) = shell::invocation(words) else {
            continue;
        };
        if let Some(script) = shell::shell_script(&call) {
            if let Some(reason) = trap_in(script, true) {
                return Some(reason);
            }
            continue;
        }
        if let Some(what) = burner(&call, backgrounded(parts, at)) {
            return Some(burns(what));
        }
        // The word index of the first argument, as the line's marks count words.
        let first = words.len() - call.args.len();
        if call.program == "rm"
            && (in_script || rm_targets(call.args).any(|i| line.expands(at, first + i)))
        {
            return Some(RM_BAIT.to_owned());
        }
        if quoted_anchor(&call, |i| line.quoted_dollar(at, words.len() - i)) {
            return Some(QUOTED_ANCHOR.to_owned());
        }
    }
    None
}

/// The refusal for an rm that stops on an approval dialog, which bypass mode can't skip.
const RM_BAIT: &str = "An rm on a variable, glob or $(…), or inside bash -c, stops on an approval dialog nobody answers: run rm on a literal absolute path, as a command of its own.";

/// Which of an rm's arguments it removes: those that are no options.
fn rm_targets(args: &[String]) -> impl Iterator<Item = usize> + '_ {
    let end = args.iter().position(|a| a == "--");
    args.iter()
        .enumerate()
        .filter_map(move |(at, arg)| match end {
            Some(end) if at > end => Some(at),
            Some(end) if at == end => None,
            _ => (!arg.starts_with('-')).then_some(at),
        })
}

/// The refusal for a command that is a JS object's `[object Object]`.
const OBJECT: &str = "A JS object was interpolated into the command; pass a string or array.";

/// The refusal for a grep pattern whose `$` sits inside shell quotes.
const QUOTED_ANCHOR: &str = "A $ inside a quoted grep or rg pattern stops on an approval dialog nobody answers: use rg -F 'text' for a fixed string, or rg -e foo$ with the anchor outside shell quoting.";

/// The short options that take a value, attached or as the next word, of grep, rg and git grep.
const GREP_SHORT_VALUES: &str = "efmABCdD";
const RG_SHORT_VALUES: &str = "efgmtTjMABCErd";
const GIT_GREP_SHORT_VALUES: &str = "efmABC";

/// Their long options that take the next word as their value when no `=` gives it.
const GREP_LONG_VALUES: &[&str] = &[
    "after-context",
    "before-context",
    "binary-files",
    "color",
    "colors",
    "context",
    "context-separator",
    "devices",
    "dfa-size-limit",
    "directories",
    "encoding",
    "engine",
    "exclude",
    "exclude-dir",
    "exclude-from",
    "field-context-separator",
    "field-match-separator",
    "glob",
    "group-separator",
    "hostname-bin",
    "hyperlink-format",
    "iglob",
    "ignore-file",
    "include",
    "label",
    "max-columns",
    "max-count",
    "max-depth",
    "max-filesize",
    "path-separator",
    "pre",
    "pre-glob",
    "regex-size-limit",
    "replace",
    "sort",
    "sortr",
    "threads",
    "type",
    "type-add",
    "type-clear",
    "type-not",
];

/// Whether a grep, egrep, rg or git grep call has a pattern with a quoted `$`, unless it matches
/// fixed strings. `quoted` says it of the argument that many words from the command's end.
fn quoted_anchor(call: &Invocation<'_>, quoted: impl Fn(usize) -> bool) -> bool {
    let (args, short_values) = match call.program {
        "grep" | "egrep" => (call.args, GREP_SHORT_VALUES),
        "rg" => (call.args, RG_SHORT_VALUES),
        "git" => match super::GitGlobals::of(call, None).rest.split_first() {
            Some((sub, rest)) if sub == "grep" => (rest, GIT_GREP_SHORT_VALUES),
            _ => return false,
        },
        _ => return false,
    };
    patterns(args, short_values).is_some_and(|patterns| {
        patterns
            .into_iter()
            .any(|at| at < args.len() && quoted(args.len() - at))
    })
}

/// Which of a grep's `args` are patterns: the values of `-e` and `--regexp`, or else its first
/// word that is no option nor an option's value. None when it matches fixed strings.
fn patterns(args: &[String], short_values: &str) -> Option<Vec<usize>> {
    let mut patterns = Vec::new();
    let mut positional = None;
    // Patterns come from `-e` or `-f`, so the words that are no options are all paths.
    let mut given = false;
    let mut fixed = false;
    let mut options_end = false;
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        at += 1;
        if options_end || !arg.starts_with('-') || arg == "-" {
            positional.get_or_insert(at - 1);
        } else if arg == "--" {
            options_end = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            fixed |= name == "fixed-strings";
            given |= matches!(name, "regexp" | "file");
            let takes = matches!(name, "regexp" | "file") || GREP_LONG_VALUES.contains(&name);
            match (name, value) {
                ("regexp", Some(_)) => patterns.push(at - 1),
                ("regexp", None) => patterns.push(at),
                _ => {}
            }
            at += usize::from(takes && value.is_none());
        } else {
            let group = &arg[1..];
            for (pos, flag) in group.char_indices() {
                fixed |= flag == 'F';
                if !short_values.contains(flag) {
                    continue;
                }
                let attached = pos + flag.len_utf8() < group.len();
                given |= matches!(flag, 'e' | 'f');
                if flag == 'e' {
                    patterns.push(if attached { at - 1 } else { at });
                }
                at += usize::from(!attached);
                break;
            }
        }
    }
    if fixed {
        return None;
    }
    if !given {
        patterns.extend(positional);
    }
    Some(patterns)
}

fn burns(what: &str) -> String {
    format!(
        "{what} burns the CPU every agent and CI on this host share; run it in a capped scope: {CAPPED}."
    )
}

/// The CPU burner a command runs, unless it runs in a CPU-capped scope.
fn burner(call: &Invocation<'_>, backgrounded: bool) -> Option<&'static str> {
    let args = call.args;
    match call.program {
        "systemd-run" => {
            let (capped, command) = scoped(args);
            if capped {
                return None;
            }
            burner(&shell::invocation(command)?, backgrounded)
        }
        "yes" if backgrounded || to_null(args) => Some("yes on its own"),
        "stress" | "stress-ng" if !asks_about_itself(args) => Some("a stress run"),
        "openssl" if args.first().is_some_and(|a| a == "speed") => Some("openssl speed"),
        _ => None,
    }
}

/// `--help` or `--version`, which run nothing.
fn asks_about_itself(args: &[String]) -> bool {
    args.iter()
        .any(|a| matches!(a.as_str(), "--help" | "-h" | "--version" | "-V"))
}

/// Whether the command's standard output goes to /dev/null.
fn to_null(args: &[String]) -> bool {
    const TO: &[&str] = &["&>>", "1>>", ">>", "&>", "1>", ">|", ">"];
    args.iter().enumerate().any(|(at, arg)| {
        TO.iter().any(|to| match arg.strip_prefix(to) {
            Some("") => args.get(at + 1).is_some_and(|next| next == "/dev/null"),
            Some(rest) => rest == "/dev/null",
            None => false,
        })
    })
}

/// Whether the command at `at` runs in the background: its and-or list ends in `&`, and no
/// pipe reads what it prints.
fn backgrounded(parts: &[Part], at: usize) -> bool {
    for part in &parts[at + 1..] {
        match part {
            Part::Background => return true,
            Part::Pipe | Part::Sequence => return false,
            Part::Command(_) | Part::Enter | Part::Leave => {}
        }
    }
    false
}

/// systemd-run's options that take the next word as their value.
const SYSTEMD_RUN_VALUES: &[&str] = &[
    "-p",
    "--property",
    "-u",
    "--unit",
    "--description",
    "--slice",
    "-E",
    "--setenv",
    "--uid",
    "--gid",
    "-M",
    "--machine",
    "-H",
    "--host",
    "--nice",
    "--working-directory",
];

/// Whether systemd-run's options set a CPU quota on what it runs, and the command it runs. An
/// option's value, such as a `--description`, sets nothing.
fn scoped(args: &[String]) -> (bool, &[String]) {
    let quota = |value: &str| value.starts_with("CPUQuota=");
    let mut capped = false;
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        if arg == "--" {
            return (capped, &args[at + 1..]);
        }
        if !arg.starts_with('-') {
            break;
        }
        let value = args.get(at + 1);
        capped |= match arg.as_str() {
            "-p" | "--property" => value.is_some_and(|v| quota(v)),
            other => other
                .strip_prefix("--property=")
                .or_else(|| other.strip_prefix("-p"))
                .is_some_and(quota),
        };
        at += if SYSTEMD_RUN_VALUES.contains(&arg.as_str()) {
            2
        } else {
            1
        };
    }
    (capped, &args[at.min(args.len())..])
}

/// A word of the line, and whether it starts its command; the operators between commands are
/// words too.
type Token<'a> = (&'a str, bool);

/// Whether the line runs a loop that never ends and never waits: `while :`, `while true`,
/// `while ((1))`, `until false` or `for ((;;))`, whose body does only `:`, `true`, `let` or
/// `((…))` arithmetic.
fn endless_empty_loop(parts: &[Part]) -> bool {
    let mut tokens: Vec<Token<'_>> = Vec::new();
    for part in parts {
        let operator = match part {
            Part::Command(words) => {
                tokens.extend(words.iter().enumerate().map(|(i, w)| (w.as_str(), i == 0)));
                continue;
            }
            Part::Sequence => ";",
            Part::Enter => "(",
            Part::Leave => ")",
            Part::Pipe => "|",
            Part::Background => "&",
        };
        tokens.push((operator, false));
    }
    (0..tokens.len()).any(|at| endless_loop_at(&tokens[at..]))
}

/// `tokens` after its first `n` when they read `words` (`*` standing for any one word).
fn after<'t, 'a>(tokens: &'t [Token<'a>], words: &[&str]) -> Option<&'t [Token<'a>]> {
    let head = tokens.get(..words.len())?;
    let matches = head
        .iter()
        .zip(words)
        .all(|((word, _), want)| *want == "*" || word == want);
    matches.then(|| &tokens[words.len()..])
}

fn endless_loop_at(tokens: &[Token<'_>]) -> bool {
    if !tokens.first().is_some_and(|(_, starts)| *starts) {
        return false;
    }
    const HEADS: &[&[&str]] = &[
        &["while", ":", ";"],
        &["while", "true", ";"],
        &["until", "false", ";"],
        &["while", "(", "(", "1", ")", ")", ";"],
        &["for", "(", "(", ";", ";", ")", ")", ";"],
        &["for", "(", "(", ";", ";", ")", ")"],
    ];
    let Some(rest) = HEADS.iter().find_map(|head| after(tokens, head)) else {
        return false;
    };
    let Some(body) = after(rest, &["do"]) else {
        return false;
    };
    let mut body = after(body, &[";"]).unwrap_or(body);
    let mut idle = false;
    loop {
        if after(body, &["done"]).is_some() {
            return idle;
        }
        let Some(more) = idle_step(body) else {
            return false;
        };
        (body, idle) = (more, true);
    }
}

/// What follows one command of a loop body that does no more than spin, with its `;`: `:`,
/// `true`, `let …` or `((…))`.
fn idle_step<'t, 'a>(body: &'t [Token<'a>]) -> Option<&'t [Token<'a>]> {
    let end = body.iter().position(|(word, _)| *word == ";")?;
    let (command, rest) = (&body[..end], &body[end + 1..]);
    let idle = match command {
        [(":" | "true", _)] => true,
        [("let", _), args @ ..] => !args.is_empty(),
        [("(", _), ("(", _), inner @ .., (")", _), (")", _)] => {
            !inner.is_empty() && inner.iter().all(|(word, _)| !matches!(*word, "(" | ")"))
        }
        _ => false,
    };
    idle.then_some(rest)
}
