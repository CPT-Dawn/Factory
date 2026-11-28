//! Reading a JavaScript snippet closely enough to find the identifiers it uses and the shell
//! commands it runs.
//!
//! This is no parser: it reads tokens, skipping comments and telling string and regex literals
//! and the text of template literals (but not their `${…}` expressions) from code, so a word that
//! only appears in a string is not a use. A regex literal is told from a division by the token
//! before it, and a `)` by whether it closes an `if`, `while`, `for` or `with` condition.

/// An identifier the script uses, and the line it is on (from 1).
#[derive(Debug, PartialEq, Eq)]
pub struct Use {
    pub name: &'static str,
    pub line: usize,
}

/// The first of `names` the script uses as a variable: not a property (`x.name`, `{name: 1}`),
/// not inside a string, comment or regex.
pub fn first_use(code: &str, names: &[&'static str]) -> Option<Use> {
    let tokens = tokens(code);
    tokens.iter().enumerate().find_map(|(at, token)| {
        let Tok::Word(word) = &token.tok else {
            return None;
        };
        let name = names.iter().find(|name| **name == word)?;
        let prev = at.checked_sub(1).map(|before| &tokens[before].tok);
        let property = prev == Some(&Tok::Punct('.'));
        let key = matches!(prev, Some(Tok::Punct('{' | ',') | Tok::HoleOpen))
            && tokens.get(at + 1).map(|next| &next.tok) == Some(&Tok::Punct(':'));
        (!property && !key).then_some(Use {
            name,
            line: token.line,
        })
    })
}

/// A shell command the script runs through the REPL's `$`: a `$`…`` template, or `$(text)` with a
/// string or template literal.
#[derive(Debug, PartialEq, Eq)]
pub struct ShellCall {
    /// The line the `$` is on (from 1).
    pub line: usize,
    /// The shell text as the REPL hands it over, `\`` and `\${` without their backslash. Each
    /// `${…}` of a template stands as one literal word, which is what the REPL makes of it.
    pub text: String,
    /// Whether `text` is what runs: false when an expression splices in text the script doesn't
    /// say, as `${{raw: s}}` does, or `${…}` in a template passed to `$(…)`.
    pub exact: bool,
    /// It is a `$`…`` template, whose text the shell denies read; a `$(…)` string call is not.
    pub template: bool,
    /// It sits in a loop's body, a `.then`, `.catch`, `.finally`, `.forEach`, `.map` or `.flatMap`
    /// callback, or a `setTimeout` or `setInterval`: it may run more than once.
    pub repeated: bool,
    /// The timeout it is given in ms: a `.timeout(n)` chained on it, or `$(text, n)`.
    pub timeout: Option<u64>,
}

/// What a template's `${…}` turns into: one single-quoted word, as the REPL quotes the value.
const INTERPOLATED: &str = "'…'";

/// Every shell command the script runs through `$`, in order. A `$(name)` whose text is held in a
/// variable says nothing and is left out.
pub fn shell_calls(code: &str) -> Vec<ShellCall> {
    let tokens = tokens(code);
    let mut calls = Vec::new();
    let mut nesting = Nesting::default();
    for (at, token) in tokens.iter().enumerate() {
        nesting.take(&tokens, at);
        let dollar = matches!(&token.tok, Tok::Word(word) if word == "$");
        let property = at
            .checked_sub(1)
            .is_some_and(|before| tokens[before].tok == Tok::Punct('.'));
        if !dollar || property {
            continue;
        }
        let is_template = tokens.get(at + 1).map(|next| &next.tok) == Some(&Tok::TemplateOpen);
        let shell = match tokens.get(at + 1).map(|next| &next.tok) {
            Some(Tok::TemplateOpen) => {
                template(&tokens, at + 1, true).map(|(text, exact, end)| (text, exact, None, end))
            }
            Some(Tok::Punct('(')) => called(&tokens, at + 1),
            _ => None,
        };
        let Some((text, exact, given, end)) = shell else {
            continue;
        };
        calls.push(ShellCall {
            line: token.line,
            text,
            exact,
            template: is_template,
            repeated: nesting.repeats(),
            timeout: chained_timeout(&tokens, end + 1).or(given),
        });
    }
    calls
}

/// The text of the template literal that opens at `open`, whether it is exact, and where it closes.
/// `quoted`: each `${…}` is a quoted word (a `$`…`` template), not spliced-in text.
fn template(tokens: &[Token], open: usize, quoted: bool) -> Option<(String, bool, usize)> {
    let mut text = String::new();
    let mut exact = true;
    let mut holes = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open + 1) {
        match &token.tok {
            Tok::Text(raw) if holes == 0 => {
                text.push_str(&raw.replace("\\`", "`").replace("\\${", "${"));
            }
            Tok::HoleOpen => {
                if holes == 0 {
                    let tok = |i: usize| tokens.get(i).map(|t| &t.tok);
                    let raw = matches!(
                        (tok(at + 1), tok(at + 2), tok(at + 3)),
                        (Some(Tok::Punct('{')), Some(Tok::Word(word)), Some(Tok::Punct(':')))
                            if word == "raw"
                    );
                    exact &= quoted && !raw;
                    text.push_str(INTERPOLATED);
                }
                holes += 1;
            }
            Tok::HoleClose => holes = holes.saturating_sub(1),
            Tok::TemplateClose if holes == 0 => return Some((text, exact, at)),
            _ => {}
        }
    }
    None
}

/// `$(text)` or `$(text, ms)` whose `(` is at `open`, with a literal for its text: the text, whether
/// it is exact, the timeout given, and where the call closes.
fn called(tokens: &[Token], open: usize) -> Option<(String, bool, Option<u64>, usize)> {
    let (text, exact, after) = match &tokens.get(open + 1)?.tok {
        Tok::Str(text) => (text.clone(), true, open + 2),
        Tok::TemplateOpen => {
            let (text, exact, end) = template(tokens, open + 1, false)?;
            (text, exact, end + 1)
        }
        _ => return None,
    };
    let tok = |i: usize| tokens.get(i).map(|t| &t.tok);
    match (tok(after), tok(after + 1), tok(after + 2)) {
        (Some(Tok::Punct(')')), _, _) => Some((text, exact, None, after)),
        (Some(Tok::Punct(',')), Some(Tok::Num(ms)), Some(Tok::Punct(')'))) => {
            Some((text, exact, number(ms), after + 2))
        }
        _ => Some((text, exact, None, closing(tokens, open)?)),
    }
}

/// The `.timeout(n)` in the chain of calls that starts at `from`, such as `.nothrow().timeout(n)`.
fn chained_timeout(tokens: &[Token], from: usize) -> Option<u64> {
    let tok = |i: usize| tokens.get(i).map(|t| &t.tok);
    let mut at = from;
    let mut timeout = None;
    while let (Some(Tok::Punct('.')), Some(Tok::Word(name)), Some(Tok::Punct('('))) =
        (tok(at), tok(at + 1), tok(at + 2))
    {
        let Some(close) = closing(tokens, at + 2) else {
            break;
        };
        if let (true, Some(Tok::Num(ms))) = (name == "timeout" && close == at + 4, tok(at + 3)) {
            timeout = number(ms);
        }
        at = close + 1;
    }
    timeout
}

/// Where the bracket that opens at `open` closes.
fn closing(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token.tok {
            Tok::Punct('(' | '[' | '{') | Tok::HoleOpen => depth += 1,
            Tok::Punct(')' | ']' | '}') | Tok::HoleClose => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// A numeric literal's value: `590_000`, `0x493e0`, `0o7`, `0b11`, `6e5` or `3.0e5`.
fn number(literal: &str) -> Option<u64> {
    let digits = literal.replace('_', "").to_ascii_lowercase();
    let radix = [("0x", 16), ("0o", 8), ("0b", 2)]
        .into_iter()
        .find_map(|(prefix, radix)| Some((digits.strip_prefix(prefix)?, radix)));
    match radix {
        Some((rest, radix)) => u64::from_str_radix(rest.trim_end_matches('n'), radix).ok(),
        None => digits.trim_end_matches('n').parse().ok().or_else(|| {
            let value = digits.parse::<f64>().ok()?;
            (value.is_finite() && value >= 0.0).then_some(value as u64)
        }),
    }
}

/// The callbacks that may run more than once, or after the statement that hands them over.
const CALLBACKS: &[&str] = &["then", "catch", "finally", "forEach", "map", "flatMap"];
const TIMERS: &[&str] = &["setTimeout", "setInterval"];

/// The brackets open at a token, and which of them hold what may run more than once.
#[derive(Default)]
struct Nesting {
    open: Vec<Bracket>,
    /// A loop's head just closed, or a `do` was read: its body comes next.
    body_next: bool,
    /// A `do` body just closed: the `while` after it ends the loop and starts no other.
    after_do: bool,
    /// The loop bodies without braces being read: the depth each started at.
    bare_bodies: Vec<usize>,
}

/// An open bracket: whether it is a loop's body, and what else it is.
#[derive(Clone, Copy)]
struct Bracket {
    loop_body: bool,
    holds: Holds,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Holds {
    LoopHead,
    DoBody,
    /// The arguments of a `.then` or a timer. Only a function among them runs later, maybe
    /// more than once: `armed` from its `=>` or `function` to the next argument.
    Callback {
        armed: bool,
    },
    Other,
}

/// The words after which a statement goes on past the end of its line.
const GOES_ON: &[&str] = &[
    "await",
    "new",
    "typeof",
    "void",
    "delete",
    "yield",
    "in",
    "of",
    "instanceof",
    "else",
    "do",
    "case",
];

impl Nesting {
    fn repeats(&self) -> bool {
        !self.bare_bodies.is_empty()
            || self
                .open
                .iter()
                .any(|open| open.loop_body || open.holds == Holds::Callback { armed: true })
    }

    /// Takes in the token at `at`, before the caller looks at it.
    fn take(&mut self, tokens: &[Token], at: usize) {
        let token = &tokens[at];
        let before = at.checked_sub(1).map(|b| &tokens[b]);
        let word = |i: Option<usize>| match i.and_then(|i| tokens.get(i)).map(|t| &t.tok) {
            Some(Tok::Word(word)) => Some(word.as_str()),
            _ => None,
        };
        // A body without braces ends with the block it is in, or at the end of a line where its
        // statement can end and the next line does not carry it on.
        let depth = self.open.len();
        let closes = matches!(token.tok, Tok::Punct('}' | ')' | ']') | Tok::HoleClose);
        let line_ends = before.is_some_and(|b| b.line < token.line && ends_statement(&b.tok))
            && !carries_on(&token.tok);
        while let Some(&started) = self.bare_bodies.last() {
            if depth > started || (depth == started && !closes && !line_ends) {
                break;
            }
            self.bare_bodies.pop();
        }
        let after_do = std::mem::take(&mut self.after_do);
        if std::mem::take(&mut self.body_next) {
            match token.tok {
                Tok::Punct('{') => {
                    let holds = if word(at.checked_sub(1)) == Some("do") {
                        Holds::DoBody
                    } else {
                        Holds::Other
                    };
                    self.open.push(Bracket {
                        loop_body: true,
                        holds,
                    });
                    return;
                }
                Tok::Punct(';') => return,
                _ => self.bare_bodies.push(depth),
            }
        }
        match &token.tok {
            Tok::Punct('(') => {
                let before = at.checked_sub(1);
                let two_before = before.and_then(|b| b.checked_sub(1));
                let head = match word(before) {
                    Some("for") => true,
                    // A `do` loop's `while` is its end.
                    Some("while") => !after_do,
                    Some("await") => word(two_before) == Some("for"),
                    _ => false,
                };
                let after_dot = two_before.is_some_and(|b| tokens[b].tok == Tok::Punct('.'));
                let callback = word(before)
                    .is_some_and(|w| TIMERS.contains(&w) || (after_dot && CALLBACKS.contains(&w)));
                // A grouping `(` in a callback's argument, as in `.then((() => …))` or
                // `.then(ok ? (() => …) : f)`: no call, since an operator or the argument's start
                // comes before it, and what it holds may be the callback.
                let starts_argument = before.is_some_and(
                    |b| matches!(tokens[b].tok, Tok::Punct(c) if !matches!(c, ')' | ']' | '}')),
                );
                let in_callback = matches!(
                    self.open.last(),
                    Some(Bracket {
                        holds: Holds::Callback { .. },
                        ..
                    })
                );
                let holds = match (head, callback || (starts_argument && in_callback)) {
                    (true, _) => Holds::LoopHead,
                    (_, true) => Holds::Callback { armed: false },
                    _ => Holds::Other,
                };
                self.open.push(Bracket {
                    loop_body: false,
                    holds,
                });
            }
            Tok::Punct('{' | '[') | Tok::HoleOpen => self.open.push(Bracket {
                loop_body: false,
                holds: Holds::Other,
            }),
            Tok::Punct(')' | '}' | ']') | Tok::HoleClose => match self.open.pop() {
                Some(Bracket {
                    holds: Holds::LoopHead,
                    ..
                }) => self.body_next = true,
                Some(Bracket {
                    holds: Holds::DoBody,
                    ..
                }) => self.after_do = true,
                _ => {}
            },
            Tok::Punct(';') => {
                if self.bare_bodies.last() == Some(&depth) {
                    self.bare_bodies.pop();
                }
            }
            // A function among a callback's arguments: `x => …` or `function () {…}`.
            Tok::Punct('>') if before.is_some_and(|b| b.tok == Tok::Punct('=')) => self.arm(true),
            Tok::Word(word) if word == "function" => self.arm(true),
            Tok::Punct(',') => self.arm(false),
            // The `while` that ends a `do` loop keeps the mark for its `(`.
            Tok::Word(word) if word == "while" => self.after_do = after_do,
            Tok::Word(word) if word == "do" => self.body_next = true,
            _ => {}
        }
    }

    /// Marks whether the callback arguments being read are a function.
    fn arm(&mut self, function: bool) {
        if let Some(Bracket {
            holds: Holds::Callback { armed },
            ..
        }) = self.open.last_mut()
        {
            *armed = function;
        }
    }
}

/// Whether a statement can end after this token.
fn ends_statement(tok: &Tok) -> bool {
    match tok {
        Tok::Word(word) => !GOES_ON.contains(&word.as_str()),
        Tok::Num(_) | Tok::Str(_) | Tok::Regex | Tok::TemplateClose => true,
        Tok::Punct(c) => matches!(c, ')' | ']' | '}'),
        _ => false,
    }
}

/// Whether a line that starts with this token carries on the statement before it.
fn carries_on(tok: &Tok) -> bool {
    match tok {
        Tok::Punct(c) => ".?,:+-*/%&|^=<>([".contains(*c),
        Tok::TemplateOpen => true,
        _ => false,
    }
}

/// One token of the script, and the line it starts on (from 1).
struct Token {
    tok: Tok,
    line: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    Word(String),
    Punct(char),
    /// A string literal, as it reads with its escapes resolved.
    Str(String),
    Num(String),
    Regex,
    /// A spread's `...`, which is no member access.
    Spread,
    TemplateOpen,
    /// A template literal's text between its start, end and `${…}`, as written.
    Text(String),
    HoleOpen,
    HoleClose,
    TemplateClose,
}

fn tokens(code: &str) -> Vec<Token> {
    let chars: Vec<char> = code.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut at = 0;
    let mut line = 1;
    // Each open `${` of a template literal, with the braces opened inside it.
    let mut templates: Vec<usize> = Vec::new();
    let mut in_text = false;
    // The template text read so far, and the line it started on.
    let mut text = String::new();
    let mut text_line = 1;
    // The last token: its closing character, or `a` for a word and `0` for a literal.
    let mut prev = ' ';
    let mut prev_word = String::new();
    // Each open `(`: whether it holds the condition of `if`, `while`, `for` or `with`, after
    // which a statement, so maybe a regex, starts.
    let mut parens: Vec<bool> = Vec::new();
    while at < chars.len() {
        let c = chars[at];
        if in_text {
            if text.is_empty() {
                text_line = line;
            }
            let ends = match c {
                '`' => Some(Tok::TemplateClose),
                '$' if chars.get(at + 1) == Some(&'{') => Some(Tok::HoleOpen),
                _ => None,
            };
            if let Some(tok) = ends {
                if !text.is_empty() {
                    let text = Tok::Text(std::mem::take(&mut text));
                    out.push(Token {
                        tok: text,
                        line: text_line,
                    });
                }
                in_text = false;
                if tok == Tok::HoleOpen {
                    templates.push(0);
                    prev = '{';
                    at += 1;
                } else {
                    prev = '0';
                }
                out.push(Token { tok, line });
                at += 1;
                continue;
            }
            text.push(c);
            line += usize::from(c == '\n');
            if c == '\\'
                && let Some(&next) = chars.get(at + 1)
            {
                text.push(next);
                line += usize::from(next == '\n');
                at += 1;
            }
            at += 1;
            continue;
        }
        let here = line;
        let token = |tok: Tok| Token { tok, line: here };
        match c {
            '\n' => {
                line += 1;
                at += 1;
            }
            c if c.is_whitespace() => at += 1,
            '/' if chars.get(at + 1) == Some(&'/') => {
                while at < chars.len() && chars[at] != '\n' {
                    at += 1;
                }
            }
            '/' if chars.get(at + 1) == Some(&'*') => {
                at += 2;
                while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                    line += usize::from(chars[at] == '\n');
                    at += 1;
                }
                at += 2;
            }
            '/' if regex_may_start(prev, &prev_word) => {
                if let Some(end) = regex_end(&chars, at) {
                    out.push(token(Tok::Regex));
                    at = end;
                    prev = '0';
                } else {
                    out.push(token(Tok::Punct('/')));
                    prev = '/';
                    at += 1;
                }
            }
            '\'' | '"' => {
                let mut literal = String::new();
                at += 1;
                while at < chars.len() && chars[at] != c && chars[at] != '\n' {
                    if chars[at] == '\\' {
                        at += 1;
                        match chars.get(at) {
                            Some('\n') => line += 1,
                            Some('n') => literal.push('\n'),
                            Some('t') => literal.push('\t'),
                            Some(&other) => literal.push(other),
                            None => {}
                        }
                    } else {
                        literal.push(chars[at]);
                    }
                    at += 1;
                }
                at += 1;
                out.push(token(Tok::Str(literal)));
                prev = '0';
            }
            '`' => {
                out.push(token(Tok::TemplateOpen));
                in_text = true;
                at += 1;
            }
            '{' => {
                if let Some(open) = templates.last_mut() {
                    *open += 1;
                }
                out.push(token(Tok::Punct('{')));
                prev = '{';
                at += 1;
            }
            '}' => {
                let tok = match templates.last_mut() {
                    Some(0) => {
                        templates.pop();
                        in_text = true;
                        Tok::HoleClose
                    }
                    Some(open) => {
                        *open -= 1;
                        Tok::Punct('}')
                    }
                    None => Tok::Punct('}'),
                };
                out.push(token(tok));
                prev = '}';
                at += 1;
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = at;
                while at < chars.len()
                    && (chars[at].is_alphanumeric() || chars[at] == '_' || chars[at] == '$')
                {
                    at += 1;
                }
                let word: String = chars[start..at].iter().collect();
                out.push(token(Tok::Word(word.clone())));
                prev = 'a';
                prev_word = word;
            }
            c if c.is_ascii_digit() => {
                let start = at;
                while at < chars.len()
                    && (chars[at].is_alphanumeric() || matches!(chars[at], '.' | '_'))
                {
                    at += 1;
                }
                out.push(token(Tok::Num(chars[start..at].iter().collect())));
                prev = '0';
            }
            '(' => {
                let condition =
                    prev == 'a' && matches!(prev_word.as_str(), "if" | "while" | "for" | "with");
                parens.push(condition);
                out.push(token(Tok::Punct('(')));
                prev = '(';
                at += 1;
            }
            ')' => {
                prev = if parens.pop() == Some(true) { ';' } else { ')' };
                out.push(token(Tok::Punct(')')));
                at += 1;
            }
            // A spread's dots are no member access.
            '.' if chars[at..].starts_with(&['.', '.', '.']) => {
                out.push(token(Tok::Spread));
                prev = ',';
                at += 3;
            }
            c => {
                out.push(token(Tok::Punct(c)));
                prev = c;
                at += 1;
            }
        }
    }
    out
}

/// Whether a `/` after this token opens a regex literal rather than dividing.
fn regex_may_start(prev: char, prev_word: &str) -> bool {
    match prev {
        ')' | ']' | '}' | '0' => false,
        'a' => matches!(
            prev_word,
            "return"
                | "typeof"
                | "case"
                | "in"
                | "of"
                | "delete"
                | "void"
                | "throw"
                | "else"
                | "do"
        ),
        _ => true,
    }
}

/// Where the regex literal that starts at `from` ends, after its flags; none when the line ends
/// first, so it was no regex.
fn regex_end(chars: &[char], from: usize) -> Option<usize> {
    let mut at = from + 1;
    let mut in_class = false;
    while at < chars.len() {
        match chars[at] {
            '\n' => return None,
            '\\' => at += 1,
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => {
                at += 1;
                while at < chars.len() && chars[at].is_alphabetic() {
                    at += 1;
                }
                return Some(at);
            }
            _ => {}
        }
        at += 1;
    }
    None
}
