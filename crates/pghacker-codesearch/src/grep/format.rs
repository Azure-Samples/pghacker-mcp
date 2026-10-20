// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! grep's output for one file at a time, as GNU grep 3.11 prints it: line
//! heads (`file:12:`, `-` on context lines, NUL after names with -Z),
//! context with `--` between groups, -o, -c, -l, -L and -m.

use super::options::{Mode, Options};
use crate::pattern::LineMatcher;

/// Output state across files.
pub struct Printer<'a> {
    opts: &'a Options,
    matcher: &'a LineMatcher,
    pub with_filename: bool,
    pub out: Vec<u8>,
    /// Something was printed (the next group gets a separator).
    used: bool,
    /// Selected lines in all files.
    pub selected: u64,
    /// Files listed by -l or -L.
    pub listed: u64,
}

impl<'a> Printer<'a> {
    pub fn new(opts: &'a Options, matcher: &'a LineMatcher, with_filename: bool) -> Self {
        Self {
            opts,
            matcher,
            with_filename,
            out: Vec::new(),
            used: false,
            selected: 0,
            listed: 0,
        }
    }

    fn head(&mut self, name: &str, line: i32, sep: u8) {
        if self.with_filename {
            self.out.extend_from_slice(name.as_bytes());
            self.out.push(if self.opts.null { 0 } else { sep });
        }
        if self.opts.line_number {
            self.out.extend_from_slice(line.to_string().as_bytes());
            self.out.push(sep);
        }
    }

    fn context_line(&mut self, name: &str, n: i32, text: &str) {
        if self.opts.only_matching {
            return;
        }
        self.head(name, n, b'-');
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(b'\n');
    }

    fn selected_line(&mut self, name: &str, n: i32, text: &str) {
        if self.opts.only_matching {
            if self.opts.invert {
                return;
            }
            for (s, e) in self.matcher.matches(text) {
                self.head(name, n, b':');
                self.out.extend_from_slice(&text.as_bytes()[s..e]);
                self.out.push(b'\n');
            }
            return;
        }
        self.head(name, n, b':');
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(b'\n');
    }

    /// Prints one file. `selected` are its selected line numbers in order;
    /// `count` is their number when the lines themselves are not needed;
    /// `text` gives any line's text; `last` is the file's last line number.
    /// Returns the number of selected lines (after -m).
    pub fn file(
        &mut self,
        name: &str,
        selected: &[i32],
        count: Option<u64>,
        last: i32,
        text: &dyn Fn(i32) -> Option<String>,
    ) -> u64 {
        let max = self
            .opts
            .max_count
            .map_or(u64::MAX, |m| u64::try_from(m).unwrap_or(0));
        let found = count.unwrap_or(selected.len() as u64).min(max);
        self.selected += found;
        match self.opts.mode {
            Mode::Quiet => return found,
            Mode::Count => {
                if self.with_filename {
                    self.out.extend_from_slice(name.as_bytes());
                    self.out.push(if self.opts.null { 0 } else { b':' });
                }
                self.out.extend_from_slice(format!("{found}\n").as_bytes());
                return found;
            }
            Mode::FilesWithMatches | Mode::FilesWithoutMatch => {
                let listed = (found > 0) == (self.opts.mode == Mode::FilesWithMatches);
                if listed {
                    self.listed += 1;
                    self.out.extend_from_slice(name.as_bytes());
                    self.out.push(if self.opts.null { 0 } else { b'\n' });
                }
                return found;
            }
            Mode::Lines => {}
        }
        let (before, after) = self.opts.context_lines();
        let mut last_printed = 0;
        let mut pending_until = 0;
        for &s in selected.iter().take(found as usize) {
            while last_printed < pending_until && last_printed + 1 < s {
                last_printed += 1;
                if let Some(t) = text(last_printed) {
                    self.context_line(name, last_printed, &t);
                }
            }
            let start = s.saturating_sub(before).max(last_printed + 1).max(1);
            let adjacent = last_printed > 0 && start == last_printed + 1;
            if self.opts.context_on()
                && self.used
                && !adjacent
                && let Some(sep) = &self.opts.group_separator
            {
                self.out.extend_from_slice(sep.as_bytes());
                self.out.push(b'\n');
            }
            for n in start..s {
                if let Some(t) = text(n) {
                    self.context_line(name, n, &t);
                }
            }
            if let Some(t) = text(s) {
                self.selected_line(name, s, &t);
            }
            last_printed = s;
            pending_until = s.saturating_add(after);
            self.used = true;
        }
        while last_printed < pending_until && last_printed < last {
            last_printed += 1;
            if let Some(t) = text(last_printed) {
                self.context_line(name, last_printed, &t);
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{Dialect, PatternSpec, Semantics, compile};

    fn run(args: &[&str], files: &[(&str, &str)]) -> String {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let opts = Options::parse(&args).unwrap();
        let compiled = compile(&PatternSpec {
            patterns: &opts.patterns,
            dialect: opts.dialect,
            ignore_case: opts.ignore_case,
            word: opts.word,
            line: opts.line,
            semantics: if opts.dialect == Dialect::Perl {
                Semantics::LeftmostFirst
            } else {
                Semantics::LeftmostLongest
            },
        })
        .unwrap();
        let mut p = Printer::new(&opts, &compiled.matcher, files.len() > 1);
        for (name, content) in files {
            let lines: Vec<&str> = content.lines().collect();
            let selected: Vec<i32> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| compiled.matcher.is_match(l) != opts.invert)
                .map(|(k, _)| k as i32 + 1)
                .collect();
            let text = |n: i32| lines.get(n as usize - 1).map(|s| (*s).to_owned());
            p.file(name, &selected, None, lines.len() as i32, &text);
        }
        String::from_utf8(p.out).unwrap()
    }

    const A: &str = "one\nfoo two\nthree\nfour\nfive\nfoo six\nseven\neight\nnine\nfoo ten\n";

    #[test]
    fn lines_with_names_and_numbers() {
        assert_eq!(run(&["foo"], &[("a", A)]), "foo two\nfoo six\nfoo ten\n");
        assert_eq!(
            run(&["-n", "foo"], &[("a", A), ("b", "foo\n")]),
            "a:2:foo two\na:6:foo six\na:10:foo ten\nb:1:foo\n"
        );
    }

    #[test]
    fn context_and_separators() {
        assert_eq!(
            run(&["-n", "-C1", "foo"], &[("a", A)]),
            "1-one\n2:foo two\n3-three\n--\n5-five\n6:foo six\n7-seven\n--\n9-nine\n10:foo ten\n"
        );
        assert_eq!(
            run(&["-A1", "foo"], &[("a", "foo\nx\nfoo\n"), ("b", "foo\n")]),
            "a:foo\na-x\na:foo\n--\nb:foo\n"
        );
        // -A0 still separates groups that are not adjacent.
        assert_eq!(
            run(&["-A0", "foo"], &[("a", A)]),
            "foo two\n--\nfoo six\n--\nfoo ten\n"
        );
        assert_eq!(
            run(&["--no-group-separator", "-A0", "foo"], &[("a", A)]),
            "foo two\nfoo six\nfoo ten\n"
        );
    }

    #[test]
    fn max_count_prints_trailing_context() {
        assert_eq!(
            run(&["-m1", "-A5", "foo"], &[("a", "foo 1\nfoo 2\nx\n")]),
            "foo 1\nfoo 2\nx\n"
        );
        assert_eq!(run(&["-m2", "-c", "foo"], &[("a", A)]), "2\n");
    }

    #[test]
    fn huge_context_saturates() {
        let all = "one\nfoo two\nthree\n";
        assert_eq!(
            run(&["-A2147483647", "foo"], &[("a", all)]),
            "foo two\nthree\n"
        );
        assert_eq!(run(&["-C9999999999", "foo"], &[("a", all)]), all);
    }

    #[test]
    fn only_matching_counts_and_lists() {
        assert_eq!(
            run(&["-on", "fo*"], &[("a", "foo fooo\nbar\n")]),
            "1:foo\n1:fooo\n"
        );
        assert_eq!(run(&["-c", "foo"], &[("a", A), ("b", "x\n")]), "a:3\nb:0\n");
        assert_eq!(run(&["-l", "foo"], &[("a", A), ("b", "x\n")]), "a\n");
        assert_eq!(run(&["-L", "foo"], &[("a", A), ("b", "x\n")]), "b\n");
        assert_eq!(run(&["-lZ", "foo"], &[("a", A)]), "a\0");
        assert_eq!(run(&["-cv", "o"], &[("a", A)]), "5\n");
        assert_eq!(run(&["-vo", "o"], &[("a", A)]), "");
    }
}
