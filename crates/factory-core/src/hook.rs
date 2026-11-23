//! The rules Claude Code's hooks apply (spec #1, "Hooks").
//!
//! Each rule is a pure function of the hook's input, so a table can drive it. The binary reads
//! stdin, asks the rule, and answers in Claude Code's hook protocol. A rule that needs the
//! outside world, such as the staged diff, hands back what it needs instead of fetching it.

mod js;
mod shell;

use std::collections::BTreeMap;

use serde_json::Value;

/// The variable that lets one call use a JavaScript package manager: set in the environment the
/// hook runs in, or in front of the command itself.
pub const ESCAPE: &str = "FACTORY_ALLOW_NPM";

/// What the pre-tool hook decides about a tool call.
#[derive(Debug, PartialEq, Eq)]
pub enum PreTool {
    Allow,
    /// Refuse the call; the line says what to do instead.
    Block(String),
    /// The call commits or pushes: the caller checks the staged diff with [`private_address`]
    /// and blocks the call if it leaks.
    CheckStaged(GitAction),
}

/// Whether a git command records or publishes work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitKind {
    Commit,
    Push,
}

/// A commit or push in a command, and where its diff to check is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitAction {
    pub kind: GitKind,
    /// What the command stages itself, with `commit -a` or an earlier `git add`, is not staged yet
    /// when the hook runs: the diff to check is the whole working tree against HEAD.
    pub all: bool,
    /// The directory of `git -C <dir>`, relative to the call's working directory.
    pub dir: Option<String>,
}

/// The pre-tool rules over Claude Code's hook input. `escape` says the escape variable is set
/// in the hook's own environment.
pub fn pre_tool(input: &Value, escape: bool) -> PreTool {
    let tool = input.get("tool_name").and_then(Value::as_str);
    let params = input.get("tool_input");
    let text = |key: &str| params?.get(key)?.as_str();
    match tool {
        Some("Bash") => {
            let Some(command) = text("command") else {
                return PreTool::Allow;
            };
            if let Some(reason) = package_manager(command, escape) {
                return PreTool::Block(reason);
            }
            git_action(command).map_or(PreTool::Allow, PreTool::CheckStaged)
        }
        Some("REPL") => text("code")
            .and_then(repl_misuse)
            .map_or(PreTool::Allow, PreTool::Block),
        _ => PreTool::Allow,
    }
}

/// What the JavaScript REPL lacks, with the fix for each.
const REPL_LACKS: &[(&str, &str)] = &[
    (
        "process",
        "read the environment with $`printenv NAME`, run commands with $`cmd`, and finish by returning a value",
    ),
    (
        "require",
        "there is no require or import; use the globals $, file, edit, write and tools.*, or run a script with $`bun x.ts`",
    ),
    (
        "BASH_REMATCH",
        "that is bash; match in JavaScript with const m = text.match(/(re)/)",
    ),
];

/// The refusal for a REPL script that uses a bash or Node idiom the REPL doesn't have.
pub fn repl_misuse(code: &str) -> Option<String> {
    let names: Vec<&'static str> = REPL_LACKS.iter().map(|(name, _)| *name).collect();
    let found = js::first_use(code, &names)?;
    let fix = REPL_LACKS.iter().find(|(name, _)| *name == found.name)?.1;
    Some(format!(
        "REPL line {}: {} does not exist here; {fix}.",
        found.line, found.name
    ))
}

/// The most characters of a tool result the post-tool hook lets pass without a word.
pub const RESULT_LIMIT: usize = 12_000;

/// The post-tool rule over Claude Code's hook input: one line for a result over
/// [`RESULT_LIMIT`] characters, which tells the agent to read less next time.
pub fn post_tool(input: &Value) -> Option<String> {
    let chars = text_chars(input.get("tool_response")?);
    (chars > RESULT_LIMIT).then(|| {
        format!(
            "That result was {chars} characters: next time locate with rg -n and read only the line range you need."
        )
    })
}

/// The characters of every string in a tool's response, whatever its shape.
fn text_chars(response: &Value) -> usize {
    match response {
        Value::String(text) => text.chars().count(),
        Value::Array(items) => items.iter().map(text_chars).sum(),
        Value::Object(fields) => fields.values().map(text_chars).sum(),
        _ => 0,
    }
}

/// How long a subagent's final report may be. A field left out sets no limit on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Template {
    pub max_lines: Option<usize>,
    pub max_chars: Option<usize>,
}

/// The report templates: one for every agent, and one for an agent type, which takes over
/// each field it sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Templates {
    pub default: Template,
    pub agents: BTreeMap<String, Template>,
}

impl Templates {
    fn for_agent(&self, agent: &str) -> Template {
        let own = self.agents.get(agent).copied().unwrap_or_default();
        Template {
            max_lines: own.max_lines.or(self.default.max_lines),
            max_chars: own.max_chars.or(self.default.max_chars),
        }
    }
}

/// The subagent-stop rule over Claude Code's hook input: the line that sends a report longer
/// than its template back to the subagent. An agent that was already sent back once is let go,
/// so a template it can't meet never loops.
pub fn subagent_stop(input: &Value, templates: &Templates) -> Option<String> {
    if input.get("stop_hook_active").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let report = input.get("last_assistant_message")?.as_str()?.trim();
    let agent = input
        .get("agent_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let template = templates.for_agent(agent);
    let lines = report.lines().count();
    let chars = report.chars().count();
    let (what, was, limit) = match (template.max_lines, template.max_chars) {
        (Some(limit), _) if lines > limit => ("lines", lines, limit),
        (_, Some(limit)) if chars > limit => ("characters", chars, limit),
        _ => return None,
    };
    Some(format!(
        "Your report is {was} {what}; its template allows {limit}. Send it again within the template, with the essentials only."
    ))
}

/// The commit or push a command runs, if any. A commit wins over a push.
pub fn git_action(command: &str) -> Option<GitAction> {
    let mut staged_before = false;
    let mut commit = None;
    let mut push = None;
    for words in all_commands(command) {
        let Some(call) = shell::invocation(&words) else {
            continue;
        };
        if call.program != "git" {
            continue;
        }
        let (dir, rest) = git_subcommand(call.args);
        let Some((sub, args)) = rest.split_first() else {
            continue;
        };
        match sub.as_str() {
            "add" | "stage" => staged_before = true,
            "commit" if commit.is_none() => {
                let all = args.iter().any(|a| a == "--all" || is_short_flag(a, 'a'));
                commit = Some(GitAction {
                    kind: GitKind::Commit,
                    all: all || staged_before,
                    dir,
                });
            }
            "push" if push.is_none() => {
                push = Some(GitAction {
                    kind: GitKind::Push,
                    all: staged_before,
                    dir,
                });
            }
            _ => {}
        }
    }
    commit.or(push)
}

fn is_short_flag(word: &str, flag: char) -> bool {
    word.starts_with('-') && !word.starts_with("--") && word.contains(flag)
}

/// Splits git's own options (`-C dir`, `-c k=v`, `--no-pager`) from the subcommand and its
/// arguments, and returns the `-C` directory.
fn git_subcommand(args: &[String]) -> (Option<String>, &[String]) {
    let mut dir = None;
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        match arg.as_str() {
            "-C" => dir = args.get(at + 1).cloned(),
            "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => {}
            _ if arg.starts_with('-') => {
                at += 1;
                continue;
            }
            _ => break,
        }
        at += 2;
    }
    (dir, args.get(at..).unwrap_or_default())
}

/// Every simple command of a command line, those inside `bash -c '…'` included.
fn all_commands(command: &str) -> Vec<Vec<String>> {
    let mut all = Vec::new();
    for words in shell::commands(command) {
        let script = shell::invocation(&words)
            .and_then(|call| shell::shell_script(&call).map(str::to_owned));
        match script {
            Some(script) => all.extend(all_commands(&script)),
            None => all.push(words),
        }
    }
    all
}

/// The refusal for a staged diff that adds one of the private `addresses`, naming the file and not
/// the address, since the line lands in the agent's transcript. Addresses match without case;
/// an empty one matches nothing.
pub fn private_address(diff: &str, addresses: &[String]) -> Option<String> {
    let needles: Vec<String> = addresses
        .iter()
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    if needles.is_empty() {
        return None;
    }
    let mut file = "";
    let mut in_header = false;
    for line in diff.lines() {
        if let Some(names) = line.strip_prefix("diff --git ") {
            in_header = true;
            file = names.rsplit_once(" b/").map_or(names, |(_, new)| new);
        } else if line.starts_with("@@") {
            in_header = false;
        } else if !in_header && line.starts_with('+') {
            let added = line.to_lowercase();
            if needles.iter().any(|needle| added.contains(needle.as_str())) {
                return Some(format!(
                    "The staged diff adds a private address in {file}: remove it, or unstage that file, then commit again."
                ));
            }
        }
    }
    None
}

/// The JavaScript package managers bun replaces.
const PACKAGE_MANAGERS: &[&str] = &["npm", "npx", "yarn", "pnpm"];

/// The refusal for a command that runs npm, npx, yarn or pnpm without the escape.
pub fn package_manager(command: &str, escape: bool) -> Option<String> {
    if escape {
        return None;
    }
    let found = all_commands(command)
        .iter()
        .find_map(|words| manager_in(words))?;
    let fix = if found == "npx" {
        "bunx".to_owned()
    } else {
        "bun install, bun run or bunx".to_owned()
    };
    Some(format!(
        "{found} is not used here: run it with {fix}, or put {ESCAPE}=1 in front of the command to use {found} this once."
    ))
}

fn manager_in(words: &[String]) -> Option<&'static str> {
    let call = shell::invocation(words)?;
    let found = PACKAGE_MANAGERS.iter().find(|m| **m == call.program)?;
    let escaped = call.assignments.iter().any(|a| {
        a.strip_prefix(ESCAPE)
            .and_then(|rest| rest.strip_prefix('='))
            .is_some_and(|value| !value.is_empty() && value != "0")
    });
    (!escaped).then_some(found)
}
