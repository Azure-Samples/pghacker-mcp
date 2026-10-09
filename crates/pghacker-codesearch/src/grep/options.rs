//! GNU grep 3.11's command-line options, parsed as its getopt_long does:
//! options may follow operands, short options bundle (`-rnE`), values
//! attach (`-A3`, `-efoo`) or follow, long options take `=value` and may be
//! abbreviated, `-NUM` sets the context, and `--` ends the options.

use crate::glob::GnuExclude;
use crate::pattern::Dialect;

/// What grep prints per file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Lines,
    Count,
    FilesWithMatches,
    FilesWithoutMatch,
    Quiet,
}

/// What grep does with a directory operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Directories {
    Read,
    Skip,
    Recurse,
}

/// grep's options and operands.
#[derive(Clone, Debug)]
pub struct Options {
    pub dialect: Dialect,
    /// Patterns from -e, or the first operand.
    pub patterns: Vec<String>,
    pub ignore_case: bool,
    pub word: bool,
    pub line: bool,
    pub invert: bool,
    pub mode: Mode,
    pub only_matching: bool,
    pub line_number: bool,
    /// -H (Some(true)) or -h (Some(false)), the last one given.
    pub with_filename: Option<bool>,
    pub null: bool,
    /// -A and -B, or -1 when not given.
    pub after: i64,
    pub before: i64,
    pub max_count: Option<i64>,
    pub directories: Directories,
    /// --include and --exclude, in order.
    pub files: GnuExclude,
    pub exclude_dir: GnuExclude,
    pub no_messages: bool,
    pub group_separator: Option<String>,
    pub operands: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arg {
    None,
    Required,
    Optional,
}

/// Long options: (name, argument, short equivalent or an internal code).
const LONG: &[(&str, Arg, char)] = &[
    ("basic-regexp", Arg::None, 'G'),
    ("extended-regexp", Arg::None, 'E'),
    ("fixed-regexp", Arg::None, 'F'),
    ("fixed-strings", Arg::None, 'F'),
    ("perl-regexp", Arg::None, 'P'),
    ("after-context", Arg::Required, 'A'),
    ("before-context", Arg::Required, 'B'),
    ("binary-files", Arg::Required, '\u{1}'),
    ("byte-offset", Arg::None, 'b'),
    ("context", Arg::Required, 'C'),
    ("color", Arg::Optional, '\u{2}'),
    ("colour", Arg::Optional, '\u{2}'),
    ("count", Arg::None, 'c'),
    ("devices", Arg::Required, 'D'),
    ("directories", Arg::Required, 'd'),
    ("exclude", Arg::Required, '\u{3}'),
    ("exclude-from", Arg::Required, '\u{4}'),
    ("exclude-dir", Arg::Required, '\u{5}'),
    ("file", Arg::Required, 'f'),
    ("files-with-matches", Arg::None, 'l'),
    ("files-without-match", Arg::None, 'L'),
    ("group-separator", Arg::Required, '\u{6}'),
    ("help", Arg::None, '\u{7}'),
    ("include", Arg::Required, '\u{8}'),
    ("ignore-case", Arg::None, 'i'),
    ("no-ignore-case", Arg::None, '\u{9}'),
    ("initial-tab", Arg::None, 'T'),
    ("label", Arg::Required, '\u{a}'),
    ("line-buffered", Arg::None, '\u{b}'),
    ("line-number", Arg::None, 'n'),
    ("line-regexp", Arg::None, 'x'),
    ("max-count", Arg::Required, 'm'),
    ("no-filename", Arg::None, 'h'),
    ("no-group-separator", Arg::None, '\u{c}'),
    ("no-messages", Arg::None, 's'),
    ("null", Arg::None, 'Z'),
    ("null-data", Arg::None, 'z'),
    ("only-matching", Arg::None, 'o'),
    ("quiet", Arg::None, 'q'),
    ("recursive", Arg::None, 'r'),
    ("dereference-recursive", Arg::None, 'R'),
    ("regexp", Arg::Required, 'e'),
    ("invert-match", Arg::None, 'v'),
    ("silent", Arg::None, 'q'),
    ("text", Arg::None, 'a'),
    ("binary", Arg::None, 'U'),
    ("unix-byte-offsets", Arg::None, 'u'),
    ("version", Arg::None, 'V'),
    ("with-filename", Arg::None, 'H'),
    ("word-regexp", Arg::None, 'w'),
];

/// Short options that take a value.
const SHORT_WITH_VALUE: &str = "ABCDXdefm";
const SHORT_FLAGS: &str = "EFGHIPTUVZLabchilnoqRrsuvwxyz";

fn context_arg(v: &str) -> Result<i64, String> {
    v.trim()
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .ok_or_else(|| format!("{v}: invalid context length argument"))
}

/// Parser state besides the options themselves.
struct State {
    matcher: Option<char>,
    keys: bool,
    default_context: i64,
    count: bool,
    list: Option<Mode>,
    quiet: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            matcher: None,
            keys: false,
            default_context: -1,
            count: false,
            list: None,
            quiet: false,
        }
    }
}

impl State {
    fn finish_digits(&mut self, digits: &mut String) -> Result<(), String> {
        if !digits.is_empty() {
            self.default_context = context_arg(digits)?;
            digits.clear();
        }
        Ok(())
    }
}

fn apply(
    o: &mut Options,
    st: &mut State,
    opt: char,
    value: Option<String>,
    name: &str,
) -> Result<(), String> {
    let value_or = || value.clone().unwrap_or_default();
    match opt {
        'E' | 'F' | 'G' | 'P' => {
            if st.matcher.is_some_and(|m| m != opt) {
                return Err("conflicting matchers specified".into());
            }
            st.matcher = Some(opt);
            o.dialect = match opt {
                'E' => Dialect::Extended,
                'F' => Dialect::Fixed,
                'P' => Dialect::Perl,
                _ => Dialect::Basic,
            };
        }
        'A' => o.after = context_arg(&value_or())?,
        'B' => o.before = context_arg(&value_or())?,
        'C' => st.default_context = context_arg(&value_or())?,
        'D' => {}
        'X' => return Err("-X is not supported; use -G, -E, -F or -P".into()),
        'd' => {
            o.directories = match value_or().as_str() {
                "read" => Directories::Read,
                "skip" => Directories::Skip,
                "recurse" => Directories::Recurse,
                v => return Err(format!("invalid argument '{v}' for '--directories'")),
            }
        }
        'e' => {
            o.patterns.push(value_or());
            st.keys = true;
        }
        'f' => {
            return Err("-f (patterns from a file) is not supported; give patterns with -e".into());
        }
        'm' => {
            let v = value_or();
            let n = v
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid max count: {v}"))?;
            o.max_count = (n >= 0).then_some(n);
        }
        'H' => o.with_filename = Some(true),
        'h' => o.with_filename = Some(false),
        'I' | 'U' | 'a' | 'u' | '\u{1}' | '\u{2}' | '\u{a}' | '\u{b}' => {}
        'T' => return Err("-T (--initial-tab) is not supported".into()),
        'V' | '\u{7}' => {
            return Err(format!(
                "{name} is not supported: this grep searches the index"
            ));
        }
        'b' => return Err("-b (--byte-offset) is not supported".into()),
        'z' => return Err("-z (--null-data) is not supported".into()),
        'L' => st.list = Some(Mode::FilesWithoutMatch),
        'l' => st.list = Some(Mode::FilesWithMatches),
        'Z' => o.null = true,
        'c' => st.count = true,
        'i' | 'y' => o.ignore_case = true,
        '\u{9}' => o.ignore_case = false,
        'n' => o.line_number = true,
        'o' => o.only_matching = true,
        'q' => st.quiet = true,
        'R' | 'r' => o.directories = Directories::Recurse,
        's' => o.no_messages = true,
        'v' => o.invert = true,
        'w' => o.word = true,
        'x' => o.line = true,
        '\u{3}' => o.files.push(false, &value_or()),
        '\u{8}' => o.files.push(true, &value_or()),
        '\u{5}' => {
            let v = value_or();
            // grep strips trailing slashes from --exclude-dir.
            let trimmed = v.trim_end_matches('/');
            o.exclude_dir
                .push(false, if trimmed.is_empty() { &v } else { trimmed });
        }
        '\u{4}' => return Err("--exclude-from is not supported".into()),
        '\u{6}' => o.group_separator = Some(value_or()),
        '\u{c}' => o.group_separator = None,
        _ => return Err(format!("invalid option -- '{opt}'")),
    }
    Ok(())
}

impl Options {
    /// Parses grep's arguments (without the program name).
    pub fn parse(args: &[String]) -> Result<Options, String> {
        let mut o = Options {
            dialect: Dialect::Basic,
            patterns: Vec::new(),
            ignore_case: false,
            word: false,
            line: false,
            invert: false,
            mode: Mode::Lines,
            only_matching: false,
            line_number: false,
            with_filename: None,
            null: false,
            after: -1,
            before: -1,
            max_count: None,
            directories: Directories::Read,
            files: GnuExclude::default(),
            exclude_dir: GnuExclude::default(),
            no_messages: false,
            group_separator: Some("--".into()),
            operands: Vec::new(),
        };
        let mut st = State::default();
        let mut operands = Vec::new();
        let mut i = 0;
        // Digits form one context number while they stay in one argument.
        let mut digits = String::new();
        let mut digit_arg: Option<usize> = None;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                operands.extend(args[i + 1..].iter().cloned());
                break;
            }
            if let Some(long) = arg.strip_prefix("--") {
                st.finish_digits(&mut digits)?;
                let (name, inline) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_owned())),
                    None => (long, None),
                };
                let (full, kind, opt) = lookup_long(name)?;
                let value =
                    match kind {
                        Arg::None => {
                            if inline.is_some() {
                                return Err(format!("option '--{full}' doesn't allow an argument"));
                            }
                            None
                        }
                        Arg::Optional => inline,
                        Arg::Required => match inline {
                            Some(v) => Some(v),
                            None => {
                                i += 1;
                                Some(args.get(i).cloned().ok_or_else(|| {
                                    format!("option '--{full}' requires an argument")
                                })?)
                            }
                        },
                    };
                apply(&mut o, &mut st, opt, value, &format!("--{full}"))?;
                i += 1;
                continue;
            }
            if arg.len() > 1 && arg.starts_with('-') {
                let chars: Vec<char> = arg[1..].chars().collect();
                let mut k = 0;
                while k < chars.len() {
                    let c = chars[k];
                    if c.is_ascii_digit() {
                        if digit_arg != Some(i) || digits.is_empty() {
                            digits.clear();
                        }
                        digits.push(c);
                        digit_arg = Some(i);
                        k += 1;
                        continue;
                    }
                    st.finish_digits(&mut digits)?;
                    if SHORT_WITH_VALUE.contains(c) {
                        let rest: String = chars[k + 1..].iter().collect();
                        let value = if rest.is_empty() {
                            i += 1;
                            args.get(i)
                                .cloned()
                                .ok_or_else(|| format!("option requires an argument -- '{c}'"))?
                        } else {
                            rest
                        };
                        apply(&mut o, &mut st, c, Some(value), &format!("-{c}"))?;
                        break;
                    }
                    if !SHORT_FLAGS.contains(c) {
                        return Err(format!("invalid option -- '{c}'"));
                    }
                    apply(&mut o, &mut st, c, None, &format!("-{c}"))?;
                    k += 1;
                }
                i += 1;
                continue;
            }
            operands.push(arg.clone());
            i += 1;
        }
        st.finish_digits(&mut digits)?;
        if !st.keys {
            if operands.is_empty() {
                return Err("no pattern: usage is grep [OPTION]... PATTERNS [FILE]...".into());
            }
            o.patterns.push(operands.remove(0));
        }
        o.operands = operands;
        if o.after < 0 {
            o.after = st.default_context;
        }
        if o.before < 0 {
            o.before = st.default_context;
        }
        // -q overrides -l and -L, which override -c.
        o.mode = if st.quiet {
            Mode::Quiet
        } else if let Some(list) = st.list {
            list
        } else if st.count {
            Mode::Count
        } else {
            Mode::Lines
        };
        Ok(o)
    }

    /// Context lines are on (even -A0): group separators are printed.
    pub fn context_on(&self) -> bool {
        self.after >= 0 || self.before >= 0
    }
}

/// A long option by name or unambiguous prefix.
fn lookup_long(name: &str) -> Result<(&'static str, Arg, char), String> {
    if let Some(&(full, kind, opt)) = LONG.iter().find(|(n, _, _)| *n == name) {
        return Ok((full, kind, opt));
    }
    let matches: Vec<&(&str, Arg, char)> = LONG
        .iter()
        .filter(|(n, _, _)| n.starts_with(name))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("unrecognized option '--{name}'")),
        [one] => Ok((one.0, one.1, one.2)),
        many => {
            // Abbreviations of synonyms (--fixed-regexp, --fixed-strings) agree.
            if many.iter().all(|m| m.2 == many[0].2 && m.1 == many[0].1) {
                Ok((many[0].0, many[0].1, many[0].2))
            } else {
                Err(format!("option '--{name}' is ambiguous"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn bundled_flags_and_values() {
        let o = parse(&["-rnEi", "a|b", "src"]).unwrap();
        assert_eq!(o.dialect, Dialect::Extended);
        assert!(o.line_number && o.ignore_case);
        assert_eq!(o.directories, Directories::Recurse);
        assert_eq!(o.patterns, vec!["a|b"]);
        assert_eq!(o.operands, vec!["src"]);
        let o = parse(&["-A3", "-B", "1", "-m5", "x"]).unwrap();
        assert_eq!((o.after, o.before, o.max_count), (3, 1, Some(5)));
        let o = parse(&["-C2", "-A0", "x"]).unwrap();
        assert_eq!((o.after, o.before), (0, 2));
        assert!(o.context_on());
    }

    #[test]
    fn several_patterns_and_permutation() {
        let o = parse(&["-e", "foo", "-efoo2", "file", "-n", "--regexp=bar"]).unwrap();
        assert_eq!(o.patterns, vec!["foo", "foo2", "bar"]);
        assert_eq!(o.operands, vec!["file"]);
        assert!(o.line_number);
        let o = parse(&["pat", "a", "-c", "--", "-b"]).unwrap();
        assert_eq!(o.patterns, vec!["pat"]);
        assert_eq!(o.operands, vec!["a", "-b"]);
        assert_eq!(o.mode, Mode::Count);
    }

    #[test]
    fn long_options_and_abbreviations() {
        let o = parse(&[
            "--include=*.c",
            "--exclude",
            "*.h",
            "--exclude-dir=test/",
            "--incl=*.y",
            "--fixed",
            "--ignore-case",
            "x",
        ])
        .unwrap();
        assert_eq!(o.dialect, Dialect::Fixed);
        assert!(o.ignore_case);
        assert!(!o.files.excluded("a.c", true) && o.files.excluded("a.h", true));
        assert!(!o.files.excluded("a.y", true));
        assert!(o.exclude_dir.excluded("test", true));
        assert!(parse(&["--ex=a", "x"]).unwrap_err().contains("ambiguous"));
        assert!(parse(&["--count=1", "x"]).is_err());
        assert!(parse(&["--nope", "x"]).is_err());
    }

    #[test]
    fn digit_contexts_and_modes() {
        let o = parse(&["-5", "x"]).unwrap();
        assert_eq!((o.after, o.before), (5, 5));
        let o = parse(&["-1", "-2", "x"]).unwrap();
        assert_eq!(o.after, 2);
        let o = parse(&["-n12", "x"]).unwrap();
        assert_eq!(o.after, 12);
        assert_eq!(parse(&["-lc", "x"]).unwrap().mode, Mode::FilesWithMatches);
        assert_eq!(parse(&["-lq", "x"]).unwrap().mode, Mode::Quiet);
        assert!(
            parse(&["-E", "-F", "x"])
                .unwrap_err()
                .contains("conflicting")
        );
        assert!(parse(&["-k", "x"]).is_err());
        assert!(parse(&[]).is_err());
        assert!(parse(&["-e"]).is_err());
    }
}
