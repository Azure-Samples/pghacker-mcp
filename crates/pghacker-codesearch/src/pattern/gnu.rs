//! The rules that differ from ripgrep: in a basic regex `\(`, `\|`, `\{`,
//! `\+` and `\?` are the operators; `^` is an anchor only at the start of
//! the pattern, a group or an alternative, and `$` only at the end; `*` (and
//! `\{`) with nothing to repeat is a literal; in an extended regex a
//! repetition with nothing to repeat applies to the empty string, a `{` that
//! does not start an interval is a literal, and so is an unmatched `)`. `\<`
//! and `\>` are word starts and ends; other escaped letters, such as `\d` or
//! `\t`, are the letters themselves. A bracket expression is POSIX: `\` is a
//! literal inside it.

/// A pattern in Rust syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converted {
    pub rust: String,
    /// The number of capturing groups.
    pub groups: u32,
    /// Whether the pattern has backreferences (Rust's regex crate lacks
    /// them, fancy-regex has them).
    pub backrefs: bool,
}

/// A basic (`extended == false`) or extended regex as Rust syntax.
/// `group_offset` renumbers backreferences when patterns are joined.
pub fn gnu_to_rust(pattern: &str, extended: bool, group_offset: u32) -> Result<Converted, String> {
    let mut parser = Parser {
        lex: Lexer {
            c: pattern.chars().collect(),
            pos: 0,
            ere: extended,
            laststart: true,
            lasttok: Tok::End,
            parens: 0,
        },
        tok: Tok::End,
        groups: 0,
        completed: Vec::new(),
        backrefs: false,
    };
    parser.tok = parser.lex.lex()?;
    let node = parser.regexp()?;
    if parser.tok != Tok::End {
        return Err("Unmatched ) or \\)".into());
    }
    let mut rust = String::with_capacity(pattern.len() + 8);
    emit(&node, group_offset, &mut rust);
    Ok(Converted {
        rust,
        groups: parser.groups,
        backrefs: parser.backrefs,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    End,
    Char(char),
    Any,
    Class(String),
    BegLine,
    EndLine,
    BegWord,
    EndWord,
    LimWord,
    NotLimWord,
    Backref(u32),
    Star,
    Plus,
    QMark,
    Rep(u32, Option<u32>),
    Or,
    LParen,
    RParen,
}

const RE_DUP_MAX: u32 = 0x7fff;

struct Lexer {
    c: Vec<char>,
    pos: usize,
    ere: bool,
    /// A repetition here would have nothing to repeat.
    laststart: bool,
    lasttok: Tok,
    parens: usize,
}

impl Lexer {
    fn left(&self) -> usize {
        self.c.len() - self.pos
    }

    fn at(&self, k: usize) -> Option<char> {
        self.c.get(self.pos + k).copied()
    }

    fn lex(&mut self) -> Result<Tok, String> {
        let tok = self.lex_inner()?;
        self.lasttok = tok.clone();
        Ok(tok)
    }

    fn normal(&mut self, ch: char) -> Tok {
        self.laststart = false;
        Tok::Char(ch)
    }

    fn lex_inner(&mut self) -> Result<Tok, String> {
        let mut backslash = false;
        loop {
            if self.left() == 0 {
                if backslash {
                    return Err("Trailing backslash".into());
                }
                return Ok(Tok::End);
            }
            let ch = self.c[self.pos];
            self.pos += 1;
            // Operators the basic syntax spells with a backslash and the
            // extended syntax without.
            let bk_op = backslash != self.ere;
            match ch {
                '\\' if !backslash => {
                    if self.left() == 0 {
                        return Err("Trailing backslash".into());
                    }
                    backslash = true;
                    continue;
                }
                '^' if !backslash => {
                    if self.ere || matches!(self.lasttok, Tok::End | Tok::LParen | Tok::Or) {
                        return Ok(Tok::BegLine);
                    }
                    return Ok(self.normal(ch));
                }
                '$' if !backslash => {
                    let bk = usize::from(!self.ere);
                    let next_is = |want: char| {
                        let k = bk & usize::from(self.at(0) == Some('\\'));
                        self.left() > bk && self.at(k) == Some(want)
                    };
                    if self.ere || self.left() == 0 || next_is(')') || next_is('|') {
                        return Ok(Tok::EndLine);
                    }
                    return Ok(self.normal(ch));
                }
                '1'..='9' if backslash => {
                    self.laststart = false;
                    return Ok(Tok::Backref(ch as u32 - '0' as u32));
                }
                '`' if backslash => return Ok(Tok::BegLine),
                '\'' if backslash => return Ok(Tok::EndLine),
                '<' if backslash => return Ok(Tok::BegWord),
                '>' if backslash => return Ok(Tok::EndWord),
                'b' if backslash => return Ok(Tok::LimWord),
                'B' if backslash => return Ok(Tok::NotLimWord),
                '?' | '+' if bk_op => {
                    if self.laststart && !self.ere {
                        return Ok(self.normal(ch));
                    }
                    return Ok(if ch == '?' { Tok::QMark } else { Tok::Plus });
                }
                '*' if !backslash => {
                    if self.laststart && !self.ere {
                        return Ok(self.normal(ch));
                    }
                    return Ok(Tok::Star);
                }
                '{' if bk_op => {
                    if let Some(tok) = self.interval(ch)? {
                        return Ok(tok);
                    }
                    return Ok(self.normal(ch));
                }
                '|' if bk_op => {
                    self.laststart = true;
                    return Ok(Tok::Or);
                }
                '(' if bk_op => {
                    self.parens += 1;
                    self.laststart = true;
                    return Ok(Tok::LParen);
                }
                ')' if bk_op => {
                    if self.parens == 0 && self.ere {
                        return Ok(self.normal(ch));
                    }
                    self.parens = self.parens.saturating_sub(1);
                    self.laststart = false;
                    return Ok(Tok::RParen);
                }
                '.' if !backslash => {
                    self.laststart = false;
                    return Ok(Tok::Any);
                }
                's' | 'S' | 'w' | 'W' if backslash => {
                    self.laststart = false;
                    return Ok(Tok::Class(format!("\\{ch}")));
                }
                '[' if !backslash => {
                    self.laststart = false;
                    let class = self.bracket()?;
                    return Ok(Tok::Class(class));
                }
                _ => return Ok(self.normal(ch)),
            }
        }
    }

    /// An interval after `{` (or `\{`): the token, or None when `{` is a
    /// literal.
    fn interval(&mut self, _brace: char) -> Result<Option<Tok>, String> {
        let backslash = !self.ere;
        let mut p = self.pos;
        let c = &self.c;
        let mut min: Option<u32> = None;
        let mut max: Option<u32> = None;
        while p < c.len() && c[p].is_ascii_digit() {
            let d = c[p] as u32 - '0' as u32;
            min = Some(min.map_or(d, |m| (m * 10 + d).min(RE_DUP_MAX + 1)));
            p += 1;
        }
        let mut comma = false;
        if p < c.len() {
            if c[p] != ',' {
                max = min;
            } else {
                comma = true;
                if min.is_none() {
                    min = Some(0);
                }
                p += 1;
                while p < c.len() && c[p].is_ascii_digit() {
                    let d = c[p] as u32 - '0' as u32;
                    max = Some(max.map_or(d, |m| (m * 10 + d).min(RE_DUP_MAX + 1)));
                    p += 1;
                }
            }
        }
        let mut closed = true;
        if backslash {
            if p < c.len() && c[p] == '\\' {
                p += 1;
            } else {
                closed = false;
            }
        }
        if closed && p < c.len() && c[p] == '}' {
            p += 1;
        } else {
            closed = false;
        }
        let order_ok = match (min, max) {
            (Some(lo), Some(hi)) => lo <= hi,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let invalid = !(closed && order_ok);
        if invalid && self.ere {
            // GNU regex rejects a well-formed interval whose bounds are out
            // of order; dfa.c alone would read it as text.
            if closed && min.is_some() && comma && !order_ok {
                return Err("Invalid content of \\{\\}".into());
            }
            return Ok(None);
        }
        if self.laststart && !self.ere {
            return Ok(None);
        }
        if invalid {
            return Err(if closed {
                "Invalid content of \\{\\}".into()
            } else {
                "Unmatched \\{".into()
            });
        }
        if max.is_some_and(|m| m > RE_DUP_MAX) || min.is_some_and(|m| m > RE_DUP_MAX) {
            return Err("Regular expression too big".into());
        }
        self.pos = p;
        self.laststart = false;
        Ok(Some(Tok::Rep(
            min.unwrap_or(0),
            if comma { max } else { min },
        )))
    }

    /// A POSIX bracket expression after `[`, as a Rust class.
    fn bracket(&mut self) -> Result<String, String> {
        const UNMATCHED: &str = "Unmatched [, [^, [:, [., or [=";
        let fetch = |lex: &mut Lexer| -> Result<char, String> {
            let ch = lex.at(0).ok_or(UNMATCHED)?;
            lex.pos += 1;
            Ok(ch)
        };
        let mut out = String::from("[");
        let mut ch = fetch(self)?;
        if ch == '^' {
            out.push('^');
            ch = fetch(self)?;
        }
        let mut colon_state = u8::from(ch == ':');
        loop {
            let mut next: Option<char> = None;
            colon_state &= !2;
            if ch == '[' {
                let c1 = fetch(self)?;
                if matches!(c1, ':' | '.' | '=') {
                    let mut name = String::new();
                    loop {
                        let c2 = fetch(self)?;
                        if c2 == c1 && self.at(0) == Some(']') {
                            break;
                        }
                        name.push(c2);
                    }
                    fetch(self)?;
                    match c1 {
                        ':' => {
                            const CLASSES: [&str; 12] = [
                                "alpha", "upper", "lower", "digit", "xdigit", "space", "print",
                                "punct", "graph", "cntrl", "blank", "alnum",
                            ];
                            if !CLASSES.contains(&name.as_str()) {
                                return Err("Invalid character class name".into());
                            }
                            out.push_str(&format!("[:{name}:]"));
                        }
                        _ => {
                            let mut cs = name.chars();
                            match (cs.next(), cs.next()) {
                                (Some(one), None) => push_class_char(&mut out, one),
                                _ => return Err("Invalid collation character".into()),
                            }
                        }
                    }
                    colon_state |= 8;
                    ch = fetch(self)?;
                    if ch == ']' {
                        break;
                    }
                    continue;
                }
                next = Some(c1);
            }
            let c1 = match next {
                Some(c1) => c1,
                None => fetch(self)?,
            };
            if c1 == '-' {
                let c2 = fetch(self)?;
                if c2 == ']' {
                    // In [x-], the - is an ordinary character.
                    self.pos -= 1;
                } else {
                    colon_state |= 8;
                    let after = fetch(self)?;
                    if ch != c2 {
                        if ch > c2 {
                            return Err("Invalid range end".into());
                        }
                        push_class_char(&mut out, ch);
                        out.push('-');
                        push_class_char(&mut out, c2);
                        ch = after;
                        if ch == ']' {
                            break;
                        }
                        continue;
                    }
                    colon_state |= if ch == ':' { 2 } else { 4 };
                    push_class_char(&mut out, ch);
                    ch = after;
                    if ch == ']' {
                        break;
                    }
                    continue;
                }
            }
            colon_state |= if ch == ':' { 2 } else { 4 };
            push_class_char(&mut out, ch);
            ch = c1;
            if ch == ']' {
                break;
            }
        }
        if colon_state == 7 {
            return Err("character class syntax is [[:space:]], not [:space:]".into());
        }
        out.push(']');
        Ok(out)
    }
}

/// A character inside a Rust class, escaped if Rust could read it as syntax
/// (but not `<` or `>`: Rust reserves `\<` and `\>`).
fn push_class_char(out: &mut String, ch: char) {
    if regex_syntax::is_meta_character(ch) {
        out.push('\\');
    }
    out.push(ch);
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Tok(Tok),
    Group(Box<Node>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
}

struct Parser {
    lex: Lexer,
    tok: Tok,
    groups: u32,
    completed: Vec<u32>,
    backrefs: bool,
}

impl Parser {
    fn advance(&mut self) -> Result<(), String> {
        self.tok = self.lex.lex()?;
        Ok(())
    }

    fn regexp(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.branch()?];
        while self.tok == Tok::Or {
            self.advance()?;
            alts.push(self.branch()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Node::Alt(alts)
        })
    }

    fn branch(&mut self) -> Result<Node, String> {
        let mut items = vec![self.closure()?];
        while !matches!(self.tok, Tok::RParen | Tok::Or | Tok::End) {
            items.push(self.closure()?);
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Node::Concat(items)
        })
    }

    fn closure(&mut self) -> Result<Node, String> {
        let mut node = self.atom()?;
        loop {
            let (min, max) = match self.tok {
                Tok::QMark => (0, Some(1)),
                Tok::Star => (0, None),
                Tok::Plus => (1, None),
                Tok::Rep(min, max) => (min, max),
                _ => return Ok(node),
            };
            node = Node::Repeat(Box::new(node), min, max);
            self.advance()?;
        }
    }

    fn atom(&mut self) -> Result<Node, String> {
        match self.tok.clone() {
            Tok::Backref(n) => {
                if !self.completed.contains(&n) {
                    return Err("Invalid back reference".into());
                }
                self.backrefs = true;
                self.advance()?;
                Ok(Node::Tok(Tok::Backref(n)))
            }
            tok @ (Tok::Char(_)
            | Tok::Any
            | Tok::Class(_)
            | Tok::BegLine
            | Tok::EndLine
            | Tok::BegWord
            | Tok::EndWord
            | Tok::LimWord
            | Tok::NotLimWord) => {
                self.advance()?;
                Ok(Node::Tok(tok))
            }
            Tok::LParen => {
                self.groups += 1;
                let number = self.groups;
                self.advance()?;
                let inner = self.regexp()?;
                if self.tok != Tok::RParen {
                    return Err("Unmatched ( or \\(".into());
                }
                self.completed.push(number);
                self.advance()?;
                Ok(Node::Group(Box::new(inner)))
            }
            _ => Ok(Node::Empty),
        }
    }
}

fn emit(node: &Node, offset: u32, out: &mut String) {
    match node {
        Node::Empty => {}
        Node::Tok(tok) => emit_tok(tok, offset, out),
        Node::Group(inner) => {
            out.push('(');
            emit(inner, offset, out);
            out.push(')');
        }
        Node::Concat(items) => {
            for item in items {
                emit(item, offset, out);
            }
        }
        Node::Alt(alts) => {
            for (k, alt) in alts.iter().enumerate() {
                if k > 0 {
                    out.push('|');
                }
                emit(alt, offset, out);
            }
        }
        Node::Repeat(inner, min, max) => {
            let simple = matches!(
                **inner,
                Node::Group(_)
                    | Node::Tok(Tok::Char(_) | Tok::Any | Tok::Class(_) | Tok::Backref(_))
            );
            if simple {
                emit(inner, offset, out);
            } else {
                out.push_str("(?:");
                emit(inner, offset, out);
                out.push(')');
            }
            match (min, max) {
                (0, None) => out.push('*'),
                (1, None) => out.push('+'),
                (0, Some(1)) => out.push('?'),
                (m, None) => out.push_str(&format!("{{{m},}}")),
                (m, Some(x)) if m == x => out.push_str(&format!("{{{m}}}")),
                (m, Some(x)) => out.push_str(&format!("{{{m},{x}}}")),
            }
        }
    }
}

fn emit_tok(tok: &Tok, offset: u32, out: &mut String) {
    match tok {
        Tok::Char(ch) => {
            if regex_syntax::is_meta_character(*ch) {
                out.push('\\');
            }
            out.push(*ch);
        }
        Tok::Any => out.push('.'),
        Tok::Class(class) => out.push_str(class),
        Tok::BegLine => out.push('^'),
        Tok::EndLine => out.push('$'),
        Tok::BegWord => out.push_str("\\b{start}"),
        Tok::EndWord => out.push_str("\\b{end}"),
        Tok::LimWord => out.push_str("\\b"),
        Tok::NotLimWord => out.push_str("\\B"),
        Tok::Backref(n) => out.push_str(&format!("\\{}", n + offset)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bre(p: &str) -> String {
        gnu_to_rust(p, false, 0).unwrap().rust
    }

    fn ere(p: &str) -> String {
        gnu_to_rust(p, true, 0).unwrap().rust
    }

    #[test]
    fn basic_operators_are_escaped() {
        assert_eq!(bre(r"a\|b"), "a|b");
        assert_eq!(bre(r"\(ab\)*"), "(ab)*");
        assert_eq!(bre(r"a\{2,3\}"), "a{2,3}");
        assert_eq!(bre(r"a\+b\?"), "a+b?");
        assert_eq!(bre("a|b(c)+?{1}"), r"a\|b\(c\)\+\?\{1\}");
    }

    #[test]
    fn basic_anchors_and_stars_depend_on_context() {
        assert_eq!(bre("^a^b$c$"), r"^a\^b\$c$");
        assert_eq!(bre(r"\(^a$\)"), "(^a$)");
        assert_eq!(bre("*a"), r"\*a");
        assert_eq!(bre("^*a"), r"^\*a");
        assert_eq!(bre(r"x\|*y"), r"x|\*y");
        assert_eq!(bre(r"\{1\}"), r"\{1\}");
        assert_eq!(bre("a**"), "(?:a*)*");
    }

    #[test]
    fn extended_operators() {
        assert_eq!(ere("(a|b)+c?"), "(a|b)+c?");
        assert_eq!(ere(r"a\|b\(c\)"), r"a\|b\(c\)");
        assert_eq!(ere("a+?"), "(?:a+)?");
        assert_eq!(ere("*foo"), "(?:)*foo");
        assert_eq!(ere("a|*b"), "a|(?:)*b");
        assert_eq!(ere("a{"), r"a\{");
        assert_eq!(ere("a{x}"), r"a\{x\}");
        assert_eq!(ere("a{,3}"), "a{0,3}");
        assert_eq!(ere("a)"), r"a\)");
        assert!(gnu_to_rust("x{2,1}", true, 0).is_err());
        assert!(gnu_to_rust("(a", true, 0).is_err());
    }

    #[test]
    fn gnu_escapes() {
        assert_eq!(bre(r"\<foo\>"), r"\b{start}foo\b{end}");
        assert_eq!(bre(r"\bx\B"), r"\bx\B");
        assert_eq!(ere(r"\w+\s\S\W"), r"\w+\s\S\W");
        // Unknown escapes are the characters themselves.
        assert_eq!(ere(r"\d\t\."), r"dt\.");
        assert!(gnu_to_rust("a\\", false, 0).is_err());
    }

    #[test]
    fn brackets_are_posix() {
        assert_eq!(bre(r"[\n]"), r"[\\n]");
        assert_eq!(bre("[]a]"), r"[\]a]");
        assert_eq!(bre("[^]a-z]"), r"[^\]a-z]");
        assert_eq!(bre("[a-]"), r"[a\-]");
        assert_eq!(bre("[[:alpha:]_]"), "[[:alpha:]_]");
        assert_eq!(ere("[^<>]+"), "[^<>]+");
        assert_eq!(bre("[[.a.][=b=]]"), "[ab]");
        assert!(gnu_to_rust("[:space:]", false, 0).is_err());
        assert!(gnu_to_rust("[[:foo:]]", false, 0).is_err());
        assert!(gnu_to_rust("[z-a]", false, 0).is_err());
        assert!(gnu_to_rust("[abc", false, 0).is_err());
    }

    #[test]
    fn backreferences() {
        let c = gnu_to_rust(r"\(a\)\1", false, 0).unwrap();
        assert_eq!(c.rust, r"(a)\1");
        assert!(c.backrefs);
        assert_eq!(c.groups, 1);
        assert_eq!(gnu_to_rust("(a)\\1", true, 2).unwrap().rust, r"(a)\3");
        assert!(gnu_to_rust(r"\1\(a\)", false, 0).is_err());
    }

    #[test]
    fn converted_patterns_compile_in_rust() {
        for (p, extended) in [
            (r"^foo\(bar\)*$", false),
            ("a|*b", true),
            ("^*", true),
            ("(|a)", true),
            ("()", true),
            (r"x\{0,3\}y", false),
            ("[[:digit:]]+[.]", true),
            (r"\<[A-Z_]\+\>", false),
        ] {
            let rust = gnu_to_rust(p, extended, 0).unwrap().rust;
            regex::Regex::new(&rust).unwrap_or_else(|e| panic!("{p} -> {rust}: {e}"));
        }
    }
}
