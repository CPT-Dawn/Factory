//! The rules Claude Code's hooks apply (spec #1, "Hooks").
//!
//! Each rule is a pure function of the hook's input, so a table can drive it. The binary reads
//! stdin, asks the rule, and answers in Claude Code's hook protocol. A rule that needs the
//! outside world, such as the staged diff, hands back what it needs instead of fetching it.

mod js;
mod shell;

use std::collections::BTreeMap;
use std::path::Path;

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
    /// The call commits or pushes: the caller checks the staged diff of each with
    /// [`private_address`] and blocks the call if one leaks.
    CheckStaged(Vec<GitAction>),
}

/// A commit or push in a command, and the diff to check for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitAction {
    /// The directory it runs in, relative to the call's working directory (`cd` and `-C`).
    pub dir: Option<String>,
    /// It takes every tracked change along: `commit -a` or an earlier `git add -u`.
    pub tracked: bool,
    /// An earlier `git add` in the command stages more, which is not staged yet when the hook
    /// runs: the pathspecs it named, or none named for all of them.
    pub added: Option<Vec<String>>,
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
            match git_actions(command) {
                actions if actions.is_empty() => PreTool::Allow,
                actions => PreTool::CheckStaged(actions),
            }
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

/// The commits and pushes a command runs, each once.
pub fn git_actions(command: &str) -> Vec<GitAction> {
    /// What the command has staged so far in one directory.
    #[derive(Default)]
    struct Staged {
        tracked: bool,
        added: Option<Vec<String>>,
    }
    let mut cwd: Option<String> = None;
    let mut staged: BTreeMap<Option<String>, Staged> = BTreeMap::new();
    let mut actions: Vec<GitAction> = Vec::new();
    for words in all_commands(command) {
        let Some(call) = shell::invocation(&words) else {
            continue;
        };
        if call.program == "cd" {
            // A bare `cd` and `cd -` go somewhere the command line doesn't say.
            cwd = call
                .args
                .iter()
                .find(|arg| !arg.starts_with('-'))
                .map(|target| join(cwd.as_deref(), target));
            continue;
        }
        if call.program != "git" {
            continue;
        }
        let (dash_c, rest) = git_subcommand(call.args);
        let dir = match (dash_c, &cwd) {
            (Some(dash_c), base) => Some(join(base.as_deref(), &dash_c)),
            (None, base) => base.clone(),
        };
        let Some((sub, args)) = rest.split_first() else {
            continue;
        };
        let here = staged.entry(dir.clone()).or_default();
        match sub.as_str() {
            "add" | "stage" => {
                let (all, update, paths) = add_args(args);
                here.tracked |= update;
                if all || (!update && paths.is_empty()) {
                    here.added = Some(Vec::new());
                } else if !paths.is_empty() && here.added.as_ref().is_none_or(|a| !a.is_empty()) {
                    // (An empty list already means every path.)
                    here.added.get_or_insert_with(Vec::new).extend(paths);
                }
            }
            "commit" | "push" => {
                let own =
                    sub == "commit" && args.iter().any(|a| a == "--all" || is_short_flag(a, 'a'));
                let action = GitAction {
                    dir,
                    tracked: own || here.tracked,
                    added: here.added.clone(),
                };
                if !actions.contains(&action) {
                    actions.push(action);
                }
            }
            _ => {}
        }
    }
    actions
}

/// What `git add` was told: whether `-A`, whether `-u`, and the pathspecs.
fn add_args(args: &[String]) -> (bool, bool, Vec<String>) {
    let (mut all, mut update) = (false, false);
    let mut paths = Vec::new();
    let mut options_end = false;
    for arg in args {
        if options_end || !arg.starts_with('-') {
            paths.push(arg.clone());
        } else if arg == "--" {
            options_end = true;
        } else if arg == "--all" || is_short_flag(arg, 'A') {
            all = true;
        } else if arg == "--update" || is_short_flag(arg, 'u') {
            update = true;
        }
    }
    (all, update, paths)
}

/// `next` as seen from `base`: an absolute path stands alone.
fn join(base: Option<&str>, next: &str) -> String {
    match base {
        Some(base) => Path::new(base).join(next).to_string_lossy().into_owned(),
        None => next.to_owned(),
    }
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
    let has = |text: &str| {
        let lowered = text.to_lowercase();
        needles
            .iter()
            .any(|needle| lowered.contains(needle.as_str()))
    };
    let refusal = |named: &str| {
        Some(format!(
            "The staged diff adds a private address in {named}: remove it, or unstage that file, then commit again."
        ))
    };
    // The name may carry the address too; the line must not.
    let name_of = |file: &str| {
        if has(file) {
            "a file whose name has it (see git diff --cached --name-only)".to_owned()
        } else {
            file.to_owned()
        }
    };
    let mut file = "";
    let mut in_header = false;
    // A file whose new path carries the address leaks it even when its content is clean,
    // unless the diff deletes it.
    let mut named = false;
    let mut deleted = false;
    for line in diff.lines() {
        if let Some(names) = line.strip_prefix("diff --git ") {
            if named && !deleted {
                return refusal(&name_of(file));
            }
            in_header = true;
            file = names.rsplit_once(" b/").map_or(names, |(_, new)| new);
            named = has(file);
            deleted = false;
        } else if line.starts_with("@@") {
            in_header = false;
        } else if in_header && line.starts_with("deleted file mode") {
            deleted = true;
        } else if !in_header && line.starts_with('+') && has(line) {
            return refusal(&name_of(file));
        }
    }
    if named && !deleted {
        return refusal(&name_of(file));
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
