//! Reading a JavaScript snippet closely enough to find the identifiers it uses.
//!
//! This is no parser: it skips comments, string and regex literals and the text of template
//! literals (but not their `\${…}` expressions), so a word that only appears in a string is not a
//! use. A regex literal is told from a division by the token before it, and a `)` by whether
//! it closes an `if`, `while`, `for` or `with` condition.

/// An identifier the script uses, and the line it is on (from 1).
#[derive(Debug, PartialEq, Eq)]
pub struct Use {
    pub name: &'static str,
    pub line: usize,
}

/// The first of `names` the script uses as a variable: not a property (`x.name`, `{name: 1}`),
/// not inside a string, comment or regex.
pub fn first_use(code: &str, names: &[&'static str]) -> Option<Use> {
    let chars: Vec<char> = code.chars().collect();
    let mut at = 0;
    let mut line = 1;
    // Each open `\${` of a template literal, with the braces opened inside it.
    let mut templates: Vec<usize> = Vec::new();
    let mut in_text = false;
    // The last token: its closing character, or `a` for a word and `0` for a literal.
    let mut prev = ' ';
    let mut prev_word = String::new();
    // Each open `(`: whether it holds the condition of `if`, `while`, `for` or `with`, after
    // which a statement, so maybe a regex, starts.
    let mut parens: Vec<bool> = Vec::new();
    while at < chars.len() {
        let c = chars[at];
        if in_text {
            match c {
                '\\' => {
                    line += usize::from(chars.get(at + 1) == Some(&'\n'));
                    at += 1;
                }
                '\n' => line += 1,
                '`' => {
                    in_text = false;
                    prev = '0';
                }
                '$' if chars.get(at + 1) == Some(&'{') => {
                    templates.push(0);
                    in_text = false;
                    prev = '{';
                    at += 1;
                }
                _ => {}
            }
            at += 1;
            continue;
        }
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
                    at = end;
                    prev = '0';
                } else {
                    prev = '/';
                    at += 1;
                }
            }
            '\'' | '"' => {
                at += 1;
                while at < chars.len() && chars[at] != c && chars[at] != '\n' {
                    if chars[at] == '\\' {
                        line += usize::from(chars.get(at + 1) == Some(&'\n'));
                        at += 1;
                    }
                    at += 1;
                }
                at += 1;
                prev = '0';
            }
            '`' => {
                in_text = true;
                at += 1;
            }
            '{' => {
                if let Some(open) = templates.last_mut() {
                    *open += 1;
                }
                prev = '{';
                at += 1;
            }
            '}' => {
                match templates.last_mut() {
                    Some(0) => {
                        templates.pop();
                        in_text = true;
                    }
                    Some(open) => *open -= 1,
                    None => {}
                }
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
                let property = prev == '.';
                let key = matches!(prev, '{' | ',') && next_is(&chars, at, ':');
                if let Some(name) = names.iter().find(|name| **name == word)
                    && !property
                    && !key
                {
                    return Some(Use { name, line });
                }
                prev = 'a';
                prev_word = word;
            }
            c if c.is_ascii_digit() => {
                while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '.') {
                    at += 1;
                }
                prev = '0';
            }
            '(' => {
                let condition =
                    prev == 'a' && matches!(prev_word.as_str(), "if" | "while" | "for" | "with");
                parens.push(condition);
                prev = '(';
                at += 1;
            }
            ')' => {
                prev = if parens.pop() == Some(true) { ';' } else { ')' };
                at += 1;
            }
            // A spread's dots are no member access.
            '.' if chars[at..].starts_with(&['.', '.', '.']) => {
                prev = ',';
                at += 3;
            }
            c => {
                prev = c;
                at += 1;
            }
        }
    }
    None
}

fn next_is(chars: &[char], from: usize, wanted: char) -> bool {
    chars[from..]
        .iter()
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| *c == wanted)
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
