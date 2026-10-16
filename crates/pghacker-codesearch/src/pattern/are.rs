// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Rust (ripgrep) and PCRE regexes as PostgreSQL advanced regular expressions
//! (AREs), the filter that lets pg_trgm prune candidate lines.
//!
//! Constructs an ARE cannot express are widened to something that matches at
//! least the same lines (`\p{L}` becomes `.`, a scoped `(?i:...)` makes the
//! whole ARE case-insensitive); the result is then marked inexact and the
//! exact Rust matcher decides.

/// A pattern as an ARE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Are {
    pub re: String,
    /// Match case-insensitively: the pattern starts with `(?i)`, or turns
    /// case-insensitivity on somewhere an ARE cannot scope it.
    pub ci: bool,
    /// Whether the ARE matches exactly the lines the pattern matches.
    pub exact: bool,
}

/// Lookarounds for a whole word, as `-w` and `\b{start-half}` mean it.
pub const WORD_PRE: &str = "(?<![[:alnum:]_])";
pub const WORD_POST: &str = "(?![[:alnum:]_])";

/// `s` with ARE metacharacters escaped.
pub fn escape_are(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `s` with LIKE's wildcards and escape character escaped.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn hex_escape(code: u32) -> String {
    format!("\\U{code:08X}")
}

/// A hexadecimal escape after `\x`, `\u` or `\U` at `at`: `{hex}` or a
/// fixed number of digits. Returns the code point and the characters used.
fn hex_digits(c: &[char], at: usize, fixed: usize) -> Option<(u32, usize)> {
    if c.get(at) == Some(&'{') {
        let end = (at + 1..c.len()).find(|&j| c[j] == '}')?;
        let hex: String = c[at + 1..end].iter().collect();
        let code = u32::from_str_radix(hex.trim(), 16).ok()?;
        return Some((code, end + 1 - at));
    }
    if at + fixed > c.len() {
        return None;
    }
    let hex: String = c[at..at + fixed].iter().collect();
    if !hex.chars().all(|d| d.is_ascii_hexdigit()) {
        return None;
    }
    Some((u32::from_str_radix(&hex, 16).ok()?, fixed))
}

struct Out {
    re: String,
    exact: bool,
    force_ci: bool,
}

impl Out {
    fn widen(&mut self, with: &str) {
        self.re.push_str(with);
        self.exact = false;
    }
}

/// A Rust regex (or a PCRE pattern, with `pcre`) as an ARE. The pattern must
/// already compile in Rust; an error means no ARE can stand in for it.
pub fn rust_to_are(p: &str, pcre: bool) -> Result<Are, String> {
    let c: Vec<char> = p.chars().collect();
    let n = c.len();
    let mut out = Out {
        re: String::with_capacity(p.len() + 8),
        exact: true,
        force_ci: false,
    };
    let mut ci = false;
    let mut i = 0;
    while i < n {
        let ch = c[i];
        if ch == '\\' {
            let Some(&d) = c.get(i + 1) else {
                return Err("trailing backslash".into());
            };
            i += 2;
            match d {
                'b' if c.get(i) == Some(&'{') => {
                    let end = (i..n).find(|&j| c[j] == '}').ok_or("unterminated \\b{")?;
                    let kind: String = c[i + 1..end].iter().collect();
                    i = end + 1;
                    out.re.push_str(match kind.as_str() {
                        "start" => "\\m",
                        "end" => "\\M",
                        "start-half" => WORD_PRE,
                        "end-half" => WORD_POST,
                        _ => return Err(format!("unsupported \\b{{{kind}}}")),
                    });
                }
                // \b is a backspace and \B a backslash in an ARE.
                'b' => out.re.push_str("\\y"),
                'B' => out.re.push_str("\\Y"),
                '<' => out.re.push_str("\\m"),
                '>' => out.re.push_str("\\M"),
                'A' => out.re.push_str("\\A"),
                'z' | 'Z' => out.re.push_str("\\Z"),
                'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'n' | 't' | 'r' | 'f' | 'v' | 'a' => {
                    out.re.push('\\');
                    out.re.push(d);
                }
                'e' if pcre => out.re.push_str("\\e"),
                'x' | 'u' | 'U' => {
                    let fixed = match d {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let (code, used) =
                        hex_digits(&c, i, fixed).ok_or_else(|| format!("bad \\{d} escape"))?;
                    out.re.push_str(&hex_escape(code));
                    i += used;
                }
                'p' | 'P' => {
                    if c.get(i) == Some(&'{') {
                        i = (i..n).find(|&j| c[j] == '}').map_or(n, |j| j + 1);
                    } else {
                        i += 1;
                    }
                    out.widen(".");
                }
                'Q' => {
                    let rest: String = c[i..].iter().collect();
                    match rest.find("\\E") {
                        Some(e) => {
                            out.re.push_str(&escape_are(&rest[..e]));
                            i += rest[..e].chars().count() + 2;
                        }
                        None => {
                            out.re.push_str(&escape_are(&rest));
                            i = n;
                        }
                    }
                }
                'E' => {}
                '1'..='9' => {
                    // A backreference; keep a following digit out of its number.
                    out.re.push('\\');
                    out.re.push(d);
                    if c.get(i).is_some_and(char::is_ascii_digit) {
                        out.re.push_str("(?:)");
                    }
                }
                // \K only moves the reported start of the match.
                'K' if pcre => {}
                'G' if pcre => out.exact = false,
                'N' if pcre => out.re.push('.'),
                'h' | 'H' | 'R' | 'C' | 'V' if pcre => out.widen("."),
                'X' if pcre => out.widen(".+"),
                'g' | 'k' if pcre => {
                    // A named or relative backreference or a subroutine call.
                    match c.get(i) {
                        Some('{') => i = (i..n).find(|&j| c[j] == '}').map_or(n, |j| j + 1),
                        Some('<') => i = (i..n).find(|&j| c[j] == '>').map_or(n, |j| j + 1),
                        Some('\'') => i = (i + 1..n).find(|&j| c[j] == '\'').map_or(n, |j| j + 1),
                        _ => {
                            if c.get(i) == Some(&'-') || c.get(i) == Some(&'+') {
                                i += 1;
                            }
                            while c.get(i).is_some_and(char::is_ascii_digit) {
                                i += 1;
                            }
                        }
                    }
                    out.widen(".*");
                }
                d if d.is_ascii_alphanumeric() => return Err(format!("unsupported escape \\{d}")),
                // Escaped punctuation is a literal in both.
                d => {
                    out.re.push('\\');
                    out.re.push(d);
                }
            }
            continue;
        }
        if ch == '[' {
            let (class, end, exact) = convert_class(&c, i);
            out.re.push_str(&class);
            out.exact &= exact;
            i = end;
            continue;
        }
        if ch == '(' && c.get(i + 1) == Some(&'?') {
            let rest = &c[i + 2..];
            let starts = |s: &str| {
                let s: Vec<char> = s.chars().collect();
                rest.len() >= s.len() && rest[..s.len()] == s[..]
            };
            if starts(":") {
                out.re.push_str("(?:");
                i += 3;
            } else if starts("=") || starts("!") {
                out.re.push_str("(?");
                out.re.push(rest[0]);
                i += 3;
            } else if starts("<=") || starts("<!") {
                out.re.push_str("(?<");
                out.re.push(rest[1]);
                i += 4;
            } else if let Some(len) = named_group(rest) {
                out.re.push('(');
                i += 2 + len;
            } else if starts("#") {
                i = (i..n).find(|&j| c[j] == ')').map_or(n, |j| j + 1);
            } else if starts(">") || starts("|") {
                // Atomic groups and branch resets only constrain the match.
                out.widen("(?:");
                i += 3;
            } else if let Some((on, off, close, len)) = flag_group(rest) {
                if on.contains('x') || off.contains('x') {
                    return Err("verbose mode (?x) has no ARE equivalent".into());
                }
                if let Some(bad) = on
                    .chars()
                    .chain(off.chars())
                    .find(|f| !"imsuUR".contains(*f))
                {
                    return Err(format!("unsupported flag {bad}"));
                }
                i += 2 + len;
                if on.contains('i') {
                    if close == ')' && out.re.is_empty() {
                        ci = true;
                    } else {
                        out.force_ci = true;
                        out.exact = false;
                    }
                }
                // Turning case-insensitivity or Unicode off, or CRLF mode
                // on, can only make the pattern match fewer lines.
                if off.contains('i') || off.contains('u') || on.contains('R') {
                    out.exact = false;
                }
                if close == ':' {
                    out.re.push_str("(?:");
                }
            } else {
                return Err("unsupported group syntax".into());
            }
            continue;
        }
        if ch == '{' {
            if let Some((lo, hi, comma, len)) = counted(&c[i..]) {
                i += len;
                let lo = lo.unwrap_or(0);
                if lo > 255 || hi.is_some_and(|h| h > 255) {
                    // PostgreSQL allows counts up to 255; fewer is a superset.
                    out.widen(&format!("{{{},}}", lo.min(255)));
                } else {
                    match (comma, hi) {
                        (false, _) => out.re.push_str(&format!("{{{lo}}}")),
                        (true, None) => out.re.push_str(&format!("{{{lo},}}")),
                        (true, Some(h)) => out.re.push_str(&format!("{{{lo},{h}}}")),
                    }
                }
                if pcre && c.get(i) == Some(&'+') {
                    out.exact = false;
                    i += 1;
                }
                continue;
            }
            out.re.push_str("\\{");
            i += 1;
            continue;
        }
        if ch == '}' {
            out.re.push_str("\\}");
            i += 1;
            continue;
        }
        if matches!(ch, '*' | '+' | '?') && pcre && c.get(i + 1) == Some(&'+') {
            out.re.push(ch);
            out.exact = false;
            i += 2;
            continue;
        }
        out.re.push(ch);
        i += 1;
    }
    Ok(Are {
        re: out.re,
        ci: ci || out.force_ci,
        exact: out.exact,
    })
}

/// `P<name>`, `<name>` or `'name'` after `(?`: the characters it takes.
fn named_group(rest: &[char]) -> Option<usize> {
    let mut j = usize::from(rest.first() == Some(&'P'));
    let close = match rest.get(j)? {
        '<' => '>',
        '\'' => '\'',
        _ => return None,
    };
    j += 1;
    let first = *rest.get(j)?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    while rest
        .get(j)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        j += 1;
    }
    (rest.get(j) == Some(&close)).then_some(j + 1)
}

/// `flags)`, `flags:`, `flags-flags)` or `flags-flags:` after `(?`.
fn flag_group(rest: &[char]) -> Option<(String, String, char, usize)> {
    let mut j = 0;
    let mut on = String::new();
    let mut off = String::new();
    while rest.get(j).is_some_and(char::is_ascii_alphabetic) {
        on.push(rest[j]);
        j += 1;
    }
    if rest.get(j) == Some(&'-') {
        j += 1;
        while rest.get(j).is_some_and(char::is_ascii_alphabetic) {
            off.push(rest[j]);
            j += 1;
        }
    }
    match rest.get(j) {
        Some(&close @ (')' | ':')) => Some((on, off, close, j + 1)),
        _ => None,
    }
}

/// A counted repetition `{m}`, `{m,}`, `{m,n}` or `{,n}`: (m, n, comma, length).
fn counted(c: &[char]) -> Option<(Option<u32>, Option<u32>, bool, usize)> {
    let mut j = 1;
    let digits = |j: &mut usize| {
        let start = *j;
        while c.get(*j).is_some_and(char::is_ascii_digit) {
            *j += 1;
        }
        (start < *j).then(|| c[start..*j].iter().collect::<String>().parse::<u32>().ok())?
    };
    let lo = digits(&mut j);
    let comma = c.get(j) == Some(&',');
    if comma {
        j += 1;
    }
    let hi = if comma { digits(&mut j) } else { lo };
    if c.get(j) != Some(&'}') || (lo.is_none() && hi.is_none()) {
        return None;
    }
    Some((lo, hi, comma, j + 1))
}

/// A Rust bracket expression at `c[start] == '['` as an ARE bracket
/// expression: the text, the index after it, and whether it is exact. A
/// class an ARE cannot express becomes `.`, which matches every character.
fn convert_class(c: &[char], start: usize) -> (String, usize, bool) {
    let n = c.len();
    let mut i = start + 1;
    let mut out = String::from("[");
    let mut ok = true;
    if c.get(i) == Some(&'^') {
        out.push('^');
        i += 1;
    }
    let mut first = true;
    let mut depth = 0usize;
    while i < n {
        let ch = c[i];
        if ch == ']' && !first {
            if depth == 0 {
                i += 1;
                return if ok {
                    (out + "]", i, true)
                } else {
                    (".".into(), i, false)
                };
            }
            depth -= 1;
            i += 1;
            continue;
        }
        first = false;
        if ch == '[' {
            if let Some((negated, name, len)) = posix_class(&c[i..]) {
                if negated {
                    ok = false;
                } else {
                    out.push_str(&format!("[:{name}:]"));
                }
                i += len;
                continue;
            }
            // A nested class (Rust syntax): skip it, with its own `^` and `]`.
            ok = false;
            depth += 1;
            i += 1;
            if c.get(i) == Some(&'^') {
                i += 1;
            }
            if c.get(i) == Some(&']') {
                i += 1;
            }
            continue;
        }
        if (ch == '&' || ch == '-' || ch == '~') && c.get(i + 1) == Some(&ch) {
            ok = false;
            i += 2;
            continue;
        }
        if ch == '\\' {
            let d = c.get(i + 1).copied().unwrap_or('\\');
            i += 2;
            match d {
                'd' | 's' | 'w' => {
                    out.push('\\');
                    out.push(d);
                }
                'D' | 'S' | 'W' => ok = false,
                'p' | 'P' => {
                    ok = false;
                    if c.get(i) == Some(&'{') {
                        i = (i..n).find(|&j| c[j] == '}').map_or(n, |j| j + 1);
                    } else {
                        i += 1;
                    }
                }
                'x' | 'u' | 'U' => {
                    let fixed = match d {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    match hex_digits(c, i, fixed) {
                        Some((code, used)) => {
                            out.push_str(&hex_escape(code));
                            i += used;
                        }
                        None => ok = false,
                    }
                }
                'n' | 't' | 'r' | 'f' | 'v' | 'a' => {
                    out.push('\\');
                    out.push(d);
                }
                d if d.is_ascii_alphanumeric() => ok = false,
                // \] \[ \- \\ \^ and other punctuation are literals in an ARE too.
                d => {
                    out.push('\\');
                    out.push(d);
                }
            }
            continue;
        }
        if depth == 0 {
            out.push(ch);
        }
        i += 1;
    }
    (".".into(), n, false)
}

/// `[:name:]` or `[:^name:]` at the start of `c`: (negated, name, length).
fn posix_class(c: &[char]) -> Option<(bool, String, usize)> {
    if c.get(1) != Some(&':') {
        return None;
    }
    let mut j = 2;
    let negated = c.get(j) == Some(&'^');
    if negated {
        j += 1;
    }
    let start = j;
    while c.get(j).is_some_and(char::is_ascii_lowercase) {
        j += 1;
    }
    if j == start || c.get(j) != Some(&':') || c.get(j + 1) != Some(&']') {
        return None;
    }
    Some((negated, c[start..j].iter().collect(), j + 2))
}

/// An ARE split at its top-level `|`, outside groups and bracket expressions.
pub fn split_top_alts(re: &str) -> Vec<String> {
    let c: Vec<char> = re.chars().collect();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' {
            cur.push(ch);
            if let Some(&d) = c.get(i + 1) {
                cur.push(d);
            }
            i += 2;
            continue;
        }
        if ch == '[' {
            let mut j = i + 1;
            if c.get(j) == Some(&'^') {
                j += 1;
            }
            if c.get(j) == Some(&']') {
                j += 1;
            }
            while j < c.len() && c[j] != ']' {
                if c[j] == '\\' {
                    j += 1;
                } else if c[j] == '['
                    && c.get(j + 1) == Some(&':')
                    && let Some(e) =
                        (j + 2..c.len().saturating_sub(1)).find(|&k| c[k] == ':' && c[k + 1] == ']')
                {
                    j = e + 1;
                }
                j += 1;
            }
            let end = (j + 1).min(c.len());
            cur.extend(&c[i..end]);
            i = end;
            continue;
        }
        if ch == '(' {
            depth += 1;
        }
        if ch == ')' {
            depth = depth.saturating_sub(1);
        }
        if ch == '|' && depth == 0 {
            parts.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(ch);
        i += 1;
    }
    parts.push(cur);
    parts
}

/// The longest run of literal text that every match of an ARE alternative
/// (one without a top-level `|`) contains, if any.
pub fn required_literal(alt: &str) -> Option<String> {
    let c: Vec<char> = alt.chars().collect();
    let mut best = String::new();
    let mut run = String::new();
    let mut i = 0;
    let finish = |run: &mut String, best: &mut String| {
        if run.chars().count() > best.chars().count() {
            *best = run.clone();
        }
        run.clear();
    };
    while i < c.len() {
        // An atom: a literal character or something else.
        let (atom, next): (Option<char>, usize) = match c[i] {
            '\\' => match c.get(i + 1) {
                Some(&d) if !d.is_ascii_alphanumeric() => (Some(d), i + 2),
                Some('U') => (None, (i + 10).min(c.len())),
                Some(_) => (None, i + 2),
                None => (None, i + 1),
            },
            '[' => {
                let mut j = i + 1;
                if c.get(j) == Some(&'^') {
                    j += 1;
                }
                if c.get(j) == Some(&']') {
                    j += 1;
                }
                while j < c.len() && c[j] != ']' {
                    if c[j] == '\\' {
                        j += 1;
                    } else if c[j] == '['
                        && c.get(j + 1) == Some(&':')
                        && let Some(e) = (j + 2..c.len().saturating_sub(1))
                            .find(|&k| c[k] == ':' && c[k + 1] == ']')
                    {
                        j = e + 1;
                    }
                    j += 1;
                }
                (None, j + 1)
            }
            '(' => {
                let mut depth = 0;
                let mut j = i;
                while j < c.len() {
                    match c[j] {
                        '\\' => j += 1,
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                (None, j + 1)
            }
            '.' | '^' | '$' | '|' | ')' => (None, i + 1),
            ch => (Some(ch), i + 1),
        };
        // What follows: a quantifier that makes the atom optional or repeats it.
        let quant = c.get(next).copied();
        let optional = match quant {
            Some('*' | '?') => true,
            Some('{') => c.get(next + 1) == Some(&'0') || c.get(next + 1) == Some(&','),
            _ => false,
        };
        let repeated = matches!(quant, Some('+' | '{'));
        match atom {
            Some(ch) if !optional => {
                run.push(ch);
                if repeated {
                    finish(&mut run, &mut best);
                }
            }
            _ => finish(&mut run, &mut best),
        }
        i = next;
        // Skip the quantifier itself (and a lazy `?`).
        if matches!(quant, Some('*' | '+' | '?')) {
            i += 1;
        } else if quant == Some('{') {
            i = (i..c.len())
                .find(|&k| c[k] == '}')
                .map_or(c.len(), |k| k + 1);
        }
        if c.get(i) == Some(&'?') && quant.is_some_and(|q| "*+?{".contains(q)) {
            i += 1;
        }
    }
    finish(&mut run, &mut best);
    (!best.is_empty()).then_some(best)
}

/// Plain text with optional `^` and `$` anchors, so that the alternative can
/// be a LIKE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Literal {
    pub text: String,
    pub start: bool,
    pub end: bool,
}

/// The ARE alternative as plain text, if it is plain text.
pub fn literal_of(alt: &str) -> Option<Literal> {
    let mut s = alt;
    let mut start = false;
    let mut end = false;
    if let Some(rest) = s.strip_prefix('^') {
        start = true;
        s = rest;
    }
    if s.ends_with('$') && !s.ends_with("\\$") {
        end = true;
        s = &s[..s.len() - 1];
    }
    let mut text = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let d = chars.next()?;
            if d.is_ascii_alphanumeric() {
                return None;
            }
            text.push(d);
            continue;
        }
        if ".[]()*+?{}|^$".contains(ch) {
            return None;
        }
        text.push(ch);
    }
    Some(Literal { text, start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn are(p: &str) -> Are {
        rust_to_are(p, false).unwrap()
    }

    #[test]
    fn word_boundaries_become_are_escapes() {
        assert_eq!(are(r"\bfoo\b").re, r"\yfoo\y");
        assert_eq!(are(r"\Bx").re, r"\Yx");
        assert_eq!(are(r"\b{start}a\b{end}").re, r"\ma\M");
        assert_eq!(
            are(r"\b{start-half}a\b{end-half}").re,
            format!("{WORD_PRE}a{WORD_POST}")
        );
    }

    #[test]
    fn escapes_and_classes_carry_over() {
        assert_eq!(are(r"\d+\s\w").re, r"\d+\s\w");
        assert_eq!(are(r"\x41\x{263A}").re, r"\U00000041\U0000263A");
        assert_eq!(are(r"[a-z_\d]").re, r"[a-z_\d]");
        assert_eq!(are(r"[[:alpha:]]x").re, "[[:alpha:]]x");
        assert_eq!(are(r"[\]\-]").re, r"[\]\-]");
        assert_eq!(are(r"\.\*").re, r"\.\*");
        assert!(are("a").exact);
    }

    #[test]
    fn counts_and_braces() {
        assert_eq!(are("a{2,3}").re, "a{2,3}");
        assert_eq!(are("a{,3}").re, "a{0,3}");
        assert_eq!(are("a{2}").re, "a{2}");
        let big = are("a{300}");
        assert_eq!(big.re, "a{255,}");
        assert!(!big.exact);
        assert_eq!(rust_to_are("a{", true).unwrap().re, r"a\{");
    }

    #[test]
    fn groups_flags_and_lookarounds() {
        assert_eq!(are("(?:a|b)c").re, "(?:a|b)c");
        assert_eq!(are("(?P<x>a)").re, "(a)");
        let ci = are("(?i)abc");
        assert!(ci.ci && ci.exact);
        assert_eq!(ci.re, "abc");
        let scoped = are("a(?i:b)");
        assert!(scoped.ci && !scoped.exact);
        assert_eq!(scoped.re, "a(?:b)");
        let p = rust_to_are(r"(?<=a)b(?!c)", true).unwrap();
        assert_eq!(p.re, "(?<=a)b(?!c)");
        assert!(rust_to_are("(?x)a b", false).is_err());
    }

    #[test]
    fn unsupported_constructs_widen() {
        let u = are(r"\p{Greek}x");
        assert_eq!(u.re, ".x");
        assert!(!u.exact);
        let neg = are("[[:^digit:]]a");
        assert_eq!(neg.re, ".a");
        assert!(!neg.exact);
        let nested = are("[a[bc]]d");
        assert_eq!(nested.re, ".d");
        let inter = are("[a-z&&[^aeiou]]q");
        assert_eq!(inter.re, ".q");
        let poss = rust_to_are("a++b", true).unwrap();
        assert_eq!(poss.re, "a+b");
        assert!(!poss.exact);
        let atomic = rust_to_are("(?>ab)c", true).unwrap();
        assert_eq!(atomic.re, "(?:ab)c");
        let k = rust_to_are(r"foo\Kbar", true).unwrap();
        assert_eq!(k.re, "foobar");
        assert!(k.exact);
    }

    #[test]
    fn quoting() {
        assert_eq!(rust_to_are(r"\Qa.b\Ec", true).unwrap().re, r"a\.bc");
        assert_eq!(escape_are("a.b(c)"), r"a\.b\(c\)");
        assert_eq!(escape_like(r"50%_a\b"), r"50\%\_a\\b");
    }

    #[test]
    fn splits_top_level_alternatives() {
        assert_eq!(split_top_alts("a|b(c|d)|[|]e"), vec!["a", "b(c|d)", "[|]e"]);
        assert_eq!(split_top_alts(r"x\|y|z"), vec![r"x\|y", "z"]);
        assert_eq!(split_top_alts("[[:alpha:]|]|q"), vec!["[[:alpha:]|]", "q"]);
        assert_eq!(split_top_alts(""), vec![""]);
    }

    #[test]
    fn required_literals() {
        assert_eq!(
            required_literal("T_[A-Za-z]{3,8}Stmt").as_deref(),
            Some("Stmt")
        );
        assert_eq!(required_literal(r"make\w+Node\(").as_deref(), Some("Node("));
        assert_eq!(required_literal("abc?d").as_deref(), Some("ab"));
        assert_eq!(required_literal("(x|y)zz+q").as_deref(), Some("zz"));
        assert_eq!(required_literal(r"\yfoo_bar\y").as_deref(), Some("foo_bar"));
        assert_eq!(required_literal(r"\d{4,}"), None);
        assert_eq!(required_literal("ab{0,2}cd").as_deref(), Some("cd"));
    }

    #[test]
    fn literals_of_alternatives() {
        assert_eq!(
            literal_of(r"^foo\.bar$"),
            Some(Literal {
                text: "foo.bar".into(),
                start: true,
                end: true
            })
        );
        assert_eq!(literal_of("a.b"), None);
        assert_eq!(literal_of(r"a\yb"), None);
        assert_eq!(
            literal_of(r"cost\$"),
            Some(Literal {
                text: "cost$".into(),
                start: false,
                end: false
            })
        );
    }
}
