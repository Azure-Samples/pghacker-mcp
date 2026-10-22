// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The subset of bash the grep tool reads: words with quotes and escapes,
//! `$HOME`, brace expansion, redirections, one pipeline, an optional
//! `cd DIR &&` before it and an optional `|| true` or `|| echo ...` after.

/// A piece of a word, quoted (taken literally) or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub text: String,
    pub quoted: bool,
}

/// A shell word before expansion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Word {
    pub parts: Vec<Part>,
}

impl Word {
    /// The word's text after quote removal.
    pub fn text(&self) -> String {
        self.parts.iter().map(|p| p.text.as_str()).collect()
    }

    /// Whether an unquoted `*`, `?` or `[` makes this a wildcard.
    pub fn has_wildcards(&self) -> bool {
        self.parts
            .iter()
            .any(|p| !p.quoted && p.text.contains(['*', '?', '[']))
    }

    /// The word as a fnmatch pattern: quoted characters escaped.
    pub fn glob_pattern(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            for c in p.text.chars() {
                if p.quoted && matches!(c, '*' | '?' | '[' | ']' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
        }
        out
    }

    fn push(&mut self, c: char, quoted: bool) {
        match self.parts.last_mut() {
            Some(p) if p.quoted == quoted => p.text.push(c),
            _ => self.parts.push(Part {
                text: c.to_string(),
                quoted,
            }),
        }
    }

    fn push_str(&mut self, s: &str, quoted: bool) {
        for c in s.chars() {
            self.push(c, quoted);
        }
    }
}

/// A redirection such as `2>/dev/null` or `2>&1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub fd: Option<u32>,
    pub op: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(Word),
    Op(&'static str),
    Redirect(Redirect),
}

/// A simple command: words and redirections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

impl Command {
    /// The command's name (its first word), after `VAR=value` assignments.
    pub fn name(&self) -> Option<String> {
        self.words.first().map(Word::text)
    }

    /// Whether stderr goes to /dev/null.
    pub fn stderr_null(&self) -> bool {
        self.redirects
            .iter()
            .any(|r| r.fd == Some(2) && r.op == ">" && r.target == "/dev/null")
            || self
                .redirects
                .iter()
                .any(|r| r.fd.is_none() && r.op == "&>" && r.target == "/dev/null")
    }

    /// Whether stderr joins stdout (`2>&1`).
    pub fn stderr_to_stdout(&self) -> bool {
        self.redirects
            .iter()
            .any(|r| r.fd == Some(2) && r.op == ">&" && r.target == "1")
    }
}

/// A parsed command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Script {
    /// The folder of a leading `cd DIR &&`.
    pub cd: Option<Word>,
    /// The pipeline's commands.
    pub stages: Vec<Command>,
    /// The command after `||`.
    pub fallback: Option<Command>,
}

/// Splits a command line into tokens.
fn tokenize(line: &str) -> Result<Vec<Token>, String> {
    let c: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut word: Option<Word> = None;
    let mut i = 0;
    let flush = |word: &mut Option<Word>, tokens: &mut Vec<Token>| {
        if let Some(w) = word.take() {
            tokens.push(Token::Word(w));
        }
    };
    while i < c.len() {
        let ch = c[i];
        match ch {
            ' ' | '\t' => {
                flush(&mut word, &mut tokens);
                i += 1;
            }
            '\n' => {
                flush(&mut word, &mut tokens);
                tokens.push(Token::Op(";"));
                i += 1;
            }
            '#' if word.is_none() => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '\'' => {
                let end = (i + 1..c.len())
                    .find(|&j| c[j] == '\'')
                    .ok_or("unterminated ' quote")?;
                let w = word.get_or_insert_with(Word::default);
                let text: String = c[i + 1..end].iter().collect();
                if text.is_empty() {
                    w.parts.push(Part { text, quoted: true });
                } else {
                    w.push_str(&text, true);
                }
                i = end + 1;
            }
            '"' => {
                let w = word.get_or_insert_with(Word::default);
                let mut j = i + 1;
                let mut empty = true;
                loop {
                    let Some(&d) = c.get(j) else {
                        return Err("unterminated \" quote".into());
                    };
                    match d {
                        '"' => break,
                        '\\' if matches!(c.get(j + 1), Some('$' | '`' | '"' | '\\' | '\n')) => {
                            if c[j + 1] != '\n' {
                                w.push(c[j + 1], true);
                                empty = false;
                            }
                            j += 2;
                        }
                        '$' => {
                            let (text, used) = expand_dollar(&c, j)?;
                            w.push_str(&text, true);
                            empty = false;
                            j += used;
                        }
                        '`' => return Err("command substitution is not supported".into()),
                        d => {
                            w.push(d, true);
                            empty = false;
                            j += 1;
                        }
                    }
                }
                if empty {
                    w.parts.push(Part {
                        text: String::new(),
                        quoted: true,
                    });
                }
                i = j + 1;
            }
            '\\' => {
                match c.get(i + 1) {
                    Some('\n') => {}
                    Some(&d) => word.get_or_insert_with(Word::default).push(d, true),
                    None => word.get_or_insert_with(Word::default).push('\\', true),
                }
                i += 2;
            }
            '$' if c.get(i + 1) == Some(&'\'') => {
                let (text, used) = ansi_c_string(&c, i + 2)?;
                let w = word.get_or_insert_with(Word::default);
                if text.is_empty() {
                    w.parts.push(Part { text, quoted: true });
                } else {
                    w.push_str(&text, true);
                }
                i += 2 + used;
            }
            '$' => {
                let (text, used) = expand_dollar(&c, i)?;
                word.get_or_insert_with(Word::default).push_str(&text, true);
                i += used;
            }
            '`' => return Err("command substitution is not supported".into()),
            '|' | '&' | ';' | '(' | ')' => {
                flush(&mut word, &mut tokens);
                let two: String = c[i..(i + 2).min(c.len())].iter().collect();
                let (op, used): (&'static str, usize) = match two.as_str() {
                    "||" => ("||", 2),
                    "|&" => ("|&", 2),
                    "&&" => ("&&", 2),
                    "&>" => {
                        i += 2;
                        let target = redirect_target(&c, &mut i)?;
                        tokens.push(Token::Redirect(Redirect {
                            fd: None,
                            op: "&>".into(),
                            target,
                        }));
                        continue;
                    }
                    ";;" => return Err("case statements are not supported".into()),
                    _ => match ch {
                        '|' => ("|", 1),
                        '&' => ("&", 1),
                        ';' => (";", 1),
                        '(' => ("(", 1),
                        _ => (")", 1),
                    },
                };
                tokens.push(Token::Op(op));
                i += used;
            }
            '<' | '>' => {
                // A word of digits right before the operator is its descriptor.
                let fd = match &word {
                    Some(w)
                        if w.parts.len() == 1
                            && !w.parts[0].quoted
                            && w.parts[0].text.chars().all(|d| d.is_ascii_digit()) =>
                    {
                        let fd = w.parts[0].text.parse().ok();
                        word = None;
                        fd
                    }
                    _ => {
                        flush(&mut word, &mut tokens);
                        None
                    }
                };
                let mut op = ch.to_string();
                i += 1;
                if ch == '>' && matches!(c.get(i), Some('>' | '|')) {
                    if c[i] == '>' {
                        op.push('>');
                    }
                    i += 1;
                } else if ch == '<' && c.get(i) == Some(&'<') {
                    return Err("here-documents are not supported".into());
                }
                if c.get(i) == Some(&'&') {
                    op.push('&');
                    i += 1;
                }
                let target = redirect_target(&c, &mut i)?;
                tokens.push(Token::Redirect(Redirect {
                    fd: fd.or(Some(if ch == '<' { 0 } else { 1 })),
                    op,
                    target,
                }));
            }
            _ => {
                word.get_or_insert_with(Word::default).push(ch, false);
                i += 1;
            }
        }
    }
    flush(&mut word, &mut tokens);
    Ok(tokens)
}

/// The word after a redirection operator.
fn redirect_target(c: &[char], i: &mut usize) -> Result<String, String> {
    while matches!(c.get(*i), Some(' ' | '\t')) {
        *i += 1;
    }
    let start = *i;
    while *i < c.len() && !" \t\n|&;<>()".contains(c[*i]) {
        *i += 1;
    }
    if start == *i {
        return Err("a redirection needs a target".into());
    }
    let target: String = c[start..*i].iter().collect();
    Ok(target.trim_matches(['\'', '"']).to_owned())
}

/// `$HOME`, `${HOME}` or `$PWD` at `c[i]`: the text and the characters used.
fn expand_dollar(c: &[char], i: usize) -> Result<(String, usize), String> {
    let rest: String = c[i..].iter().take(64).collect();
    for (form, text) in [
        ("${HOME}", "~"),
        ("$HOME", "~"),
        ("${PWD}", "."),
        ("$PWD", "."),
    ] {
        if let Some(after) = rest.strip_prefix(form) {
            let next = after.chars().next();
            if form.ends_with('}') || !next.is_some_and(|n| n.is_alphanumeric() || n == '_') {
                return Ok((text.to_owned(), form.chars().count()));
            }
        }
    }
    if rest.starts_with("$(") {
        return Err("command substitution is not supported".into());
    }
    let name: String = rest[1..]
        .chars()
        .take_while(|n| n.is_alphanumeric() || *n == '_' || *n == '{' || *n == '}')
        .collect();
    if name.is_empty() {
        return Ok(("$".into(), 1));
    }
    Err(format!("shell variables are not supported: ${name}"))
}

/// The body of `$'...'` from `c[start]`: the text and the characters used,
/// the closing quote included.
fn ansi_c_string(c: &[char], start: usize) -> Result<(String, usize), String> {
    let mut out = String::new();
    let mut i = start;
    loop {
        let Some(&ch) = c.get(i) else {
            return Err("unterminated $' quote".into());
        };
        if ch == '\'' {
            return Ok((out, i + 1 - start));
        }
        if ch != '\\' {
            out.push(ch);
            i += 1;
            continue;
        }
        let Some(&d) = c.get(i + 1) else {
            return Err("unterminated $' quote".into());
        };
        i += 2;
        match d {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(d),
            'x' | 'u' | 'U' => {
                let max = match d {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let hex: String = c[i..]
                    .iter()
                    .take(max)
                    .take_while(|h| h.is_ascii_hexdigit())
                    .collect();
                if hex.is_empty() {
                    out.push('\\');
                    out.push(d);
                } else {
                    i += hex.len();
                    let code = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                    out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                }
            }
            d => {
                out.push('\\');
                out.push(d);
            }
        }
    }
}

/// Parses a command line into an optional `cd`, a pipeline and an optional
/// `||` fallback.
pub fn parse(line: &str) -> Result<Script, String> {
    if line.len() > MAX_COMMAND {
        return Err(format!("the command is longer than {MAX_COMMAND} bytes"));
    }
    let tokens = tokenize(line)?;
    let mut lists: Vec<(Vec<Token>, Option<&'static str>)> = Vec::new();
    let mut cur = Vec::new();
    for tok in tokens {
        match tok {
            Token::Op(op @ ("&&" | ";" | "||")) => {
                lists.push((std::mem::take(&mut cur), Some(op)));
            }
            Token::Op("&") => return Err("background jobs (&) are not supported".into()),
            Token::Op("(" | ")") => return Err("subshells are not supported".into()),
            tok => cur.push(tok),
        }
    }
    lists.push((cur, None));
    // Trailing separators and empty commands.
    lists.retain(|(tokens, _)| !tokens.is_empty());
    let mut script = Script::default();
    let mut k = 0;
    if let Some((first, sep)) = lists.first() {
        let cmd = simple(first)?;
        if cmd.len() == 1 && cmd[0].name().as_deref() == Some("cd") && lists.len() > 1 {
            if *sep == Some("||") {
                return Err("cd ... || is not supported".into());
            }
            let words = &cmd[0].words;
            if words.len() != 2 {
                return Err("cd needs exactly one folder".into());
            }
            script.cd = Some(words[1].clone());
            k = 1;
        }
    }
    let Some((tokens, sep)) = lists.get(k) else {
        return Err("no command".into());
    };
    script.stages = simple(tokens)?;
    match (sep, lists.get(k + 1)) {
        (None, None) => {}
        (Some("||"), Some((tokens, None))) => {
            let fallback = simple(tokens)?;
            if fallback.len() != 1 {
                return Err("only one command may follow ||".into());
            }
            script.fallback = fallback.into_iter().next();
        }
        _ => {
            return Err(
                "run one grep pipeline per call (optionally after cd DIR &&, \
                 optionally followed by || true or || echo ...)"
                    .into(),
            );
        }
    }
    Ok(script)
}

/// Commands of a pipeline.
fn simple(tokens: &[Token]) -> Result<Vec<Command>, String> {
    let mut stages = vec![Command::default()];
    for tok in tokens {
        match tok {
            Token::Op("|") => stages.push(Command::default()),
            Token::Op("|&") => return Err("|& is not supported; use 2>&1 |".into()),
            Token::Op(op) => return Err(format!("unexpected {op}")),
            Token::Word(w) => {
                let cmd = stages.last_mut().unwrap();
                // Leading VAR=value assignments, such as LC_ALL=C, are ignored.
                let text = w.text();
                let is_assign = cmd.words.is_empty()
                    && w.parts.first().is_some_and(|p| !p.quoted)
                    && text.split_once('=').is_some_and(|(name, _)| {
                        !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                            && !name.starts_with(|c: char| c.is_ascii_digit())
                    });
                if !is_assign {
                    cmd.words.push(w.clone());
                }
            }
            Token::Redirect(r) => stages.last_mut().unwrap().redirects.push(r.clone()),
        }
    }
    if stages.iter().any(|s| s.words.is_empty()) {
        return Err("empty command in the pipeline".into());
    }
    if stages.len() > MAX_STAGES {
        return Err(format!("the pipeline has more than {MAX_STAGES} commands"));
    }
    Ok(stages)
}

/// The most commands of a pipeline.
pub const MAX_STAGES: usize = 16;

/// The longest command line, in bytes.
pub const MAX_COMMAND: usize = 8 * 1024;

/// The most words brace expansion may make of one command.
pub const MAX_WORDS: usize = 10_000;

/// The most characters brace expansion may write for one command, words
/// it expands further included.
pub const MAX_EXPANDED: usize = 1 << 22;

/// The longest `{a..b..step}` body: three i64s and two `..`.
const MAX_SEQUENCE: usize = 64;

/// What brace expansion may still make of a command.
#[derive(Clone, Copy, Debug)]
pub struct Room {
    words: usize,
    chars: usize,
}

impl Default for Room {
    fn default() -> Self {
        Self {
            words: MAX_WORDS,
            chars: MAX_EXPANDED,
        }
    }
}

impl Room {
    fn take_chars(&mut self, n: usize) -> Result<(), String> {
        self.chars = self
            .chars
            .checked_sub(n)
            .ok_or_else(|| format!("brace expansion makes more than {MAX_EXPANDED} characters"))?;
        Ok(())
    }
}

fn too_many_words() -> String {
    format!("brace expansion makes more than {MAX_WORDS} words")
}

/// Bash brace expansion of an unquoted `{a,b}` or `{1..3}` (nested
/// braces included), within `room`. Returns the word itself when there is
/// none.
pub fn brace_expand(word: &Word, room: &mut Room) -> Result<Vec<Word>, String> {
    let mut out = Vec::new();
    room.take_chars(word.parts.iter().map(|p| p.text.len()).sum::<usize>() + 1)?;
    // Depth first, alternatives in order: the stack holds them reversed.
    let mut stack = vec![word.clone()];
    while let Some(word) = stack.pop() {
        // Flatten to (char, quoted) to find an unquoted, balanced brace
        // pair with a top-level comma or a sequence.
        let chars: Vec<(char, bool)> = word
            .parts
            .iter()
            .flat_map(|p| p.text.chars().map(move |c| (c, p.quoted)))
            .collect();
        // Each pending word makes at least one.
        let room_left = room.words.saturating_sub(stack.len());
        let Some((open, close, alternatives)) = group(&chars, room_left)? else {
            room.words = room.words.checked_sub(1).ok_or_else(too_many_words)?;
            out.push(word);
            continue;
        };
        let prefix = &chars[..open];
        let suffix = &chars[close + 1..];
        for alt in alternatives.iter().rev() {
            room.take_chars(prefix.len() + alt.len() + suffix.len() + 1)?;
            let mut w = Word::default();
            for &(c, q) in prefix.iter().chain(alt.iter()).chain(suffix.iter()) {
                w.push(c, q);
            }
            stack.push(w);
        }
    }
    Ok(out)
}

type Alternatives = Vec<Vec<(char, bool)>>;

/// The first brace group of `chars` that expands: its braces and its
/// alternatives, at most `max` of them.
fn group(
    chars: &[(char, bool)],
    max: usize,
) -> Result<Option<(usize, usize, Alternatives)>, String> {
    // Each unquoted `{` with its matching `}` and its own commas.
    let mut close: Vec<Option<usize>> = vec![None; chars.len()];
    let mut commas: Vec<Vec<usize>> = vec![Vec::new(); chars.len()];
    let mut open = Vec::new();
    for (j, &(c, quoted)) in chars.iter().enumerate() {
        match (c, quoted) {
            ('{', false) => open.push(j),
            ('}', false) => {
                if let Some(o) = open.pop() {
                    close[o] = Some(j);
                }
            }
            (',', false) => {
                if let Some(&o) = open.last() {
                    commas[o].push(j);
                }
            }
            _ => {}
        }
    }
    for open in 0..chars.len() {
        let Some(close) = close[open] else { continue };
        let commas = &commas[open];
        if commas.is_empty() {
            if close - open - 1 > MAX_SEQUENCE {
                continue;
            }
            let body: String = chars[open + 1..close].iter().map(|c| c.0).collect();
            let Some((count, values)) = sequence(&body) else {
                continue;
            };
            if count > max as u128 {
                return Err(too_many_words());
            }
            let alternatives = values
                .map(|s| s.chars().map(|c| (c, true)).collect())
                .collect();
            return Ok(Some((open, close, alternatives)));
        }
        if commas.len() + 1 > max {
            return Err(too_many_words());
        }
        let mut alternatives = Vec::new();
        let mut start = open + 1;
        for &comma in commas.iter().chain(std::iter::once(&close)) {
            alternatives.push(chars[start..comma].to_vec());
            start = comma + 1;
        }
        return Ok(Some((open, close, alternatives)));
    }
    Ok(None)
}

/// `1..3`, `a..c` or `1..10..2`: how many values there are, and the values.
fn sequence(body: &str) -> Option<(u128, Box<dyn Iterator<Item = String>>)> {
    let parts: Vec<&str> = body.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let step: i64 = parts.get(2).map_or(Some(1), |s| s.parse().ok())?.max(1);
    let step = step as u64;
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let count = u128::from(a.abs_diff(b) / step) + 1;
        let (a, step) = (i128::from(a), i128::from(step));
        let dir = if a <= i128::from(b) { 1 } else { -1 };
        let values = (0..count).map(move |k| (a + dir * k as i128 * step).to_string());
        return Some((count, Box::new(values)));
    }
    let (a, b) = (parts[0], parts[1]);
    if a.chars().count() == 1 && b.chars().count() == 1 {
        let (a, b) = (a.chars().next()? as u32, b.chars().next()? as u32);
        let count = u128::from(u64::from(a.abs_diff(b)) / step) + 1;
        let (a, step) = (i128::from(a), i128::from(step));
        let dir = if a <= i128::from(b) { 1 } else { -1 };
        let values = (0..count).filter_map(move |k| {
            let c = u32::try_from(a + dir * k as i128 * step).ok()?;
            char::from_u32(c).map(String::from)
        });
        return Some((count, Box::new(values)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(cmd: &Command) -> Vec<String> {
        cmd.words.iter().map(Word::text).collect()
    }

    #[test]
    fn quotes_and_escapes() {
        let s = parse(r#"grep -rn 'a b' "c\"d" e\ f $'g\th' "$HOME/x" -e"" src"#).unwrap();
        assert_eq!(
            words(&s.stages[0]),
            vec![
                "grep", "-rn", "a b", "c\"d", "e f", "g\th", "~/x", "-e", "src"
            ]
        );
        assert!(parse("grep 'open").is_err());
        assert!(parse("grep $(echo x) .").is_err());
        assert!(parse("grep $FOO .").is_err());
    }

    #[test]
    fn pipelines_redirects_and_lists() {
        let s = parse("cd /src/pg && grep -rn foo . 2>/dev/null | head -20 || echo none").unwrap();
        assert_eq!(s.cd.unwrap().text(), "/src/pg");
        assert_eq!(s.stages.len(), 2);
        assert!(s.stages[0].stderr_null());
        assert_eq!(words(&s.stages[1]), vec!["head", "-20"]);
        assert_eq!(words(&s.fallback.unwrap()), vec!["echo", "none"]);
        let s = parse("LC_ALL=C grep -c x f 2>&1 | sort | uniq -c").unwrap();
        assert_eq!(words(&s.stages[0]), vec!["grep", "-c", "x", "f"]);
        assert!(s.stages[0].stderr_to_stdout());
        assert_eq!(s.stages.len(), 3);
        assert!(parse("grep a f; grep b f").is_err());
        assert!(parse("grep a f &").is_err());
    }

    #[test]
    fn wildcards_remember_quoting() {
        let s = parse(r#"grep x src/*.c "a*" b\*"#).unwrap();
        let w = &s.stages[0].words;
        assert!(w[2].has_wildcards());
        assert!(!w[3].has_wildcards() && !w[4].has_wildcards());
        assert_eq!(w[2].glob_pattern(), "src/*.c");
    }

    #[test]
    fn braces_expand() {
        let s = parse("grep x src/{a,b{1,2}}.c '{q,r}' {1..3} --include=*.{c,h}").unwrap();
        let expanded: Vec<String> = s.stages[0]
            .words
            .iter()
            .flat_map(|w| brace_expand(w, &mut Room::default()).unwrap())
            .map(|w| w.text())
            .collect();
        assert_eq!(
            expanded,
            vec![
                "grep",
                "x",
                "src/a.c",
                "src/b1.c",
                "src/b2.c",
                "{q,r}",
                "1",
                "2",
                "3",
                "--include=*.c",
                "--include=*.h"
            ]
        );
    }

    fn expand(word: &str) -> Result<Vec<String>, String> {
        let word = Word {
            parts: vec![Part {
                text: word.into(),
                quoted: false,
            }],
        };
        Ok(brace_expand(&word, &mut Room::default())?
            .iter()
            .map(Word::text)
            .collect())
    }

    #[test]
    fn sequences() {
        assert_eq!(expand("{a,b}").unwrap(), ["a", "b"]);
        assert_eq!(expand("{1..3}").unwrap(), ["1", "2", "3"]);
        assert_eq!(expand("{3..1}").unwrap(), ["3", "2", "1"]);
        assert_eq!(expand("{1..10..2}").unwrap(), ["1", "3", "5", "7", "9"]);
        assert_eq!(expand("{10..1..4}").unwrap(), ["10", "6", "2"]);
        assert_eq!(expand("{a..c}").unwrap(), ["a", "b", "c"]);
        assert_eq!(expand("{e..a..2}").unwrap(), ["e", "c", "a"]);
        assert_eq!(expand("{-1..1}").unwrap(), ["-1", "0", "1"]);
        assert_eq!(expand("x{1..1}").unwrap(), ["x1"]);
        assert_eq!(expand("{1..}").unwrap(), ["{1..}"]);
        assert_eq!(expand("{{a,b}").unwrap(), ["{a", "{b"]);
    }

    #[test]
    fn sequences_at_the_i64_bounds() {
        assert_eq!(
            expand("{9223372036854775806..9223372036854775807}").unwrap(),
            ["9223372036854775806", "9223372036854775807"]
        );
        assert_eq!(
            expand("{-9223372036854775807..-9223372036854775808}").unwrap(),
            ["-9223372036854775807", "-9223372036854775808"]
        );
        assert_eq!(
            expand("{9223372036854775806..9223372036854775807..9223372036854775807}").unwrap(),
            ["9223372036854775806"]
        );
        assert_eq!(
            expand("{-9223372036854775808..9223372036854775807..9223372036854775807}").unwrap(),
            ["-9223372036854775808", "-1", "9223372036854775806"]
        );
        assert!(expand("{-9223372036854775808..9223372036854775807}").is_err());
    }

    #[test]
    fn expansion_is_capped() {
        let start = std::time::Instant::now();
        let err = expand("{1..99999999999}").unwrap_err();
        assert!(err.contains("more than 10000 words"), "{err}");
        assert!(expand("{\u{1}..\u{10FFFF}}").is_err());
        assert!(expand(&"{a,b}".repeat(16)).is_err());
        assert_eq!(expand(&"{a,b}".repeat(13)).unwrap().len(), 8192);
        assert!(expand(&"{a,b}".repeat(800)).is_err());
        assert!(expand(&format!("{}{}", "{".repeat(4000), "{a,b}".repeat(14))).is_err());
        assert!(expand(&format!("{{{}}}", ",".repeat(MAX_WORDS))).is_err());
        assert_eq!(
            expand(&format!("{{{}}}", ",".repeat(MAX_WORDS - 1)))
                .unwrap()
                .len(),
            MAX_WORDS
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        // Words share the room.
        let s = parse(&format!("grep {}", "{1..5000} ".repeat(3))).unwrap();
        let mut room = Room::default();
        let words = &s.stages[0].words;
        assert!(brace_expand(&words[1], &mut room).is_ok());
        assert!(brace_expand(&words[2], &mut room).is_ok());
        assert!(brace_expand(&words[3], &mut room).is_err());
    }

    #[test]
    fn long_commands_are_refused() {
        let ok = format!("grep {}", "x".repeat(MAX_COMMAND - 5));
        assert!(parse(&ok).is_ok());
        let err = parse(&format!("{ok}x")).unwrap_err();
        assert!(err.contains("longer than"), "{err}");
        let pipeline = |stages: usize| format!("grep x{}", " | cat".repeat(stages - 1));
        assert_eq!(
            parse(&pipeline(MAX_STAGES)).unwrap().stages.len(),
            MAX_STAGES
        );
        let err = parse(&pipeline(MAX_STAGES + 1)).unwrap_err();
        assert!(err.contains("more than 16 commands"), "{err}");
    }
}
