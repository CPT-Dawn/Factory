//! Reading a shell command line closely enough to know which commands it runs.
//!
//! This is no shell: it splits on the operators that start a new command, honours quotes and
//! backslashes, and leaves expansions alone. That is enough for a hook that must tell
//! `npm install` from `git commit -m "no npm here"`.

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
    /// `&`: the command before it runs in a child, in the background.
    Background,
}

/// The simple commands of `text`, in order, with where the children they run in start and end.
/// A command substitution and a subshell hold commands of their own, and a here-document's body
/// is text, not commands.
pub fn parts(text: &str) -> Vec<Part> {
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
                out.end_command();
                for (delimiter, strip_tabs) in heredocs.drain(..) {
                    at = skip_heredoc(&chars, at, &delimiter, strip_tabs);
                }
            }
            ';' => out.end_command(),
            // `&&` and `||` only join commands.
            '&' | '|' if chars.get(at) == Some(&c) => {
                at += 1;
                out.end_command();
            }
            // A redirection such as `2>&1` or `&>file`.
            '&' if at >= 2 && matches!(chars[at - 2], '>' | '<') => out.push(c),
            '&' if chars.get(at) == Some(&'>') => out.push(c),
            '&' => out.mark(Part::Background),
            '|' => {
                // `|&` pipes stderr too.
                at += usize::from(chars.get(at) == Some(&'&'));
                out.mark(Part::Pipe);
            }
            '(' => out.mark(Part::Enter),
            ')' => out.mark(Part::Leave),
            '`' => {
                in_backticks = !in_backticks;
                out.mark(if in_backticks {
                    Part::Enter
                } else {
                    Part::Leave
                });
            }
            '$' if chars.get(at) == Some(&'(') => {
                at += 1;
                out.mark(Part::Enter);
            }
            c => out.push(c),
        }
    }
    out.end_command();
    out.parts
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
}

impl Lexer {
    fn push(&mut self, c: char) {
        self.in_word = true;
        self.word.push(c);
    }

    fn open_word(&mut self) {
        self.in_word = true;
    }

    fn word_is_empty(&self) -> bool {
        !self.in_word
    }

    fn end_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    fn end_command(&mut self) {
        self.end_word();
        if !self.words.is_empty() {
            self.parts
                .push(Part::Command(std::mem::take(&mut self.words)));
        }
    }

    /// Ends the command, then records `part` after it.
    fn mark(&mut self, part: Part) {
        self.end_command();
        self.parts.push(part);
    }
}

/// Words that run what follows them, so the command is the next word that isn't an option or a
/// variable assignment.
const WRAPPERS: &[&str] = &[
    "sudo", "env", "command", "exec", "time", "nohup", "nice", "builtin", "doas", "then", "do",
    "else", "elif", "if", "while", "until", "!", "{",
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
}

/// The program a simple command runs, after its variable assignments and wrappers such as
/// `sudo` and `env`.
pub fn invocation(words: &[String]) -> Option<Invocation<'_>> {
    let mut assignments = Vec::new();
    // The wrapper whose options come next, and how many words the last option still takes.
    let mut wrapper = None;
    let mut skip = 0;
    for (at, word) in words.iter().enumerate() {
        if skip > 0 {
            skip -= 1;
        } else if is_assignment(word) {
            assignments.push(word.as_str());
        } else if let Some(name) = WRAPPERS.iter().find(|name| **name == word) {
            wrapper = Some(*name);
        } else if let (Some(name), true) = (wrapper, word.starts_with('-')) {
            // An option of the wrapper, such as `sudo -E`, with its value when it takes one.
            skip = usize::from(OPTIONS_WITH_VALUE.contains(&(name, word.as_str())));
        } else {
            return Some(Invocation {
                program: word.rsplit('/').next().unwrap_or(word),
                args: &words[at + 1..],
                assignments,
            });
        }
    }
    None
}

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
    ("doas", "-u"),
    ("doas", "-C"),
    ("env", "-u"),
    ("env", "-C"),
    ("env", "-S"),
    ("nice", "-n"),
    ("sudo", "--user"),
    ("sudo", "--group"),
    ("sudo", "--host"),
    ("sudo", "--prompt"),
    ("sudo", "--chdir"),
    ("env", "--unset"),
    ("env", "--chdir"),
    ("env", "--split-string"),
    ("nice", "--adjustment"),
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
