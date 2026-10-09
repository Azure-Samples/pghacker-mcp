//! The exact line matcher: Rust's regex crate (ripgrep's engine), or
//! fancy-regex for backreferences and lookarounds.

use regex_automata::nfa::thompson::pikevm::PikeVM;
use regex_automata::{Anchored, Input, MatchKind};

/// Which match to report when several start at the same place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Semantics {
    /// The first alternative that matches, as Perl and ripgrep report it.
    LeftmostFirst,
    /// The longest match, as POSIX and grep report it.
    LeftmostLongest,
}

#[derive(Clone, Debug)]
enum Engine {
    Std(regex::Regex),
    Fancy(fancy_regex::Regex),
}

/// A compiled line pattern.
#[derive(Clone, Debug)]
pub struct LineMatcher {
    engine: Engine,
    longest: Option<PikeVM>,
    pattern: String,
}

impl LineMatcher {
    /// Compiles `pattern` (Rust syntax). With `fancy`, or when the regex
    /// crate rejects it for lookarounds or backreferences, fancy-regex is
    /// used.
    pub fn new(pattern: &str, semantics: Semantics, fancy: bool) -> Result<Self, String> {
        let engine = if fancy {
            Engine::Fancy(compile_fancy(pattern)?)
        } else {
            match regex::RegexBuilder::new(pattern)
                .size_limit(64 << 20)
                .build()
            {
                Ok(re) => Engine::Std(re),
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("look-around") || message.contains("backreference") {
                        Engine::Fancy(compile_fancy(pattern).map_err(|_| message.clone())?)
                    } else {
                        return Err(clean_error(&message));
                    }
                }
            }
        };
        let longest = match (&engine, semantics) {
            (Engine::Std(_), Semantics::LeftmostLongest) => PikeVM::builder()
                .configure(PikeVM::config().match_kind(MatchKind::All))
                .build(pattern)
                .ok(),
            _ => None,
        };
        Ok(Self {
            engine,
            longest,
            pattern: pattern.to_owned(),
        })
    }

    /// The pattern in Rust syntax.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Whether `line` has a match.
    pub fn is_match(&self, line: &str) -> bool {
        match &self.engine {
            Engine::Std(re) => re.is_match(line),
            Engine::Fancy(re) => re.is_match(line).unwrap_or(false),
        }
    }

    /// The first match that starts at or after `start`, with the text before
    /// `start` as context for anchors and word boundaries.
    pub fn find_at(&self, line: &str, start: usize) -> Option<(usize, usize)> {
        match &self.engine {
            Engine::Std(re) => {
                let m = re.find_at(line, start)?;
                let end = match &self.longest {
                    Some(vm) => longest_end(vm, line, m.start()).unwrap_or(m.end()),
                    None => m.end(),
                };
                Some((m.start(), end))
            }
            Engine::Fancy(re) => {
                let m = re.find_from_pos(line, start).ok()??;
                Some((m.start(), m.end()))
            }
        }
    }

    /// The non-empty matches in `line`, left to right, as grep `-o` prints
    /// them: after an empty match the search moves on by one character.
    pub fn matches(&self, line: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= line.len() {
            let Some((start, end)) = self.find_at(line, at) else {
                break;
            };
            if end > start {
                out.push((start, end));
                at = end;
            } else {
                at = start + line[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
        out
    }
}

fn compile_fancy(pattern: &str) -> Result<fancy_regex::Regex, String> {
    fancy_regex::RegexBuilder::new(pattern)
        .backtrack_limit(1_000_000)
        .build()
        .map_err(|error| clean_error(&error.to_string()))
}

/// The longest match that starts at `start`.
fn longest_end(vm: &PikeVM, line: &str, start: usize) -> Option<usize> {
    let mut cache = vm.create_cache();
    let input = Input::new(line).range(start..).anchored(Anchored::Yes);
    let mut slots = [None, None];
    vm.search_slots(&mut cache, &input, &mut slots)?;
    slots[1].map(|end| end.get())
}

/// The regex crate's error without its ASCII-art pointer lines.
fn clean_error(message: &str) -> String {
    let lines: Vec<&str> = message
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.trim_start().starts_with('^'))
        .collect();
    match lines.iter().rposition(|line| line.starts_with("error:")) {
        Some(k) => lines[k].trim_start_matches("error:").trim().to_owned(),
        None => lines.join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_versus_first() {
        let first = LineMatcher::new("ab|abc", Semantics::LeftmostFirst, false).unwrap();
        assert_eq!(first.find_at("xabcd", 0), Some((1, 3)));
        let longest = LineMatcher::new("ab|abc", Semantics::LeftmostLongest, false).unwrap();
        assert_eq!(longest.find_at("xabcd", 0), Some((1, 4)));
    }

    #[test]
    fn only_matching_skips_empty_matches() {
        let m = LineMatcher::new("a*", Semantics::LeftmostLongest, false).unwrap();
        assert_eq!(m.matches("baab"), vec![(1, 3)]);
        let w = LineMatcher::new(
            r"\b{start-half}(?:foo\w*)\b{end-half}",
            Semantics::LeftmostLongest,
            false,
        )
        .unwrap();
        assert_eq!(w.matches("xfoo foo foobar"), vec![(5, 8), (9, 15)]);
    }

    #[test]
    fn backreferences_use_fancy_regex() {
        let m = LineMatcher::new(r"(a)\1", Semantics::LeftmostFirst, false).unwrap();
        assert!(m.is_match("xaay"));
        assert!(!m.is_match("ab"));
        let look = LineMatcher::new(r"foo(?=bar)", Semantics::LeftmostFirst, false).unwrap();
        assert_eq!(look.matches("foobar foobaz"), vec![(0, 3)]);
    }

    #[test]
    fn errors_are_short() {
        let error = LineMatcher::new("a(", Semantics::LeftmostFirst, false).unwrap_err();
        assert!(error.contains("unclosed group"), "{error}");
    }
}
