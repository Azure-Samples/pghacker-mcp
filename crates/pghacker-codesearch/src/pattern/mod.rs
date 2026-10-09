//! Search patterns: grep's and ripgrep's dialects, translated twice, into
//! conditions on `code_lines.line` that pg_trgm's index can use, and into an
//! exact Rust matcher that decides which candidate lines match.

pub mod are;
pub mod gnu;
pub mod matcher;

use std::sync::LazyLock;

use regex::Regex;

pub use matcher::{LineMatcher, Semantics};

use are::{Literal, WORD_POST, WORD_PRE, escape_are, escape_like, literal_of, split_top_alts};

/// How a pattern is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// Plain text (`grep -F`).
    Fixed,
    /// grep's basic regex (`grep`, `grep -G`).
    Basic,
    /// grep's extended regex (`grep -E`).
    Extended,
    /// Perl-compatible (`grep -P`).
    Perl,
    /// ripgrep's (Rust's) syntax.
    Rust,
}

/// A PostgreSQL operator on the line text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Like,
    ILike,
    Re,
    ReI,
}

impl Op {
    pub fn sql(self) -> &'static str {
        match self {
            Op::Like => "LIKE",
            Op::ILike => "ILIKE",
            Op::Re => "~",
            Op::ReI => "~*",
        }
    }

    fn of(ci: bool, like: bool) -> Op {
        match (like, ci) {
            (true, false) => Op::Like,
            (true, true) => Op::ILike,
            (false, false) => Op::Re,
            (false, true) => Op::ReI,
        }
    }
}

/// `line <op> <pattern>`, and for a regex, `line LIKE <literal>` (or ILIKE)
/// on text every match contains, which gives pg_trgm trigrams where the
/// regex alone gives too few.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cond {
    pub op: Op,
    pub pattern: String,
    pub literal: Option<String>,
}

/// What PostgreSQL can check of a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Lines that match any of the conditions: exactly the lines the pattern
    /// matches when `exact`, otherwise a superset of them.
    Any { conds: Vec<Cond>, exact: bool },
    /// Any line may match; with `exact`, every line does.
    All { exact: bool },
}

impl Filter {
    /// Whether the filter matches exactly the pattern's lines.
    pub fn is_exact(&self) -> bool {
        match self {
            Filter::Any { exact, .. } | Filter::All { exact } => *exact,
        }
    }
}

/// A pattern list and how to read it.
#[derive(Clone, Debug)]
pub struct PatternSpec<'a> {
    /// Patterns; a line matches if any does. Newlines separate patterns.
    pub patterns: &'a [String],
    pub dialect: Dialect,
    pub ignore_case: bool,
    /// Matches must be whole words (`-w`).
    pub word: bool,
    /// Matches must be whole lines (`-x`); takes precedence over `word`.
    pub line: bool,
    pub semantics: Semantics,
}

/// A compiled pattern: the SQL filter and the exact matcher.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub matcher: LineMatcher,
    pub filter: Filter,
}

/// Compiles a pattern list for PostgreSQL and for Rust.
pub fn compile(spec: &PatternSpec<'_>) -> Result<Compiled, String> {
    let pats: Vec<&str> = spec.patterns.iter().flat_map(|p| p.split('\n')).collect();
    if pats.is_empty() {
        return Err("no pattern".into());
    }
    let mut rust = Vec::with_capacity(pats.len());
    let mut backrefs = false;
    let mut offset = 0;
    for p in &pats {
        match spec.dialect {
            Dialect::Fixed => rust.push(regex::escape(p)),
            Dialect::Basic | Dialect::Extended => {
                let c = gnu::gnu_to_rust(p, spec.dialect == Dialect::Extended, offset)?;
                offset += c.groups;
                backrefs |= c.backrefs;
                rust.push(c.rust);
            }
            Dialect::Perl | Dialect::Rust => rust.push((*p).to_owned()),
        }
    }
    let joined = if rust.len() == 1 {
        rust[0].clone()
    } else {
        rust.iter()
            .map(|r| format!("(?:{r})"))
            .collect::<Vec<_>>()
            .join("|")
    };
    let wrapped = if spec.line {
        format!("^(?:{joined})$")
    } else if spec.word {
        format!("\\b{{start-half}}(?:{joined})\\b{{end-half}}")
    } else {
        joined
    };
    let full = if spec.ignore_case {
        format!("(?i){wrapped}")
    } else {
        wrapped
    };
    let matcher = LineMatcher::new(&full, spec.semantics, backrefs)?;
    let filter = sql_filter(spec, &pats, &rust);
    Ok(Compiled { matcher, filter })
}

/// The SQL conditions for a pattern list: plain-text alternatives become
/// LIKE, the rest ARE regexes.
fn sql_filter(spec: &PatternSpec<'_>, pats: &[&str], rust: &[String]) -> Filter {
    let mut exact = true;
    let mut ci = spec.ignore_case;
    let mut alts: Vec<(String, Option<Literal>)> = Vec::new();
    if spec.dialect == Dialect::Fixed {
        for p in pats {
            let lit = Literal {
                text: (*p).to_owned(),
                start: false,
                end: false,
            };
            alts.push((escape_are(p), Some(lit)));
        }
    } else {
        let mut any_ci = false;
        let mut all_ci = true;
        for r in rust {
            let Ok(a) = are::rust_to_are(r, spec.dialect == Dialect::Perl) else {
                return Filter::All { exact: false };
            };
            exact &= a.exact;
            any_ci |= a.ci;
            all_ci &= a.ci;
            for alt in split_top_alts(&a.re) {
                let lit = literal_of(&alt);
                alts.push((alt, lit));
            }
        }
        if any_ci && !ci {
            ci = true;
            exact &= all_ci;
        }
    }
    let mut conds: Vec<Cond> = Vec::new();
    for (alt, lit) in alts {
        let cond = if spec.line {
            match lit {
                Some(l) => Cond {
                    op: Op::of(ci, true),
                    pattern: escape_like(&l.text),
                    literal: None,
                },
                None => Cond {
                    op: Op::of(ci, false),
                    pattern: format!("^(?:{alt})$"),
                    literal: required_like(&alt),
                },
            }
        } else if spec.word {
            Cond {
                op: Op::of(ci, false),
                literal: required_like(&alt),
                pattern: format!("{WORD_PRE}(?:{alt}){WORD_POST}"),
            }
        } else {
            match lit {
                Some(l) => {
                    if l.text.is_empty() && !l.start && !l.end {
                        return Filter::All { exact };
                    }
                    let mut pattern = String::new();
                    if !l.start {
                        pattern.push('%');
                    }
                    pattern.push_str(&escape_like(&l.text));
                    if !l.end {
                        pattern.push('%');
                    }
                    Cond {
                        op: Op::of(ci, true),
                        pattern,
                        literal: None,
                    }
                }
                None => Cond {
                    op: Op::of(ci, false),
                    literal: required_like(&alt),
                    pattern: alt,
                },
            }
        };
        if !conds.contains(&cond) {
            conds.push(cond);
        }
    }
    Filter::Any { conds, exact }
}

/// A LIKE pattern for the longest text every match of the ARE alternative
/// contains, when it has three letters or digits in a row (what pg_trgm
/// needs for a trigram).
fn required_like(alt: &str) -> Option<String> {
    let lit = are::required_literal(alt)?;
    let mut run = 0;
    let strong = lit.chars().any(|c| {
        run = if c.is_alphanumeric() { run + 1 } else { 0 };
        run >= 3
    });
    strong.then(|| format!("%{}%", escape_like(&lit)))
}

static CAMEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([\p{Ll}\p{N}])(\p{Lu})").unwrap());
static ACRONYM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\p{Lu})(\p{Lu}\p{Ll})").unwrap());
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}]+").unwrap());

/// The words of a name in any naming convention: `ScenarioRunner`,
/// `scenario_runner`, `SCENARIO-RUNNER` and `scenario runner` all give
/// `["scenario", "runner"]`.
pub fn name_words(name: &str) -> Vec<String> {
    let split = CAMEL.replace_all(name, "$1 $2");
    let split = ACRONYM.replace_all(&split, "$1 $2");
    NON_WORD
        .split(&split.to_lowercase())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A case-insensitive Rust regex for a name in every spelling: its words
/// joined by an optional separator, as in `scenario[-_. ]?runner`.
pub fn identifier_pattern(name: &str) -> Option<String> {
    let words = name_words(name);
    if words.is_empty() {
        return None;
    }
    Some(
        words
            .iter()
            .map(|w| regex::escape(w))
            .collect::<Vec<_>>()
            .join("[-_. ]?"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(patterns: &'a [String], dialect: Dialect) -> PatternSpec<'a> {
        PatternSpec {
            patterns,
            dialect,
            ignore_case: false,
            word: false,
            line: false,
            semantics: Semantics::LeftmostFirst,
        }
    }

    fn conds(filter: &Filter) -> Vec<(String, String)> {
        match filter {
            Filter::Any { conds, .. } => conds
                .iter()
                .map(|c| (c.op.sql().to_owned(), c.pattern.clone()))
                .collect(),
            Filter::All { .. } => vec![],
        }
    }

    fn pair(op: &str, pattern: &str) -> (String, String) {
        (op.to_owned(), pattern.to_owned())
    }

    #[test]
    fn plain_text_becomes_like() {
        let p = vec!["foo_bar%".to_owned()];
        let c = compile(&spec(&p, Dialect::Fixed)).unwrap();
        assert_eq!(conds(&c.filter), vec![pair("LIKE", r"%foo\_bar\%%")]);
        assert!(c.matcher.is_match("x foo_bar% y"));
        assert!(!c.matcher.is_match("fooxbar%"));
    }

    #[test]
    fn basic_alternatives_split_into_conditions() {
        let p = vec![r"^static\|palloc(".to_owned()];
        let c = compile(&spec(&p, Dialect::Basic)).unwrap();
        assert_eq!(
            conds(&c.filter),
            vec![pair("LIKE", "static%"), pair("LIKE", "%palloc(%")]
        );
        assert!(c.matcher.is_match("x = palloc(8);"));
        assert!(!c.matcher.is_match("  static int x;"));
    }

    #[test]
    fn regexes_carry_their_required_text() {
        let p = vec!["T_[A-Za-z]{3,8}Stmt".to_owned()];
        let c = compile(&spec(&p, Dialect::Rust)).unwrap();
        let Filter::Any { conds, .. } = &c.filter else {
            panic!()
        };
        assert_eq!(conds[0].literal.as_deref(), Some("%Stmt%"));
        let p = vec![r"\d{4,}".to_owned()];
        let c = compile(&spec(&p, Dialect::Rust)).unwrap();
        let Filter::Any { conds, .. } = &c.filter else {
            panic!()
        };
        assert_eq!(conds[0].literal, None);
    }

    #[test]
    fn regexes_and_case() {
        let p = vec![r"make_\w+Node".to_owned()];
        let mut s = spec(&p, Dialect::Rust);
        s.ignore_case = true;
        let c = compile(&s).unwrap();
        assert_eq!(conds(&c.filter), vec![pair("~*", r"make_\w+Node")]);
        assert!(c.matcher.is_match("MAKE_constNODE"));
    }

    #[test]
    fn whole_words_and_lines() {
        let p = vec!["node".to_owned()];
        let mut s = spec(&p, Dialect::Fixed);
        s.word = true;
        let c = compile(&s).unwrap();
        assert_eq!(
            conds(&c.filter),
            vec![pair("~", &format!("{WORD_PRE}(?:node){WORD_POST}"))]
        );
        assert!(c.matcher.is_match("a node."));
        assert!(!c.matcher.is_match("nodes"));
        let mut s = spec(&p, Dialect::Fixed);
        s.line = true;
        let c = compile(&s).unwrap();
        assert_eq!(conds(&c.filter), vec![pair("LIKE", "node")]);
        assert!(c.matcher.is_match("node") && !c.matcher.is_match(" node"));
    }

    #[test]
    fn empty_pattern_matches_every_line() {
        let p = vec![String::new()];
        let c = compile(&spec(&p, Dialect::Basic)).unwrap();
        assert_eq!(c.filter, Filter::All { exact: true });
        assert!(c.matcher.is_match("anything"));
    }

    #[test]
    fn several_patterns_keep_their_own_groups() {
        let p = vec![r"\(a\)\1".to_owned(), r"\(b\)\1".to_owned()];
        let c = compile(&spec(&p, Dialect::Basic)).unwrap();
        assert!(c.matcher.is_match("xbb") && c.matcher.is_match("aa"));
        assert!(!c.matcher.is_match("ab"));
    }

    #[test]
    fn widened_patterns_are_inexact() {
        let p = vec![r"\p{Lu}+_CONST".to_owned()];
        let c = compile(&spec(&p, Dialect::Rust)).unwrap();
        assert_eq!(
            c.filter,
            Filter::Any {
                conds: vec![Cond {
                    op: Op::Re,
                    pattern: ".+_CONST".into(),
                    literal: Some(r"%\_CONST%".into()),
                }],
                exact: false
            }
        );
    }

    #[test]
    fn names_in_any_spelling() {
        assert_eq!(name_words("ScenarioRunner"), vec!["scenario", "runner"]);
        assert_eq!(name_words("SCENARIO_RUNNER"), vec!["scenario", "runner"]);
        assert_eq!(
            name_words("getHTTPResponse"),
            vec!["get", "http", "response"]
        );
        assert_eq!(name_words("live mode"), vec!["live", "mode"]);
        assert_eq!(
            identifier_pattern("parseNodeString").unwrap(),
            "parse[-_. ]?node[-_. ]?string"
        );
    }
}
