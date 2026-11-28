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
        return Some(burns("an endless empty loop"));
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
        if call.program == "rm" && (in_script || rm_targets(call.args).any(|w| non_literal(w))) {
            return Some(RM_BAIT.to_owned());
        }
        let quoted = |word: usize| line.quoted_dollar(at, word);
        if quoted_anchor(&call, words.len(), quoted) {
            return Some(QUOTED_ANCHOR.to_owned());
        }
    }
    None
}

/// The refusal for an rm that stops on an approval dialog, which bypass mode can't skip.
const RM_BAIT: &str = "An rm on a variable, glob or $(…), or inside bash -c, stops on an approval dialog nobody answers: run rm on a literal absolute path, as a command of its own.";

/// What an rm removes: its words that are no options.
fn rm_targets(args: &[String]) -> impl Iterator<Item = &String> {
    let end = args.iter().position(|a| a == "--");
    args.iter()
        .enumerate()
        .filter_map(move |(at, arg)| match end {
            Some(end) if at > end => Some(arg),
            Some(end) if at == end => None,
            _ => (!arg.starts_with('-')).then_some(arg),
        })
}

/// Whether a word is only known when it runs: a variable, a command substitution or a glob.
fn non_literal(word: &str) -> bool {
    word.contains(['$', '`', '*', '?', '['])
}

/// The refusal for a command that is a JS object's `[object Object]`.
const OBJECT: &str = "A JS object was interpolated into the command; pass a string or array.";

/// The refusal for a grep pattern whose `$` sits inside shell quotes.
const QUOTED_ANCHOR: &str = "A $ inside a quoted grep or rg pattern stops on an approval dialog nobody answers: use rg -F 'text' for a fixed string, or rg -e foo$ with the anchor outside shell quoting.";

/// Whether a grep, egrep, rg or git grep call has a pattern with a quoted `$` (`quoted` says
/// which of the command's `words` do), unless it matches fixed strings. Its patterns are the
/// values of `-e` and `--regexp` and the words that are no options.
fn quoted_anchor(call: &Invocation<'_>, words: usize, quoted: impl Fn(usize) -> bool) -> bool {
    let args = match call.program {
        "grep" | "egrep" | "rg" => call.args,
        "git" => match super::GitGlobals::of(call, None).rest.split_first() {
            Some((sub, rest)) if sub == "grep" => rest,
            _ => return false,
        },
        _ => return false,
    };
    // The word index of the first of `args`, which end the command.
    let first = words - args.len();
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let fixed = args[..end].iter().any(|arg| {
        arg == "--fixed-strings"
            || (!arg.starts_with("--") && arg.starts_with('-') && arg.contains('F'))
    });
    if fixed {
        return false;
    }
    let mut value_of_e = false;
    args.iter().enumerate().any(|(at, arg)| {
        let pattern = std::mem::take(&mut value_of_e)
            || at > end
            || !arg.starts_with('-')
            || arg.starts_with("--regexp=")
            || (arg.starts_with("-e") && arg.len() > 2);
        value_of_e = at < end && (arg == "-e" || arg == "--regexp");
        pattern && quoted(first + at)
    })
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
            let (options, command) = scoped(args);
            if capped(options) {
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

/// systemd-run's own options, and the command it runs.
fn scoped(args: &[String]) -> (&[String], &[String]) {
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        if arg == "--" {
            return (&args[..at], &args[at + 1..]);
        }
        if !arg.starts_with('-') {
            break;
        }
        at += if SYSTEMD_RUN_VALUES.contains(&arg.as_str()) {
            2
        } else {
            1
        };
    }
    let at = at.min(args.len());
    (&args[..at], &args[at..])
}

/// Whether systemd-run's options set a CPU quota on what it runs.
fn capped(options: &[String]) -> bool {
    let quota = |value: &str| value.starts_with("CPUQuota=");
    options.iter().enumerate().any(|(at, option)| {
        let next = || options.get(at + 1).is_some_and(|v| quota(v));
        match option.as_str() {
            "-p" | "--property" => next(),
            other => other
                .strip_prefix("--property=")
                .or_else(|| other.strip_prefix("-p"))
                .is_some_and(quota),
        }
    })
}

/// Whether the line runs a loop that never ends and does nothing in its body:
/// `while :; do :; done`, `while true`, `until false` or `for ((;;))`.
fn endless_empty_loop(parts: &[Part]) -> bool {
    // Each word, and whether it starts its command; the operators between commands as words.
    let mut tokens: Vec<(&str, bool)> = Vec::new();
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
    (0..tokens.len()).any(|at| {
        let words: Vec<&str> = tokens[at..].iter().map(|(word, _)| *word).collect();
        tokens[at].1 && endless_empty_at(&words)
    })
}

fn endless_empty_at(words: &[&str]) -> bool {
    let rest = match words {
        ["while", ":" | "true", ";", rest @ ..] | ["until", "false", ";", rest @ ..] => rest,
        ["for", "(", "(", ";", ";", ")", ")", rest @ ..] => {
            rest.strip_prefix(&[";"]).unwrap_or(rest)
        }
        _ => return false,
    };
    let Some(body) = rest.strip_prefix(&["do"]) else {
        return false;
    };
    let mut body = body.strip_prefix(&[";"]).unwrap_or(body);
    let mut empty = false;
    loop {
        match body {
            [":" | "true", ";", more @ ..] => {
                empty = true;
                body = more;
            }
            ["done", ..] => return empty,
            _ => return false,
        }
    }
}
