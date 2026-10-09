//! File-name patterns: ripgrep's `-g` globs (gitignore conventions) for
//! `search`, and GNU grep's `--include`, `--exclude` and `--exclude-dir`
//! (fnmatch without FNM_PATHNAME) for `grep`.

use globset::{GlobBuilder, GlobMatcher};

/// A glob as ripgrep's `-g` reads it: without a slash it matches a name at
/// any depth, with one it matches the path from the repository root, and a
/// trailing slash matches folders only.
#[derive(Clone, Debug)]
pub struct RgGlob {
    matcher: GlobMatcher,
    dir_only: bool,
}

impl RgGlob {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let mut line = pattern.trim();
        let mut anchored = false;
        if let Some(rest) = line.strip_prefix('/') {
            line = rest;
            anchored = true;
        }
        let mut dir_only = false;
        if let Some(rest) = line.strip_suffix('/') {
            dir_only = true;
            line = rest.strip_suffix('\\').unwrap_or(rest);
        }
        let mut actual = line.to_owned();
        if !anchored && !line.contains('/') && !line.starts_with("**/") {
            actual = format!("**/{actual}");
        }
        if actual.ends_with("/**") {
            actual.push_str("/*");
        }
        let glob = GlobBuilder::new(&actual)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| format!("invalid glob {pattern}: {}", error.kind()))?;
        Ok(Self {
            matcher: glob.compile_matcher(),
            dir_only,
        })
    }

    /// Whether the glob matches a file or folder path from the root.
    pub fn is_match(&self, path: &str, is_dir: bool) -> bool {
        (is_dir || !self.dir_only) && self.matcher.is_match(path)
    }
}

/// ripgrep's include (`-g glob`) and exclude (`-g '!glob'`) globs.
#[derive(Clone, Debug, Default)]
pub struct RgFilter {
    include: Vec<RgGlob>,
    exclude: Vec<RgGlob>,
}

impl RgFilter {
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, String> {
        let mut filter = RgFilter::default();
        for g in include.iter().filter(|g| !g.trim().is_empty()) {
            match g.trim().strip_prefix('!') {
                Some(negated) => filter.exclude.push(RgGlob::new(negated)?),
                None => filter.include.push(RgGlob::new(g)?),
            }
        }
        for g in exclude.iter().filter(|g| !g.trim().is_empty()) {
            let g = g.trim();
            filter
                .exclude
                .push(RgGlob::new(g.strip_prefix('!').unwrap_or(g))?);
        }
        Ok(filter)
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Whether a file found below the search root `root` is searched: it
    /// matches an include glob (if any), and neither it nor a folder between
    /// the root and it matches an exclude glob.
    pub fn allows(&self, root: &str, path: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|g| g.is_match(path, false)) {
            return false;
        }
        if self.exclude.is_empty() {
            return true;
        }
        if self.exclude.iter().any(|g| g.is_match(path, false)) {
            return false;
        }
        let mut end = if root.is_empty() { 0 } else { root.len() + 1 };
        while let Some(k) = path[end..].find('/') {
            let dir = &path[..end + k];
            if self.exclude.iter().any(|g| g.is_match(dir, true)) {
                return false;
            }
            end += k + 1;
        }
        true
    }
}

/// fnmatch(3) as GNU grep calls it for file names: `*`, `?`, brackets and
/// backslash escapes, where `*` also matches `/` and a leading `.`.
pub fn fnmatch(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        let step = match p.get(pi) {
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match bracket(&p[pi..], s[si]) {
                Some((true, len)) => Some(len),
                Some((false, _)) => None,
                None => (s[si] == '[').then_some(1),
            },
            Some('\\') if pi + 1 < p.len() => (p[pi + 1] == s[si]).then_some(2),
            Some(&c) => (c == s[si]).then_some(1),
            None => None,
        };
        match step {
            Some(len) => {
                pi += len;
                si += 1;
            }
            None => match star {
                Some((sp, ss)) => {
                    pi = sp + 1;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return false,
            },
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// A bracket expression at the start of `p` against `c`: (matched, length),
/// or None if the bracket is not terminated.
fn bracket(p: &[char], c: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let ch = *p.get(i)?;
        if ch == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if ch == '[' && p.get(i + 1) == Some(&':') {
            let rest: String = p[i + 2..].iter().collect();
            if let Some(end) = rest.find(":]") {
                let name = &rest[..end];
                matched |= posix_class(name, c);
                i += 2 + name.chars().count() + 2;
                continue;
            }
        }
        let mut lo = ch;
        if ch == '\\' {
            lo = *p.get(i + 1)?;
            i += 1;
        }
        i += 1;
        if p.get(i) == Some(&'-') && p.get(i + 1).is_some_and(|&h| h != ']') {
            let mut hi = p[i + 1];
            i += 2;
            if hi == '\\' {
                hi = *p.get(i)?;
                i += 1;
            }
            matched |= lo <= c && c <= hi;
        } else {
            matched |= lo == c;
        }
    }
}

fn posix_class(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_alphanumeric(),
        "upper" => c.is_uppercase(),
        "lower" => c.is_lowercase(),
        "space" => c.is_whitespace(),
        "blank" => c == ' ' || c == '\t',
        "punct" => c.is_ascii_punctuation(),
        "xdigit" => c.is_ascii_hexdigit(),
        "cntrl" => c.is_control(),
        "print" => !c.is_control(),
        "graph" => !c.is_control() && !c.is_whitespace(),
        _ => false,
    }
}

/// GNU grep's `--include` and `--exclude` patterns in command-line order
/// (or its `--exclude-dir` patterns).
#[derive(Clone, Debug, Default)]
pub struct GnuExclude {
    /// (is `--include`, pattern).
    patterns: Vec<(bool, String)>,
}

impl GnuExclude {
    pub fn push(&mut self, include: bool, pattern: &str) {
        self.patterns.push((include, pattern.to_owned()));
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether `name` is skipped. Files found while recursing are matched by
    /// base name (`anchored`); command-line names also by every suffix that
    /// starts after a slash. The last matching option wins; when none
    /// matches, the name is skipped only if the first option is --include.
    pub fn excluded(&self, name: &str, anchored: bool) -> bool {
        let Some(first) = self.patterns.first() else {
            return false;
        };
        for (include, pattern) in self.patterns.iter().rev() {
            if Self::matches(pattern, name, anchored) {
                return !include;
            }
        }
        first.0
    }

    fn matches(pattern: &str, name: &str, anchored: bool) -> bool {
        if fnmatch(pattern, name) {
            return true;
        }
        if anchored {
            return false;
        }
        let bytes = name.as_bytes();
        (0..bytes.len()).any(|k| {
            bytes[k] == b'/'
                && k + 1 < bytes.len()
                && bytes[k + 1] != b'/'
                && fnmatch(pattern, &name[k + 1..])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ripgrep_globs() {
        let g = RgGlob::new("*.c").unwrap();
        assert!(g.is_match("src/a.c", false) && g.is_match("a.c", false));
        assert!(!g.is_match("src/a.h", false));
        let anchored = RgGlob::new("src/*.c").unwrap();
        assert!(anchored.is_match("src/a.c", false));
        assert!(!anchored.is_match("x/src/a.c", false) && !anchored.is_match("src/b/a.c", false));
        let braces = RgGlob::new("**/*.{c,h}").unwrap();
        assert!(braces.is_match("a/b.h", false));
        let dir = RgGlob::new("parser/").unwrap();
        assert!(dir.is_match("src/parser", true) && !dir.is_match("src/parser", false));
        let inside = RgGlob::new("src/**").unwrap();
        assert!(inside.is_match("src/a/b.c", false) && !inside.is_match("src", true));
    }

    #[test]
    fn ripgrep_filters_prune_below_the_root_only() {
        let f = RgFilter::new(&[], &["src".into()]).unwrap();
        assert!(!f.allows("", "src/backend/a.c"));
        assert!(!f.allows("", "x/src/y.c"));
        assert!(f.allows("src/backend", "src/backend/a.c"));
        let f = RgFilter::new(&["*.c".into(), "!*test*".into()], &[]).unwrap();
        assert!(f.allows("", "src/a.c"));
        assert!(!f.allows("", "src/a.h"));
        assert!(!f.allows("", "src/test/t.c"));
    }

    #[test]
    fn fnmatch_semantics() {
        assert!(fnmatch("*.c", "a.c") && fnmatch("*.c", "dir/a.c"));
        assert!(fnmatch("*", ".hidden"));
        assert!(fnmatch("a?c", "abc") && !fnmatch("a?c", "ac"));
        assert!(fnmatch("[a-c]x", "bx") && !fnmatch("[!a-c]x", "bx"));
        assert!(fnmatch("*.[ch]", "x.h"));
        assert!(fnmatch(r"\*", "*") && !fnmatch(r"\*", "a"));
        assert!(fnmatch("*.{c,h}", "x.{c,h}") && !fnmatch("*.{c,h}", "x.c"));
        assert!(fnmatch("[[:digit:]]*", "1a"));
    }

    #[test]
    fn gnu_include_exclude_order() {
        let mut e = GnuExclude::default();
        e.push(true, "*.c");
        assert!(!e.excluded("a.c", true));
        assert!(e.excluded("a.h", true));
        let mut e = GnuExclude::default();
        e.push(false, "a*");
        e.push(true, "*b");
        // Neither matches and the first option is --exclude: included.
        assert!(!e.excluded("xyz", true));
        assert!(e.excluded("ax", true));
        assert!(!e.excluded("ab", true));
        let mut dirs = GnuExclude::default();
        dirs.push(false, "parser");
        assert!(dirs.excluded("src/backend/parser", false));
        assert!(!dirs.excluded("src/backend/parser", true));
    }
}
