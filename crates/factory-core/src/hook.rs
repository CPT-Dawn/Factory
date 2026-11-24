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

use shell::Part;

/// The variable that lets one call use a JavaScript package manager: set in the environment the
/// hook runs in, or in front of the command itself.
pub const ESCAPE: &str = "FACTORY_ALLOW_NPM";

/// What the pre-tool hook decides about a tool call.
#[derive(Debug, PartialEq, Eq)]
pub enum PreTool {
    Allow,
    /// Refuse the call; the line says what to do instead.
    Block(String),
    /// The call commits or pushes: the caller checks what each adds, the staged diff of a commit
    /// with [`private_address`] and the commits a push sends with [`pushed_address`], and
    /// blocks the call if one leaks.
    Check(Vec<GitAction>),
}

/// A commit or push in a command, and what to check for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitAction {
    /// The directory it runs in, relative to the call's working directory (`cd`, `pushd`,
    /// `env -C` and `git -C`).
    pub dir: Option<String>,
    /// The repository and working tree git was pointed at (`--git-dir`, `--work-tree`,
    /// `GIT_DIR`, `GIT_WORK_TREE`), relative to the call's working directory too.
    pub git_dir: Option<String>,
    pub work_tree: Option<String>,
    /// It takes every tracked change along: `commit -a` or an earlier `git add -u`.
    pub tracked: bool,
    /// An earlier `git add` in the command, or the commit's own pathspecs, take more along
    /// than is staged when the hook runs: the pathspecs named, or none named for all of them.
    pub added: Option<Vec<String>>,
    /// A push, which sends commits and takes nothing staged along: what it was told to send.
    pub push: Option<Push>,
}

impl GitAction {
    /// The directory, repository or working tree it names that the line cannot say before it
    /// runs: one behind a variable or a command substitution, `cd -`, or `~user`. Which
    /// repository such a call works in is unknown.
    pub fn unresolved(&self) -> Option<&str> {
        [&self.dir, &self.git_dir, &self.work_tree]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .find(|path| unknowable(path))
    }
}

/// Whether a path from the line is only known when it runs (see [`GitAction::unresolved`]).
fn unknowable(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    path.contains(['$', '`']) || first == "-" || (first.starts_with('~') && first != "~")
}

/// What a `git push` was told to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Push {
    /// The remote it sends to, as named; none for the default.
    pub remote: Option<String>,
    /// What it sends; nothing for a push that only deletes.
    pub refs: Vec<PushRef>,
}

/// One thing a push sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushRef {
    /// The current branch, where the config sends it: a push that names no refspec.
    Current,
    /// `src[:dst]`: the commit `src` names, to the remote branch `dst`, or by default the
    /// one `src` names.
    Ref { src: String, dst: Option<String> },
    /// Every local branch: `--all`, `--branches`, `--mirror` or a pattern.
    Branches,
    /// Every tag: `--tags`, `--mirror` or a pattern under `refs/tags/`.
    Tags,
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
            if let Some(reason) = machine_wide_kill(command) {
                return PreTool::Block(reason);
            }
            match git_actions(command) {
                actions if actions.is_empty() => PreTool::Allow,
                actions => PreTool::Check(actions),
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
/// than its template back to the subagent. `sent_back` says this hook sent the agent back
/// before: when that is why it is still going (`stop_hook_active`), it is let go, so a
/// template it can't meet never loops. One kept going by another hook still meets the template.
pub fn subagent_stop(input: &Value, templates: &Templates, sent_back: bool) -> Option<String> {
    let continued = input.get("stop_hook_active").and_then(Value::as_bool) == Some(true);
    if continued && sent_back {
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

/// Where the shell that runs a command line is, as far as the line says: its directory (none
/// for the call's own) and its `pushd` stack.
#[derive(Clone, Default)]
struct Place {
    cwd: Option<String>,
    pushed: Vec<Option<String>>,
}

/// Follows the directory each command of a command line runs in.
#[derive(Default)]
struct Directories {
    here: Place,
    /// Where the current and-or list started: `&` sends the whole list to a child.
    list: Place,
    /// Where each child that is still running started, and its and-or list.
    outer: Vec<(Place, Place)>,
    /// Where the shell was before the last command, when that moved it: a `|` after it puts it
    /// in a child, so it moved nothing.
    before_move: Option<Place>,
    /// The command after a `|` runs in a child too.
    piped: bool,
}

impl Directories {
    /// Takes in a piece of the line that is no command.
    fn part(&mut self, part: &Part) {
        match part {
            Part::Command(_) => return,
            Part::Enter => {
                self.outer.push((self.here.clone(), self.list.clone()));
                self.list = self.here.clone();
            }
            Part::Leave => {
                if let Some((here, list)) = self.outer.pop() {
                    (self.here, self.list) = (here, list);
                }
            }
            Part::Pipe => {
                if let Some(before) = self.before_move.take() {
                    self.here = before;
                }
                self.piped = true;
                return;
            }
            Part::Background => {
                self.here = self.list.clone();
            }
            Part::Sequence => self.list = self.here.clone(),
        }
        (self.before_move, self.piped) = (None, false);
    }

    /// Takes in a command, none for one that runs no program: true when it was `cd`, `pushd`
    /// or `popd`.
    fn command(&mut self, call: Option<&shell::Invocation>) -> bool {
        let in_child = std::mem::take(&mut self.piped);
        self.before_move = None;
        let Some(call) = call else {
            return false;
        };
        let before = self.here.clone();
        let target = call
            .args
            .iter()
            .find(|arg| !arg.starts_with(['-', '+']))
            .map(|target| join(before.cwd.as_deref(), target));
        let here = &mut self.here;
        match call.program {
            // A bare `cd` goes home; `cd -` somewhere the line doesn't say.
            "cd" if call.args.is_empty() => here.cwd = Some("~".to_owned()),
            "cd" if call.args.iter().any(|arg| arg == "-") => here.cwd = Some("-".to_owned()),
            "cd" => here.cwd = target,
            "pushd" => match target {
                Some(target) => here.pushed.push(here.cwd.replace(target)),
                // A bare `pushd` swaps the top two.
                None => {
                    if let Some(top) = here.pushed.pop() {
                        here.pushed.push(std::mem::replace(&mut here.cwd, top));
                    }
                }
            },
            "popd" => {
                if let Some(top) = here.pushed.pop() {
                    here.cwd = top;
                }
            }
            _ => return false,
        }
        if in_child {
            self.here = before;
        } else {
            self.before_move = Some(before);
        }
        true
    }
}

/// The commits and pushes a command runs, each once.
pub fn git_actions(command: &str) -> Vec<GitAction> {
    /// What the command has staged so far in one repository.
    #[derive(Default)]
    struct Staged {
        tracked: bool,
        added: Option<Vec<String>>,
    }
    let mut dirs = Directories::default();
    let mut staged: BTreeMap<[Option<String>; 3], Staged> = BTreeMap::new();
    let mut actions: Vec<GitAction> = Vec::new();
    for part in all_parts(command) {
        let Part::Command(words) = &part else {
            dirs.part(&part);
            continue;
        };
        let call = shell::invocation(words);
        if dirs.command(call.as_ref()) {
            continue;
        }
        let Some(call) = call.filter(|call| call.program == "git") else {
            continue;
        };
        let mut dir = dirs.here.cwd.clone();
        for next in &call.chdir {
            dir = Some(join(dir.as_deref(), next));
        }
        let git = GitGlobals::of(&call, dir);
        let Some((sub, args)) = git.rest.split_first() else {
            continue;
        };
        let here = staged
            .entry([git.dir.clone(), git.git_dir.clone(), git.work_tree.clone()])
            .or_default();
        match sub.as_str() {
            "add" | "stage" => {
                let (all, update, paths) = add_args(args);
                // Pathspecs narrow `-u` and `-A` to themselves.
                if update && !all && paths.is_empty() {
                    here.tracked = true;
                } else {
                    stage_more(&mut here.added, paths);
                }
            }
            "commit" | "push" => {
                let mut added = here.added.clone();
                let mut tracked = here.tracked;
                let mut push = None;
                if sub == "commit" {
                    let own = commit_args(args);
                    tracked |= own.all;
                    if let Some(paths) = own.paths {
                        stage_more(&mut added, paths);
                    }
                } else {
                    (tracked, added) = (false, None);
                    push = Some(push_args(args));
                }
                let action = GitAction {
                    dir: git.dir,
                    git_dir: git.git_dir,
                    work_tree: git.work_tree,
                    tracked,
                    added,
                    push,
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

/// Adds `paths` to what is staged, where no paths are all of them, and so is an empty list.
fn stage_more(added: &mut Option<Vec<String>>, paths: Vec<String>) {
    match added {
        Some(have) if have.is_empty() => {}
        _ if paths.is_empty() => *added = Some(Vec::new()),
        _ => added.get_or_insert_with(Vec::new).extend(paths),
    }
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

/// What `git commit` was told that changes what it takes along.
struct CommitArgs {
    /// `-a`: every tracked change.
    all: bool,
    /// The working tree of these pathspecs (`--only` or `--include`); none listed for
    /// `--pathspec-from-file`, which could name any.
    paths: Option<Vec<String>>,
}

/// The short options of `git commit` that take a value, attached or as the next word.
const COMMIT_SHORT_VALUES: &[char] = &['m', 'F', 'C', 'c', 't'];
/// Its short options whose value, if any, is attached.
const COMMIT_SHORT_OPTIONAL: &[char] = &['S', 'u'];
/// Its long options that take the next word as their value when no `=` gives it.
const COMMIT_LONG_VALUES: &[&str] = &[
    "--message",
    "--file",
    "--reuse-message",
    "--reedit-message",
    "--author",
    "--date",
    "--template",
    "--fixup",
    "--squash",
    "--cleanup",
    "--trailer",
    "--pathspec-from-file",
];

fn commit_args(args: &[String]) -> CommitArgs {
    let mut all = false;
    let mut paths: Vec<String> = Vec::new();
    let mut from_file = false;
    let mut skip_next = false;
    let mut options_end = false;
    for arg in args {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if options_end || !arg.starts_with('-') || arg == "-" {
            paths.push(arg.clone());
        } else if arg == "--" {
            options_end = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            all |= name == "all";
            from_file |= name == "pathspec-from-file";
            skip_next = value.is_none() && COMMIT_LONG_VALUES.contains(&arg.as_str());
        } else {
            for (at, flag) in arg.char_indices().skip(1) {
                all |= flag == 'a';
                if COMMIT_SHORT_VALUES.contains(&flag) {
                    // The rest of the word is the value, or else the next word is.
                    skip_next = at + flag.len_utf8() == arg.len();
                    break;
                }
                if COMMIT_SHORT_OPTIONAL.contains(&flag) {
                    break;
                }
            }
        }
    }
    let paths = if from_file {
        Some(Vec::new())
    } else {
        (!paths.is_empty()).then_some(paths)
    };
    CommitArgs { all, paths }
}

/// The long options of `git push` that take the next word as their value when no `=` gives it.
const PUSH_LONG_VALUES: &[&str] = &["--repo", "--push-option", "--receive-pack", "--exec"];

/// What `git push` was told to send: its remote, then its refspecs, and the options that send
/// more or delete.
fn push_args(args: &[String]) -> Push {
    let mut remote = None;
    let mut specs: Vec<&str> = Vec::new();
    let (mut branches, mut tags, mut delete) = (false, false, false);
    let mut skip_next = false;
    let mut options_end = false;
    for (at, arg) in args.iter().enumerate() {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if options_end || !arg.starts_with('-') {
            if remote.is_none() {
                remote = Some(arg.clone());
            } else {
                specs.push(arg);
            }
        } else if arg == "--" {
            options_end = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            match name {
                "all" | "branches" => branches = true,
                "tags" => tags = true,
                "mirror" => (branches, tags) = (true, true),
                "delete" => delete = true,
                "repo" => {
                    let given = value
                        .map(str::to_owned)
                        .or_else(|| args.get(at + 1).cloned());
                    remote = remote.or(given);
                }
                _ => {}
            }
            skip_next = value.is_none() && PUSH_LONG_VALUES.contains(&arg.as_str());
        } else {
            for (at, flag) in arg.char_indices().skip(1) {
                delete |= flag == 'd';
                if flag == 'o' {
                    // The rest of the word is the value, or else the next word is.
                    skip_next = at + flag.len_utf8() == arg.len();
                    break;
                }
            }
        }
    }
    let mut refs = Vec::new();
    let mut more = |sent: PushRef| {
        if !refs.contains(&sent) {
            refs.push(sent);
        }
    };
    if !delete {
        for spec in &specs {
            let spec = spec.strip_prefix('+').unwrap_or(spec);
            let (src, dst) = spec
                .split_once(':')
                .map_or((spec, None), |(s, d)| (s, Some(d)));
            if src.is_empty() {
                // `:dst` deletes dst.
            } else if unknowable(src) {
                // What it names is only known when the push runs: it could be any branch.
                more(PushRef::Branches);
            } else if src.contains('*') {
                more(if src.starts_with("refs/tags/") {
                    PushRef::Tags
                } else {
                    PushRef::Branches
                });
            } else {
                more(PushRef::Ref {
                    src: src.to_owned(),
                    dst: dst.map(str::to_owned),
                });
            }
        }
        if branches {
            more(PushRef::Branches);
        }
        if tags {
            more(PushRef::Tags);
        }
        if specs.is_empty() && !branches && !tags {
            more(PushRef::Current);
        }
    }
    Push { remote, refs }
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

/// Where a git invocation works, from git's own options (`-C dir`, `--git-dir`, `-c k=v`,
/// `--no-pager`) and its variables, and the subcommand with its arguments after them.
struct GitGlobals<'a> {
    dir: Option<String>,
    git_dir: Option<String>,
    work_tree: Option<String>,
    rest: &'a [String],
}

impl<'a> GitGlobals<'a> {
    /// `call` runs git; `dir` is where the shell runs it.
    fn of(call: &shell::Invocation<'a>, mut dir: Option<String>) -> GitGlobals<'a> {
        let args = call.args;
        let (mut git_dir, mut work_tree) = (None, None);
        for assignment in &call.assignments {
            match assignment.split_once('=') {
                Some(("GIT_DIR", value)) => git_dir = Some(value.to_owned()),
                Some(("GIT_WORK_TREE", value)) => work_tree = Some(value.to_owned()),
                _ => {}
            }
        }
        let mut at = 0;
        while let Some(arg) = args.get(at) {
            let value = args.get(at + 1).cloned();
            match arg.split_once('=') {
                _ if arg == "-C" => {
                    if let Some(next) = &value {
                        dir = Some(join(dir.as_deref(), next));
                    }
                }
                _ if arg == "--git-dir" => git_dir = value,
                _ if arg == "--work-tree" => work_tree = value,
                _ if matches!(arg.as_str(), "-c" | "--namespace" | "--config-env") => {}
                Some(("--git-dir", given)) => {
                    git_dir = Some(given.to_owned());
                    at += 1;
                    continue;
                }
                Some(("--work-tree", given)) => {
                    work_tree = Some(given.to_owned());
                    at += 1;
                    continue;
                }
                _ if arg.starts_with('-') => {
                    at += 1;
                    continue;
                }
                _ => break,
            }
            at += 2;
        }
        // git changes to `-C` first, so the others are seen from there.
        let from_dir = |path: Option<String>| path.map(|p| join(dir.as_deref(), &p));
        GitGlobals {
            git_dir: from_dir(git_dir),
            work_tree: from_dir(work_tree),
            dir,
            rest: args.get(at..).unwrap_or_default(),
        }
    }
}

/// Every piece of a command line, those inside `bash -c '…'` included, as a child of its own.
fn all_parts(command: &str) -> Vec<Part> {
    let mut all = Vec::new();
    for part in shell::parts(command) {
        let script = match &part {
            Part::Command(words) => shell::invocation(words)
                .and_then(|call| shell::shell_script(&call).map(str::to_owned)),
            _ => None,
        };
        match script {
            Some(script) => {
                all.push(Part::Enter);
                all.extend(all_parts(&script));
                all.push(Part::Leave);
            }
            None => all.push(part),
        }
    }
    all
}

/// The refusal for a staged diff that adds one of the private `addresses`, naming the file and not
/// the address, since the line lands in the agent's transcript. Addresses match without case;
/// an empty one matches nothing.
pub fn private_address(diff: &str, addresses: &[String]) -> Option<String> {
    let named = leaking_file(diff, &needles(addresses), "git diff --cached --name-only")?;
    Some(format!(
        "The staged diff adds a private address in {named}: remove it, or unstage that file, then commit again."
    ))
}

/// One commit a push sends: its short sha and its patch, a combined diff for a merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    pub commit: String,
    pub diff: String,
}

/// The refusal for a push whose commits, given oldest first, add one of the private
/// `addresses`: it names the first commit that does and its file, as [`private_address`] does.
pub fn pushed_address(sent: &[Patch], addresses: &[String]) -> Option<String> {
    let needles = needles(addresses);
    sent.iter().find_map(|patch| {
        let commit = &patch.commit;
        let look = format!("git show --name-only {commit}");
        let named = leaking_file(&patch.diff, &needles, &look)?;
        Some(format!(
            "Commit {commit} in this push adds a private address in {named}: rewrite that commit without it, then push again."
        ))
    })
}

/// The file in `diff` that adds one of the `needles`, as the refusal may name it: a name that
/// has the address itself gives way to the command (`look`) that lists the names.
fn leaking_file(diff: &str, needles: &[String], look: &str) -> Option<String> {
    if needles.is_empty() {
        return None;
    }
    let has = |text: &str| contains_any(text, needles);
    // The name may carry the address too; the line must not.
    let name_of = |file: &str| {
        if has(file) {
            format!("a file whose name has it (see {look})")
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
        // A merge's combined diff names the file once.
        let header = line
            .strip_prefix("diff --git ")
            .map(|names| names.rsplit_once(" b/").map_or(names, |(_, new)| new))
            .or_else(|| line.strip_prefix("diff --cc "))
            .or_else(|| line.strip_prefix("diff --combined "));
        if let Some(name) = header {
            if named && !deleted {
                return Some(name_of(file));
            }
            in_header = true;
            file = name;
            named = has(file);
            deleted = false;
        } else if line.starts_with("@@") {
            in_header = false;
        } else if in_header && line.starts_with("deleted file mode") {
            deleted = true;
        } else if !in_header && line.starts_with('+') && has(line) {
            return Some(name_of(file));
        }
    }
    if named && !deleted {
        return Some(name_of(file));
    }
    None
}

/// Whether `text` holds one of the private `addresses`, matched as [`private_address`] matches
/// them: a line that does must not reach the agent's transcript.
pub fn mentions_private(text: &str, addresses: &[String]) -> bool {
    contains_any(text, &needles(addresses))
}

/// The addresses as they are looked for: trimmed, lower case, none empty.
fn needles(addresses: &[String]) -> Vec<String> {
    addresses
        .iter()
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect()
}

fn contains_any(text: &str, needles: &[String]) -> bool {
    let lowered = text.to_lowercase();
    needles
        .iter()
        .any(|needle| lowered.contains(needle.as_str()))
}

/// The JavaScript package managers bun replaces.
const PACKAGE_MANAGERS: &[&str] = &["npm", "npx", "yarn", "pnpm"];

/// The refusal for a command that runs npm, npx, yarn or pnpm without the escape.
pub fn package_manager(command: &str, escape: bool) -> Option<String> {
    if escape {
        return None;
    }
    let found = all_parts(command).iter().find_map(|part| match part {
        Part::Command(words) => manager_in(words),
        _ => None,
    })?;
    let fix = if found == "npx" {
        "bunx".to_owned()
    } else {
        "bun install, bun run or bunx".to_owned()
    };
    Some(format!(
        "{found} is not used here: run it with {fix}, or put {ESCAPE}=1 in front of the command to use {found} this once."
    ))
}

/// The refusal for a command that kills processes by pattern: `pkill -f` and `killall` match
/// every agent's processes on the machine, and the pattern can match the caller's own shell. No
/// escape: the damage lands on other agents, not on the one that asks.
pub fn machine_wide_kill(command: &str) -> Option<String> {
    all_parts(command).iter().any(|part| match part {
        Part::Command(words) => shell::invocation(words).is_some_and(|call| match call.program {
            "killall" => true,
            "pkill" => matches_full_command_line(call.args),
            _ => false,
        }),
        _ => false,
    })
    .then(|| {
        "pkill -f and killall reach every agent's processes on this machine; kill the PID you started (note $! when you start it, or list candidates with pgrep -af <pattern> and kill only yours)."
            .to_owned()
    })
}

/// Whether pkill's options, up to a bare `--`, include `-f` (alone or inside a group such as
/// `-9f`) or `--full` (or any prefix of it getopt accepts).
fn matches_full_command_line(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| match arg.strip_prefix("--") {
            Some(long) => !long.is_empty() && "full".starts_with(long),
            None => is_short_flag(arg, 'f'),
        })
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
