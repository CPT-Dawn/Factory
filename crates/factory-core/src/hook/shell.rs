//! Reading a shell command line closely enough to know which commands it runs.
//!
//! This is no shell: it splits on the operators that start a new command, honours quotes and
//! backslashes, and leaves expansions alone. That is enough for a hook that must tell
//! `npm install` from `git commit -m "no npm here"`.

use std::collections::BTreeMap;

/// One simple command: its words, with quotes and escapes resolved.
pub type Words = Vec<String>;

/// A piece of a command line: a simple command, or where a child shell starts or ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Command(Words),
    /// A subshell, command substitution or `bash -c` starts: a `cd` inside stays inside.
    Enter,
    /// The innermost child that started ends.
    Leave,
    /// `|`: the commands on both sides each run in a child.
    Pipe,
    /// `&`: the and-or list before it runs in a child, in the background.
    Background,
    /// `;` or a new line: the next and-or list starts.
    Sequence,
}

/// What a word holds in place of a command substitution's output, which the line does not say.
const SUBSTITUTION: &str = "$(…)";

/// A command line read into its parts, with what the quoting of each word said.
pub struct Line {
    pub parts: Vec<Part>,
    /// The marks of each word of a [`Part::Command`] that has any, by (part, word).
    marks: BTreeMap<(usize, usize), Marks>,
}

/// What quoting says about a word, which its text after quote removal no longer does.
#[derive(Clone, Copy, Default)]
struct Marks {
    /// It holds a `$` that quotes keep from the shell, such as the anchor in `rg 'foo$'`.
    quoted_dollar: bool,
    /// The shell expands part of it: a variable, a command substitution or a glob outside quotes.
    expands: bool,
}

impl Line {
    /// Whether word `word` of the command at `part` holds a `$` that quotes keep literal: any
    /// in single quotes, and one in double quotes that starts no expansion. One escaped with a
    /// backslash is not counted.
    pub fn quoted_dollar(&self, part: usize, word: usize) -> bool {
        self.marks
            .get(&(part, word))
            .is_some_and(|m| m.quoted_dollar)
    }

    /// Whether the shell expands part of that word, so it is only known when it runs: a `$name`,
    /// `${…}` or `$(…)` outside single quotes, a backquote, or a `*`, `?` or `[` outside quotes.
    pub fn expands(&self, part: usize, word: usize) -> bool {
        self.marks.get(&(part, word)).is_some_and(|m| m.expands)
    }
}

/// The simple commands of `text`, in order, with where the children they run in start and end.
/// A command substitution and a subshell hold commands of their own, and a here-document's body
/// is text, not commands.
pub fn parts(text: &str) -> Vec<Part> {
    line(text).parts
}

/// [`parts`], with which words hold a quoted `$`.
pub fn line(text: &str) -> Line {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Lexer::default();
    let mut in_backticks = false;
    // The `<<WORD` delimiters whose bodies start at the next line, with whether `<<-` strips tabs.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        at += 1;
        match c {
            '\\' => match chars.get(at) {
                // A line continuation joins the lines.
                Some('\n') => {
                    at += 1;
                    out.end_word();
                }
                Some(&escaped) => {
                    at += 1;
                    out.push(escaped);
                }
                None => out.end_word(),
            },
            '\'' => {
                out.open_word();
                while at < chars.len() && chars[at] != '\'' {
                    if chars[at] == '$' && !out.word.ends_with('\\') {
                        out.word_marks.quoted_dollar = true;
                    }
                    out.word.push(chars[at]);
                    at += 1;
                }
                at += 1;
            }
            '"' => {
                out.open_word();
                while at < chars.len() && chars[at] != '"' {
                    let c = chars[at];
                    at += 1;
                    if c != '\\' {
                        match c {
                            '$' if starts_expansion(chars.get(at)) => {
                                out.word_marks.expands = true;
                            }
                            '$' if !out.word.ends_with('\\') => {
                                out.word_marks.quoted_dollar = true;
                            }
                            '`' => out.word_marks.expands = true,
                            _ => {}
                        }
                        out.word.push(c);
                        continue;
                    }
                    match chars.get(at) {
                        Some(&escaped @ ('"' | '\\' | '$' | '`')) => out.word.push(escaped),
                        Some('\n') => {}
                        Some(&other) => {
                            out.word.push('\\');
                            out.word.push(other);
                        }
                        None => break,
                    }
                    at += 1;
                }
                at += 1;
            }
            ' ' | '\t' | '\r' => out.end_word(),
            '#' if out.word_is_empty() => {
                while at < chars.len() && chars[at] != '\n' {
                    at += 1;
                }
            }
            '<' if chars.get(at) == Some(&'<') && chars.get(at + 1) != Some(&'<') => {
                at += 1;
                let strip_tabs = chars.get(at) == Some(&'-');
                at += usize::from(strip_tabs);
                while matches!(chars.get(at), Some(' ' | '\t')) {
                    at += 1;
                }
                let mut delimiter = String::new();
                while let Some(&c) = chars.get(at) {
                    if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>') {
                        break;
                    }
                    at += 1;
                    if !matches!(c, '\'' | '"' | '\\') {
                        delimiter.push(c);
                    }
                }
                heredocs.push((delimiter, strip_tabs));
            }
            '\n' => {
                // After `&&`, `||` or `|`, a new line carries on the list.
                if out.joined {
                    out.end_command();
                } else {
                    out.mark(Part::Sequence);
                }
                for (delimiter, strip_tabs) in heredocs.drain(..) {
                    at = skip_heredoc(&chars, at, &delimiter, strip_tabs);
                }
            }
            ';' => out.mark(Part::Sequence),
            // `&&` and `||` only join commands.
            '&' | '|' if chars.get(at) == Some(&c) => {
                at += 1;
                out.end_command();
                out.joined = true;
            }
            // A redirection such as `2>&1` or `&>file`.
            '&' if at >= 2 && matches!(chars[at - 2], '>' | '<') => out.push(c),
            '&' if chars.get(at) == Some(&'>') => out.push(c),
            '&' => out.mark(Part::Background),
            '|' => {
                // `|&` pipes stderr too.
                at += usize::from(chars.get(at) == Some(&'&'));
                out.mark(Part::Pipe);
                out.joined = true;
            }
            '(' => {
                out.open.push(None);
                out.mark(Part::Enter);
            }
            ')' => out.close(),
            '`' => {
                in_backticks = !in_backticks;
                if in_backticks {
                    out.substitute();
                } else {
                    out.close();
                }
            }
            '$' if chars.get(at) == Some(&'(') => {
                at += 1;
                out.substitute();
            }
            c => {
                let expands = match c {
                    '*' | '?' | '[' => true,
                    '$' => starts_expansion(chars.get(at)),
                    _ => false,
                };
                out.push(c);
                out.word_marks.expands |= expands;
            }
        }
    }
    out.end_command();
    Line {
        parts: out.parts,
        marks: out.marks,
    }
}

/// Whether a `$` before `next` starts an expansion: a name, `${`, `$(`, a
/// positional or a special parameter.
fn starts_expansion(next: Option<&char>) -> bool {
    next.is_some_and(|c| c.is_alphanumeric() || "_{(?!#@*-$".contains(*c))
}

/// Where a here-document's body that starts at `from` ends: after its delimiter line.
fn skip_heredoc(chars: &[char], from: usize, delimiter: &str, strip_tabs: bool) -> usize {
    let mut at = from;
    while at < chars.len() {
        let line_end = chars[at..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |n| at + n);
        let line: String = chars[at..line_end].iter().collect();
        let line = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            &line
        };
        at = (line_end + 1).min(chars.len());
        if line == delimiter {
            break;
        }
    }
    at
}

#[derive(Default)]
struct Lexer {
    parts: Vec<Part>,
    words: Words,
    word: String,
    /// A quoted empty string is still a word.
    in_word: bool,
    /// The last thing read was `&&`, `||` or `|`, which the next command continues.
    joined: bool,
    /// What each open `(`, `$(` or backquote interrupted: for a command substitution, the
    /// command it is part of, which goes on once it closes; none for a subshell.
    open: Vec<Option<Interrupted>>,
    /// The marks of the word being read, of each word of the command so far, and of the line.
    word_marks: Marks,
    words_marks: Vec<Marks>,
    marks: BTreeMap<(usize, usize), Marks>,
}

/// The command a command substitution interrupted, with the word it is part of.
struct Interrupted {
    words: Words,
    words_marks: Vec<Marks>,
    word: String,
    word_marks: Marks,
}

impl Lexer {
    fn push(&mut self, c: char) {
        self.open_word();
        self.word.push(c);
    }

    fn open_word(&mut self) {
        self.in_word = true;
        self.joined = false;
    }

    fn word_is_empty(&self) -> bool {
        !self.in_word
    }

    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.words_marks.push(std::mem::take(&mut self.word_marks));
            self.in_word = false;
        }
    }

    fn end_command(&mut self) {
        self.end_word();
        let part = self.parts.len();
        for (word, marks) in self.words_marks.drain(..).enumerate() {
            if marks.quoted_dollar || marks.expands {
                self.marks.insert((part, word), marks);
            }
        }
        if !self.words.is_empty() {
            self.parts
                .push(Part::Command(std::mem::take(&mut self.words)));
        }
    }

    /// Starts a command substitution: its commands run in a child, before the command it is
    /// part of, which goes on after it.
    fn substitute(&mut self) {
        let interrupted = Interrupted {
            words: std::mem::take(&mut self.words),
            words_marks: std::mem::take(&mut self.words_marks),
            word: std::mem::take(&mut self.word),
            word_marks: std::mem::take(&mut self.word_marks),
        };
        self.in_word = false;
        self.open.push(Some(interrupted));
        self.mark(Part::Enter);
    }

    /// Ends the innermost subshell or command substitution. The substitution's output is part
    /// of a word, which stands for it as [`SUBSTITUTION`].
    fn close(&mut self) {
        self.mark(Part::Leave);
        if let Some(Some(was)) = self.open.pop() {
            (self.words, self.words_marks) = (was.words, was.words_marks);
            (self.word, self.word_marks) = (was.word, was.word_marks);
            self.push_str(SUBSTITUTION);
            self.word_marks.expands = true;
        }
    }

    fn push_str(&mut self, text: &str) {
        self.open_word();
        self.word.push_str(text);
    }

    /// Ends the command, then records `part` after it.
    fn mark(&mut self, part: Part) {
        self.end_command();
        self.parts.push(part);
        self.joined = false;
    }
}

/// Words that run what follows them, with how many words of their own (after their options)
/// come first, such as `timeout`'s duration; the command is the next word that isn't one of
/// those, an option or a variable assignment.
const WRAPPERS: &[(&str, usize)] = &[
    ("sudo", 0),
    ("env", 0),
    ("command", 0),
    ("exec", 0),
    ("time", 0),
    ("nohup", 0),
    ("nice", 0),
    ("builtin", 0),
    ("doas", 0),
    ("xargs", 0),
    ("stdbuf", 0),
    ("ionice", 0),
    ("setsid", 0),
    ("timeout", 1),
    ("flock", 1),
    ("chrt", 1),
    ("taskset", 1),
    ("then", 0),
    ("do", 0),
    ("else", 0),
    ("elif", 0),
    ("if", 0),
    ("while", 0),
    ("until", 0),
    ("!", 0),
    ("{", 0),
];

/// The shells that run their `-c` argument.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash"];

/// What a simple command runs.
pub struct Invocation<'a> {
    /// The program's name without its directory.
    pub program: &'a str,
    /// The words after the program.
    pub args: &'a [String],
    /// The leading `NAME=value` words, which set variables for this command alone.
    pub assignments: Vec<&'a str>,
    /// The directories a wrapper changes to first (`env -C`, `sudo -D`), in order.
    pub chdir: Vec<&'a str>,
}

/// The program a simple command runs, after its variable assignments and wrappers such as
/// `sudo` and `env`.
pub fn invocation(words: &[String]) -> Option<Invocation<'_>> {
    let mut assignments = Vec::new();
    let mut chdir = Vec::new();
    // The wrapper whose options come next, and how many of its own words are still to come.
    let mut wrapper: Option<(&str, usize)> = None;
    // The wrapper option whose value is the next word.
    let mut value_of: Option<(&str, &str)> = None;
    for (at, word) in words.iter().enumerate() {
        if let Some(option) = value_of.take() {
            if CHDIR.contains(&option) {
                chdir.push(word.as_str());
            }
        } else if is_assignment(word) {
            assignments.push(word.as_str());
        } else if let Some(&(name, own)) = WRAPPERS.iter().find(|(name, _)| *name == word) {
            wrapper = Some((name, own));
        } else if let (Some((name, _)), true) = (wrapper, word.starts_with('-')) {
            // An option of the wrapper, such as `sudo -E`, with its value when it takes one.
            match word.split_once('=') {
                Some((option, value)) if CHDIR.contains(&(name, option)) => chdir.push(value),
                Some(_) => {}
                None if OPTIONS_WITH_VALUE.contains(&(name, word.as_str())) => {
                    value_of = Some((name, word.as_str()));
                }
                None => {}
            }
        } else if let Some((name, own @ 1..)) = wrapper {
            wrapper = Some((name, own - 1));
        } else {
            return Some(Invocation {
                program: word.rsplit('/').next().unwrap_or(word),
                args: &words[at + 1..],
                assignments,
                chdir,
            });
        }
    }
    None
}

/// The wrapper options that change directory before running the command.
const CHDIR: &[(&str, &str)] = &[
    ("env", "-C"),
    ("env", "--chdir"),
    ("sudo", "-D"),
    ("sudo", "--chdir"),
];

/// Wrapper options that take the next word as their value.
const OPTIONS_WITH_VALUE: &[(&str, &str)] = &[
    ("sudo", "-u"),
    ("sudo", "-g"),
    ("sudo", "-h"),
    ("sudo", "-p"),
    ("sudo", "-C"),
    ("sudo", "-D"),
    ("sudo", "-R"),
    ("sudo", "-T"),
    ("sudo", "-U"),
    ("sudo", "--user"),
    ("sudo", "--group"),
    ("sudo", "--host"),
    ("sudo", "--prompt"),
    ("sudo", "--chdir"),
    ("doas", "-u"),
    ("doas", "-C"),
    ("env", "-u"),
    ("env", "-C"),
    ("env", "-S"),
    ("env", "--unset"),
    ("env", "--chdir"),
    ("env", "--split-string"),
    ("nice", "-n"),
    ("nice", "--adjustment"),
    ("timeout", "-s"),
    ("timeout", "-k"),
    ("timeout", "--signal"),
    ("timeout", "--kill-after"),
    ("xargs", "-I"),
    ("xargs", "-n"),
    ("xargs", "-P"),
    ("xargs", "-d"),
    ("xargs", "-E"),
    ("xargs", "-L"),
    ("xargs", "-s"),
    ("xargs", "-a"),
    ("xargs", "--max-args"),
    ("xargs", "--max-procs"),
    ("xargs", "--delimiter"),
    ("xargs", "--max-lines"),
    ("xargs", "--max-chars"),
    ("xargs", "--arg-file"),
    ("xargs", "--process-slot-var"),
    ("stdbuf", "-i"),
    ("stdbuf", "-o"),
    ("stdbuf", "-e"),
    ("stdbuf", "--input"),
    ("stdbuf", "--output"),
    ("stdbuf", "--error"),
    ("ionice", "-c"),
    ("ionice", "-n"),
    ("ionice", "-p"),
    ("ionice", "-P"),
    ("ionice", "-u"),
    ("ionice", "--class"),
    ("ionice", "--classdata"),
    ("ionice", "--pid"),
    ("ionice", "--pgid"),
    ("ionice", "--uid"),
    ("flock", "-w"),
    ("flock", "-E"),
    ("flock", "--timeout"),
    ("flock", "--wait"),
    ("flock", "--conflict-exit-code"),
];

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The script a shell invocation runs with `-c`, such as `bash -c 'npm test'`.
pub fn shell_script<'a>(invocation: &Invocation<'a>) -> Option<&'a str> {
    if !SHELLS.contains(&invocation.program) {
        return None;
    }
    let args = invocation.args;
    let flag = args
        .iter()
        .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))?;
    args.get(flag + 1).map(String::as_str)
}
