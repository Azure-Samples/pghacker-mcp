// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The PG19 replay: serves the PG19 subset from a commit history of its own
//! (schema `replay_pg19`, branch REL_19_STABLE), generates searches from its
//! own code with a fixed seed, and checks each three ways: `search` and the
//! grep tool's selected lines against ripgrep (patterns translated to Rust
//! syntax), and the grep tool's whole output against GNU grep running the
//! same command line in Docker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, stream};
use pghacker_codesearch::glob::fnmatch;
use pghacker_codesearch::grep::{self as grep_tool, Limits};
use pghacker_codesearch::pattern::Dialect;
use pghacker_codesearch::pattern::gnu::gnu_to_rust;
use pghacker_codesearch::revision::FileTable;
use pghacker_codesearch::search::{MatchMode, OutputMode, Strings};
use pghacker_codesearch::{Revision, SearchArgs, search};
use pghacker_core::db;
use regex::Regex;
use serde::Serialize;
use tokio_postgres::Client;

use crate::rg::{RgLines, RgOpts, rg_lines};
use crate::{Ctx, gnu, percentile, tree};

const URL: &str = "https://github.com/postgres/postgres.git";
const REF: &str = "REL_19_STABLE";
const SHA: &str = "2359968b449a0fd53cfc04d4a644ae02370adec6";
/// The history the subset is served from.
const SCHEMA: &str = "replay_pg19";
const PATHS: [&str; 4] = [
    "src/backend/parser",
    "src/backend/utils/adt",
    "src/include/nodes",
    "contrib/pg_trgm",
];
/// Where commands with absolute paths think the checkout is.
const ABS_ROOT: &str = "/home/agent/postgres";

/// splitmix64: a small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// Names and messages taken from the subset's code.
struct Corpus {
    files: Vec<String>,
    dirs: Vec<String>,
    /// Identifiers by frequency, most frequent first.
    idents: Vec<String>,
    /// Identifiers that occur at most three times.
    rare: Vec<String>,
    /// errmsg() strings.
    messages: Vec<String>,
    /// Files by extension.
    by_ext: BTreeMap<String, Vec<String>>,
}

fn read_corpus(tree: &Path, files: &[String]) -> Result<Corpus> {
    let ident = Regex::new(r"[A-Za-z_][A-Za-z0-9_]{3,}").unwrap();
    let errmsg = Regex::new(r#"errmsg\("([^"\\]{12,})"#).unwrap();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut messages = Vec::new();
    let mut by_ext: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dirs = HashSet::new();
    for f in files {
        let text = std::fs::read_to_string(tree.join(f)).unwrap_or_default();
        for m in ident.find_iter(&text) {
            *counts.entry(m.as_str().to_owned()).or_default() += 1;
        }
        for c in errmsg.captures_iter(&text) {
            messages.push(c[1].to_owned());
        }
        let ext = f.rsplit_once('.').map(|(_, e)| e).unwrap_or("").to_owned();
        by_ext.entry(ext).or_default().push(f.clone());
        let mut end = 0;
        while let Some(k) = f[end..].find('/') {
            end += k;
            dirs.insert(f[..end].to_owned());
            end += 1;
        }
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let rare = ranked
        .iter()
        .filter(|(_, n)| *n <= 3)
        .map(|(s, _)| s.clone())
        .collect();
    let idents = ranked.into_iter().take(3000).map(|(s, _)| s).collect();
    messages.sort();
    messages.dedup();
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort();
    Ok(Corpus {
        files: files.to_vec(),
        dirs,
        idents,
        rare,
        messages,
        by_ext,
    })
}

/// A generated search.
#[derive(Clone, Debug, Default)]
struct Case {
    id: usize,
    family: &'static str,
    dialect: Option<Dialect>,
    patterns: Vec<String>,
    ignore_case: bool,
    word: bool,
    line: bool,
    invert: bool,
    recursive: bool,
    /// Operands as typed: folders, files, wildcards, absolute paths.
    operands: Vec<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    exclude_dir: Vec<String>,
    /// Output flags such as -n, -c, -l, -o, -h, -A2, -m3.
    flags: Vec<String>,
    pipeline: Vec<String>,
    cd: Option<String>,
    command: String,
}

impl Case {
    fn dialect(&self) -> Dialect {
        self.dialect.unwrap_or(Dialect::Basic)
    }

    fn dialect_name(&self) -> &'static str {
        match self.dialect() {
            Dialect::Fixed => "fixed",
            Dialect::Basic => "basic",
            Dialect::Extended => "extended",
            Dialect::Perl => "perl",
            Dialect::Rust => "rust",
        }
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Renders the command line in one of several styles: bundled or separate
/// flags, long options, the pattern before or after the flags.
fn render(c: &mut Case, rng: &mut Rng) {
    let mut bools = String::new();
    if c.recursive {
        bools.push('r');
    }
    match c.dialect() {
        Dialect::Extended => bools.push('E'),
        Dialect::Fixed => bools.push('F'),
        Dialect::Perl => bools.push('P'),
        _ => {}
    }
    for (on, f) in [
        (c.ignore_case, 'i'),
        (c.word, 'w'),
        (c.line, 'x'),
        (c.invert, 'v'),
    ] {
        if on {
            bools.push(f);
        }
    }
    let mut flag_words: Vec<String> = Vec::new();
    match rng.below(4) {
        0 if !bools.is_empty() => flag_words.push(format!("-{bools}")),
        1 => {
            for f in bools.chars() {
                flag_words.push(
                    match f {
                        'r' => "--recursive",
                        'E' => "--extended-regexp",
                        'F' => "--fixed-strings",
                        'P' => "--perl-regexp",
                        'i' => "--ignore-case",
                        'w' => "--word-regexp",
                        'x' => "--line-regexp",
                        _ => "--invert-match",
                    }
                    .to_owned(),
                );
            }
        }
        _ => flag_words.extend(bools.chars().map(|f| format!("-{f}"))),
    }
    flag_words.extend(c.flags.iter().cloned());
    for g in &c.include {
        flag_words.push(format!("--include={}", quote(g)));
    }
    for g in &c.exclude {
        flag_words.push(format!("--exclude={}", quote(g)));
    }
    for g in &c.exclude_dir {
        flag_words.push(format!("--exclude-dir={g}"));
    }
    let explicit = c.patterns.len() > 1 || c.patterns[0].starts_with('-') || rng.chance(15);
    let pattern_words: Vec<String> = if explicit {
        c.patterns
            .iter()
            .flat_map(|p| ["-e".to_owned(), quote(p)])
            .collect()
    } else {
        vec![quote(&c.patterns[0])]
    };
    let mut words: Vec<String> = vec!["grep".into()];
    if !explicit && rng.chance(15) {
        // GNU getopt also takes options after the pattern.
        words.extend(pattern_words);
        words.extend(flag_words);
    } else {
        words.extend(flag_words);
        words.extend(pattern_words);
    }
    words.extend(c.operands.iter().cloned());
    let mut command = words.join(" ");
    for stage in &c.pipeline {
        command.push_str(" | ");
        command.push_str(stage);
    }
    if let Some(cd) = &c.cd {
        command = format!("cd {cd} && {command}");
    }
    c.command = command;
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn generate(corpus: &Corpus, count: usize, seed: u64) -> Vec<Case> {
    let mut rng = Rng(seed);
    let mut cases = Vec::with_capacity(count);
    let c_files = corpus.by_ext.get("c").cloned().unwrap_or_default();
    let h_files = corpus.by_ext.get("h").cloned().unwrap_or_default();
    let top_dirs = strings(&PATHS);
    let common = |rng: &mut Rng| corpus.idents[rng.below(400.min(corpus.idents.len()))].clone();
    let any_ident = |rng: &mut Rng| rng.pick(&corpus.idents).clone();
    let rare = |rng: &mut Rng| rng.pick(&corpus.rare).clone();
    let dir = |rng: &mut Rng| -> String {
        if rng.chance(60) {
            rng.pick(&top_dirs).clone()
        } else {
            rng.pick(&corpus.dirs).clone()
        }
    };
    for id in 0..count {
        let mut c = Case {
            id,
            recursive: true,
            flags: strings(&["-n"]),
            ..Case::default()
        };
        match id % 21 {
            0 => {
                c.family = "fixed identifier";
                c.dialect = Some(Dialect::Fixed);
                c.patterns = vec![any_ident(&mut rng)];
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["head -40"]);
            }
            1 => {
                c.family = "basic identifier, whole tree";
                c.patterns = vec![rare(&mut rng)];
                if rng.chance(50) {
                    c.operands = strings(&["."]);
                }
            }
            2 => {
                c.family = "extended alternation";
                c.dialect = Some(Dialect::Extended);
                let names: Vec<String> =
                    (0..2 + rng.below(3)).map(|_| any_ident(&mut rng)).collect();
                c.patterns = vec![names.join("|")];
                c.operands = vec![dir(&mut rng)];
                c.pipeline = vec![format!("head -{}", 5 + rng.below(40))];
            }
            3 => {
                c.family = "basic alternation";
                let names: Vec<String> =
                    (0..2 + rng.below(3)).map(|_| any_ident(&mut rng)).collect();
                c.patterns = vec![names.join("\\|")];
                c.recursive = false;
                c.operands = vec![rng.pick(&corpus.files).clone()];
            }
            4 => {
                c.family = "errmsg text";
                c.dialect = Some(Dialect::Fixed);
                let m = rng.pick(&corpus.messages).clone();
                let words: Vec<&str> = m.split(' ').collect();
                let start = rng.below(words.len());
                let frag = words[start..(start + 2 + rng.below(3)).min(words.len())].join(" ");
                c.patterns = vec![if frag.len() < 6 { m.clone() } else { frag }];
                c.ignore_case = rng.chance(30);
                c.operands = strings(&["."]);
                c.pipeline = strings(&["head -20"]);
            }
            5 => {
                c.family = "anchors";
                let name = common(&mut rng);
                let choices = [
                    format!("^{name}("),
                    "^Datum$".into(),
                    ";$".into(),
                    "^#define [A-Z_][A-Z_]*".into(),
                    format!("^static .*{name}"),
                    "^}".into(),
                ];
                c.patterns = vec![rng.pick(&choices).clone()];
                c.flags = strings(&["-c"]);
                c.operands = vec![dir(&mut rng)];
            }
            6 => {
                c.family = "classes";
                let choices = [
                    (Dialect::Extended, "[A-Z][a-z]+Expr\\b"),
                    (Dialect::Basic, "[[:upper:]][[:lower:]]*Stmt"),
                    (Dialect::Basic, "[0-9]\\{3,\\}"),
                    (Dialect::Extended, "[[:alpha:]_]+_[[:digit:]]+"),
                    (Dialect::Extended, "T_[A-Z][A-Za-z]*(Expr|Stmt)"),
                    (Dialect::Basic, "[^a-z_]json[^a-z_]"),
                    (Dialect::Extended, "\"[^\"]*%s[^\"]*\""),
                ];
                let (d, p) = rng.pick(&choices);
                c.dialect = Some(*d);
                c.patterns = vec![(*p).to_owned()];
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["head -30"]);
            }
            7 => {
                c.family = "word boundaries";
                let name = common(&mut rng);
                match rng.below(4) {
                    0 => {
                        c.word = true;
                        c.patterns = vec![name];
                    }
                    1 => c.patterns = vec![format!("\\<{name}")],
                    2 => {
                        c.dialect = Some(Dialect::Extended);
                        c.patterns = vec![format!("\\b{name}\\b")];
                    }
                    _ => {
                        c.dialect = Some(Dialect::Perl);
                        c.patterns = vec![format!("\\b{name}\\w*")];
                    }
                }
                c.flags = strings(&["-c"]);
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["grep -v ':0$'"]);
            }
            8 => {
                c.family = "perl";
                c.dialect = Some(Dialect::Perl);
                let choices = [
                    "(?<=make)\\w+Node",
                    "\\d{4,}",
                    "json\\w*_\\w+(?=\\()",
                    "(?i)jsonb?_path\\w*",
                    "\\bPG_RETURN_[A-Z]+\\(",
                    "(?<!\\w)NIL\\b",
                ];
                c.patterns = vec![(*rng.pick(&choices)).to_owned()];
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["head -25"]);
            }
            9 => {
                c.family = "ignore case, counts";
                c.ignore_case = true;
                c.patterns = vec![common(&mut rng).to_uppercase()];
                c.flags = strings(&["-c"]);
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["grep -v ':0$'", "sort -t: -k2 -n", "tail -5"]);
            }
            10 => {
                c.family = "invert";
                c.invert = true;
                c.recursive = false;
                let f = rng.pick(&h_files).clone();
                c.patterns = vec![if rng.chance(50) { "^$" } else { "^\\s*\\*" }.to_owned()];
                c.flags = vec![if rng.chance(50) { "-c" } else { "-n" }.to_owned()];
                c.operands = vec![f];
                c.pipeline = strings(&["head -15"]);
            }
            11 => {
                c.family = "include and exclude";
                c.patterns = vec![common(&mut rng)];
                c.flags = strings(&["-l"]);
                match rng.below(3) {
                    0 => c.include = strings(&["*.h"]),
                    1 => c.exclude = strings(&["*.y", "*.l"]),
                    _ => {
                        c.include = strings(&["*.c"]);
                        c.exclude_dir = strings(&["adt"]);
                    }
                }
                c.operands = vec![if rng.chance(50) { "src" } else { "." }.to_owned()];
            }
            12 => {
                c.family = "context";
                c.recursive = false;
                c.patterns = vec![any_ident(&mut rng)];
                let (f, n) = *rng.pick(&[("-A", 2), ("-B", 1), ("-C", 2), ("-C", 1)]);
                c.flags = vec!["-n".into(), format!("{f}{n}")];
                c.operands = vec![rng.pick(&c_files).clone(), rng.pick(&h_files).clone()];
                c.pipeline = strings(&["head -60"]);
            }
            13 => {
                c.family = "output modes";
                c.patterns = vec![common(&mut rng)];
                let modes = ["-l", "-L", "-c", "-o", "-h", "-m 2", "-ohn", "-l -v"];
                let mode = *rng.pick(&modes);
                if let Some(list) = mode.strip_suffix(" -v") {
                    c.invert = true;
                    c.flags = strings(&[list]);
                } else {
                    c.flags = vec![mode.to_owned()];
                }
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["head -50"]);
            }
            14 => {
                c.family = "pipelines";
                c.patterns = vec![common(&mut rng)];
                let pipes: [&[&str]; 8] = [
                    &["wc -l"],
                    &["cut -d: -f1", "sort", "uniq -c"],
                    &["cut -c1-60", "head -10"],
                    &["grep -v '/\\*'", "head -12"],
                    &["sort", "uniq -c", "sort -rn", "head -5"],
                    &["tail -3"],
                    &["cut -d: -f1", "sort -u"],
                    &["sort -t: -k2,2n", "head -4"],
                ];
                let pipe = *rng.pick(&pipes);
                if pipe.len() == 4 {
                    c.flags = strings(&["-ho"]);
                }
                c.pipeline = strings(pipe);
                c.operands = vec![dir(&mut rng)];
            }
            15 => {
                c.family = "wildcards";
                c.recursive = false;
                let globs = [
                    "src/backend/parser/*.c",
                    "contrib/pg_trgm/*.[ch]",
                    "src/include/nodes/*.h",
                    "src/backend/utils/adt/json*.c",
                    "src/backend/utils/adt/*range*.c",
                ];
                c.patterns = vec![common(&mut rng)];
                c.operands = vec![(*rng.pick(&globs)).to_owned()];
                c.pipeline = strings(&["head -30"]);
            }
            16 => {
                c.family = "absolute paths";
                c.patterns = vec![any_ident(&mut rng)];
                c.operands = vec![format!("{ABS_ROOT}/{}", dir(&mut rng))];
                c.pipeline = strings(&["head -20"]);
            }
            17 => {
                c.family = "cd prefix";
                c.cd = Some("src/backend".into());
                c.patterns = vec![common(&mut rng)];
                c.operands = vec![
                    if rng.chance(50) {
                        "parser"
                    } else {
                        "utils/adt"
                    }
                    .to_owned(),
                ];
                c.pipeline = strings(&["head -10"]);
            }
            18 => {
                c.family = "whole lines";
                c.line = true;
                c.recursive = false;
                let choices = ["{", "}", "Datum", "static void", "\treturn NULL;"];
                c.patterns = vec![(*rng.pick(&choices)).to_owned()];
                c.flags = strings(&["-c"]);
                c.operands = vec![rng.pick(&c_files).clone()];
            }
            19 => {
                c.family = "quantifiers";
                let choices = [
                    (Dialect::Extended, "T_[A-Za-z]{3,8}Stmt"),
                    (Dialect::Basic, "pg_[a-z]*_\\?internal"),
                    (Dialect::Extended, "[a-z]+_[0-9]+"),
                    (Dialect::Basic, "x\\{2\\}"),
                    (Dialect::Extended, "(json|jsonb)_(in|out)\\b"),
                    (Dialect::Basic, "\\(make\\|copy\\)Node"),
                ];
                let (d, p) = rng.pick(&choices);
                c.dialect = Some(*d);
                c.patterns = vec![(*p).to_owned()];
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["head -30"]);
            }
            _ => {
                c.family = "several patterns";
                c.dialect = Some(*rng.pick(&[Dialect::Fixed, Dialect::Basic, Dialect::Extended]));
                c.patterns = (0..2 + rng.below(2)).map(|_| any_ident(&mut rng)).collect();
                c.operands = vec![dir(&mut rng)];
                c.pipeline = strings(&["head -40"]);
            }
        }
        render(&mut c, &mut rng);
        cases.push(c);
    }
    cases
}

type Lines = HashSet<(String, u32)>;

/// Expands a wildcard operand (one wildcard component, the last) like bash.
fn expand(op: &str, files: &[String], dirs: &[String]) -> Vec<String> {
    if !op.contains(['*', '?', '[']) {
        return vec![op.to_owned()];
    }
    let (dir, pat) = op.rsplit_once('/').unwrap_or(("", op));
    let mut out: Vec<String> = files
        .iter()
        .chain(dirs)
        .filter(|p| {
            let (d, name) = p.rsplit_once('/').unwrap_or(("", p));
            d == dir && !name.starts_with('.') && fnmatch(pat, name)
        })
        .cloned()
        .collect();
    out.sort();
    if out.is_empty() {
        vec![op.to_owned()]
    } else {
        out
    }
}

/// The case's patterns in Rust syntax, as the grep tool reads them.
fn rust_patterns(c: &Case) -> Result<Vec<String>> {
    c.patterns
        .iter()
        .map(|p| match c.dialect() {
            Dialect::Basic | Dialect::Extended => {
                gnu_to_rust(p, c.dialect() == Dialect::Extended, 0)
                    .map(|r| r.rust)
                    .map_err(|e| anyhow!(e))
            }
            _ => Ok(p.clone()),
        })
        .collect()
}

/// Repository paths of the operands (after `cd` and expansion); "" is the
/// root.
fn repo_operands(c: &Case, corpus: &Corpus) -> Vec<String> {
    let cwd = c.cd.clone().unwrap_or_default();
    let prefix = format!("{ABS_ROOT}/");
    let ops: Vec<String> = if c.operands.is_empty() {
        vec![".".into()]
    } else {
        c.operands.clone()
    };
    ops.iter()
        .flat_map(|op| {
            let rel = match op.strip_prefix(&prefix) {
                Some(rest) => rest.to_owned(),
                None if cwd.is_empty() => op.clone(),
                None => format!("{cwd}/{op}"),
            };
            let rel = match rel.as_str() {
                "." => String::new(),
                r => r.strip_suffix("/.").unwrap_or(r).to_owned(),
            };
            expand(&rel, &corpus.files, &corpus.dirs)
        })
        .collect()
}

/// ripgrep's lines for the case: the patterns in Rust syntax, -x spelled
/// with anchors.
fn ripgrep(rg: &Path, tree: &Path, c: &Case, corpus: &Corpus) -> Result<Option<Lines>> {
    let d = c.dialect();
    let pats = rust_patterns(c)?;
    let mut globs: Vec<String> = c.include.clone();
    globs.extend(c.exclude.iter().map(|g| format!("!{g}")));
    globs.extend(c.exclude_dir.iter().map(|g| format!("!{g}/")));
    let mut opts = RgOpts {
        fixed: d == Dialect::Fixed && !c.line,
        ignore_case: c.ignore_case,
        word: c.word && !c.line,
        pcre2: d == Dialect::Perl,
        invert: c.invert,
        paths: repo_operands(c, corpus)
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect(),
        globs,
        types: Vec::new(),
    };
    let pats: Vec<String> = if c.line {
        pats.iter()
            .map(|p| {
                let p = if d == Dialect::Fixed {
                    regex::escape(p)
                } else {
                    p.clone()
                };
                format!("^(?:{p})$")
            })
            .collect()
    } else {
        pats
    };
    if opts.paths.is_empty() {
        opts.paths = Vec::new();
    }
    Ok(match rg_lines(rg, tree, &pats, &opts, 2_000_000)? {
        RgLines::Lines(map) => Some(
            map.into_iter()
                .flat_map(|(p, ls)| ls.into_iter().map(move |l| (p.clone(), l)))
                .collect(),
        ),
        _ => None,
    })
}

fn search_args(c: &Case, corpus: &Corpus) -> Result<SearchArgs> {
    let d = c.dialect();
    let mut pats = rust_patterns(c)?;
    let mut mode = if d == Dialect::Fixed {
        MatchMode::Text
    } else {
        MatchMode::Regex
    };
    if c.line {
        if d == Dialect::Fixed {
            pats = pats.iter().map(|p| regex::escape(p)).collect();
            mode = MatchMode::Regex;
        }
        pats = pats.iter().map(|p| format!("^(?:{p})$")).collect();
    }
    let mut args = SearchArgs::new(Strings::Many(pats.clone()));
    args.match_mode = mode;
    args.case_sensitive = Some(!c.ignore_case);
    args.whole_word = c.word && !c.line;
    if c.invert {
        args.query = Strings::One(String::new());
        args.not = Strings::Many(pats);
    }
    let ops = repo_operands(c, corpus);
    if !ops.iter().any(String::is_empty) {
        args.paths = Strings::Many(ops);
    }
    args.include = Strings::Many(c.include.clone());
    let mut exclude = c.exclude.clone();
    exclude.extend(c.exclude_dir.iter().map(|g| format!("{g}/")));
    args.exclude = Strings::Many(exclude);
    args.output = OutputMode::Lines;
    args.limit = Some(pghacker_codesearch::search::MAX_LIMIT);
    args.budget_ms = Some(10_000);
    Ok(args)
}

/// The PG19 subset, fetched from GitHub and served on its branch.
fn spec() -> tree::Spec {
    tree::Spec {
        repo: "postgres".into(),
        source: tree::Source::Remote(URL.into()),
        sha: SHA.into(),
        paths: strings(&PATHS),
        branch: REF.into(),
        schema: SCHEMA.into(),
    }
}

/// Where the subset's files are materialized.
fn tree_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime
        .join("trees")
        .join(format!("postgres-{}", &SHA[..12]))
}

/// The commit's text files, in the order the database sorts paths: the
/// order the corpus has always been read in, so that a seed gives the same
/// searches.
async fn text_files(client: &Client, revision: &Revision) -> Result<Vec<String>> {
    let table = FileTable::load(client, revision).await?;
    let paths: Vec<String> = table
        .files
        .iter()
        .filter(|f| !f.is_binary)
        .map(|f| f.path.clone())
        .collect();
    Ok(client
        .query(
            "SELECT p FROM unnest($1::text[]) AS p ORDER BY p",
            &[&paths],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Every line `search` returns, page by page, and the time it took.
async fn search_lines(
    client: &Client,
    revision: &Revision,
    args: &SearchArgs,
) -> Result<(Lines, f64)> {
    let mut args = args.clone();
    let mut set = HashSet::new();
    let mut ms = None;
    loop {
        let t = Instant::now();
        let r = search(client, revision, &args).await?;
        // The latency of one call: the first page, what an agent waits for.
        ms.get_or_insert(t.elapsed().as_secs_f64() * 1e3);
        if !r.complete {
            bail!("the search was incomplete");
        }
        for h in r.lines {
            set.insert((h.path, h.line));
        }
        if !r.cut {
            if r.total_lines as usize != set.len() {
                bail!(
                    "total_lines {} but {} lines paged",
                    r.total_lines,
                    set.len()
                );
            }
            return Ok((set, ms.unwrap_or(0.0)));
        }
        args.offset += pghacker_codesearch::search::MAX_LIMIT;
    }
}

#[derive(Default, Serialize)]
struct Outcome {
    id: usize,
    family: &'static str,
    dialect: &'static str,
    command: String,
    rg_lines: Option<usize>,
    search: String,
    search_ms: f64,
    grep_lines: String,
    grep_ms: f64,
    gnu: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip)]
    ours: Option<grep_tool::Output>,
}

fn diff(want: &Lines, got: &Lines) -> String {
    if want == got {
        return "exact".into();
    }
    let missing = want.difference(got).count();
    let extra = got.difference(want).count();
    format!("mismatch (missing {missing}, extra {extra})")
}

async fn check(
    pool: &deadpool_postgres::Pool,
    revision: &Revision,
    rg: &Path,
    tree: &Path,
    corpus: &Corpus,
    c: &Case,
) -> Result<Outcome> {
    let mut o = Outcome {
        id: c.id,
        family: c.family,
        dialect: c.dialect_name(),
        command: c.command.clone(),
        ..Outcome::default()
    };
    let truth = ripgrep(rg, tree, c, corpus)?;
    o.rg_lines = truth.as_ref().map(HashSet::len);
    let client = pool.get().await?;
    let client: &Client = &client;
    match (&truth, search_args(c, corpus)) {
        (Some(truth), Ok(args)) => match search_lines(client, revision, &args).await {
            Ok((got, ms)) => {
                o.search = diff(truth, &got);
                o.search_ms = ms;
            }
            Err(e) => o.search = format!("error: {e:#}"),
        },
        (None, _) => o.search = "no ripgrep result".into(),
        (_, Err(e)) => o.search = format!("error: {e:#}"),
    }
    let t = Instant::now();
    match grep_tool::select(client, revision, &c.command, 10_000).await {
        Ok((searched, _)) => {
            o.grep_ms = t.elapsed().as_secs_f64() * 1e3;
            let got: Lines = searched
                .into_iter()
                .flat_map(|s| s.lines.into_iter().map(move |n| (s.path.clone(), n as u32)))
                .collect();
            o.grep_lines = match &truth {
                Some(truth) => diff(truth, &got),
                None => "no ripgrep result".into(),
            };
        }
        Err(e) => o.grep_lines = format!("error: {e:#}"),
    }
    let limits = Limits {
        budget_ms: 10_000,
        max_output: usize::MAX,
    };
    match grep_tool::run(client, revision, &c.command, limits).await {
        Ok(out) => o.ours = Some(out),
        Err(e) => o.gnu = format!("error: {e:#}"),
    }
    Ok(o)
}

pub async fn run(ctx: &Ctx, count: usize, seed: u64, jobs: usize) -> Result<()> {
    let started = Instant::now();
    let reader = db::connect(&ctx.reader_url()?, "pghacker-replay").await?;
    let t = Instant::now();
    let served = tree::serve(&ctx.admin_url()?, &reader, &ctx.runtime, &spec()).await?;
    let load_s = t.elapsed().as_secs_f64();
    let revision = served.revision.clone();
    println!(
        "PG19 subset: {} history in {SCHEMA}, commit {} on {REF}: {} files ({} text, {} binary), {} lines in {load_s:.1}s",
        if served.built { "built" } else { "reused" },
        &revision.sha[..12],
        served.files,
        served.text_files,
        served.binary_files,
        served.lines
    );
    let tree = tree_dir(ctx);
    let (git_dir, commit, tr) = (served.git_dir.clone(), revision.sha.clone(), tree.clone());
    let written =
        tokio::task::spawn_blocking(move || crate::tree::materialize(&git_dir, &commit, &tr))
            .await??;
    println!("materialized {written} files");
    let files = text_files(&reader, &revision).await?;
    let corpus = Arc::new(read_corpus(&tree, &files).context("cannot read the tree")?);
    let cases = generate(&corpus, count, seed);
    println!(
        "generated {} searches (seed {seed}) from {} identifiers and {} messages",
        cases.len(),
        corpus.idents.len(),
        corpus.messages.len()
    );

    let pool = db::pool(&ctx.reader_url()?, jobs, "pghacker-replay")?;
    let rg = Arc::new(ctx.rg.clone());
    let tree_arc = Arc::new(tree.clone());
    let revision_arc = Arc::new(revision.clone());
    let mut outcomes: Vec<Outcome> = stream::iter(cases.iter().cloned().map(|c| {
        let (pool, rg, tree, corpus, revision) = (
            pool.clone(),
            rg.clone(),
            tree_arc.clone(),
            corpus.clone(),
            revision_arc.clone(),
        );
        async move { check(&pool, &revision, &rg, &tree, &corpus, &c).await }
    }))
    .buffer_unordered(jobs)
    .collect::<Vec<Result<Outcome>>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    outcomes.sort_by_key(|o| o.id);

    // GNU grep runs every command line in Docker.
    let gnu_jobs: Vec<gnu::Job> = cases
        .iter()
        .map(|c| gnu::Job {
            tree: "postgres".into(),
            cwd: String::new(),
            command: c.command.clone(),
        })
        .collect();
    let parent = Path::new(ABS_ROOT).parent().unwrap().display();
    let prelude = format!("mkdir -p {parent} && ln -s /t/postgres {ABS_ROOT}\n");
    let t = Instant::now();
    let ran = gnu::run(
        &gnu_jobs,
        &[("postgres".into(), tree.clone())],
        &ctx.runtime.join("gnu-pg19"),
        8,
        &prelude,
    )?;
    println!(
        "GNU grep ran {} commands in {:.0}s",
        ran.len(),
        t.elapsed().as_secs_f64()
    );
    for (o, g) in outcomes.iter_mut().zip(ran) {
        let Some(ours) = o.ours.take() else { continue };
        o.gnu = if ours.stdout == g.stdout && ours.status == g.status {
            "exact".into()
        } else if ours.stdout == g.stdout {
            format!("mismatch (status {} vs {})", ours.status, g.status)
        } else {
            let first = ours
                .stdout
                .iter()
                .zip(&g.stdout)
                .position(|(a, b)| a != b)
                .unwrap_or(ours.stdout.len().min(g.stdout.len()));
            o.detail = Some(format!(
                "ours {} bytes, GNU {} bytes, first difference at byte {first}; GNU stderr: {}",
                ours.stdout.len(),
                g.stdout.len(),
                g.stderr.lines().next().unwrap_or("")
            ));
            "mismatch".into()
        };
    }

    let tally = |pick: &dyn Fn(&Outcome) -> &String| -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for o in &outcomes {
            let v = pick(o);
            let key = v
                .split(' ')
                .next()
                .unwrap_or("")
                .trim_end_matches(':')
                .to_owned();
            *m.entry(key).or_default() += 1;
        }
        m
    };
    let by = |pick: &dyn Fn(&Outcome) -> &String, group: &dyn Fn(&Outcome) -> &'static str| {
        let mut m: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
        for o in &outcomes {
            let e = m.entry(group(o)).or_default();
            e.0 += usize::from(pick(o) == "exact");
            e.1 += 1;
        }
        m.into_iter()
            .map(|(k, (e, n))| (k, format!("{e}/{n}")))
            .collect::<BTreeMap<_, _>>()
    };
    let mut search_ms: Vec<f64> = outcomes
        .iter()
        .map(|o| o.search_ms)
        .filter(|m| *m > 0.0)
        .collect();
    let mut grep_ms: Vec<f64> = outcomes
        .iter()
        .map(|o| o.grep_ms)
        .filter(|m| *m > 0.0)
        .collect();
    search_ms.sort_by(f64::total_cmp);
    grep_ms.sort_by(f64::total_cmp);
    let failures: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.search != "exact" || o.grep_lines != "exact" || o.gnu != "exact")
        .collect();
    let summary = serde_json::json!({
        "revision": {
            "ref": REF, "sha": SHA, "paths": PATHS, "schema": SCHEMA,
            "commit": revision.sha, "files": served.files,
            "text_files": served.text_files, "binary_files": served.binary_files,
            "lines": served.lines, "load_seconds": load_s, "reused": !served.built,
        },
        "searches": outcomes.len(),
        "seed": seed,
        "search_vs_ripgrep": tally(&|o| &o.search),
        "grep_lines_vs_ripgrep": tally(&|o| &o.grep_lines),
        "grep_output_vs_gnu_grep": tally(&|o| &o.gnu),
        "by_family": {
            "search": by(&|o| &o.search, &|o| o.family),
            "grep_lines": by(&|o| &o.grep_lines, &|o| o.family),
            "gnu": by(&|o| &o.gnu, &|o| o.family),
        },
        "by_dialect": {
            "search": by(&|o| &o.search, &|o| o.dialect),
            "grep_lines": by(&|o| &o.grep_lines, &|o| o.dialect),
            "gnu": by(&|o| &o.gnu, &|o| o.dialect),
        },
        "latency_ms": {
            "search": { "p50": percentile(&search_ms, 0.5), "p95": percentile(&search_ms, 0.95) },
            "grep": { "p50": percentile(&grep_ms, 0.5), "p95": percentile(&grep_ms, 0.95) },
        },
        "failures": failures,
        "seconds": started.elapsed().as_secs_f64(),
    });
    let path = ctx.runtime.join("pg19-summary.json");
    std::fs::write(&path, serde_json::to_string_pretty(&summary)?)?;
    let mut brief = summary.clone();
    brief["failures"] = serde_json::json!(failures.len());
    println!("{}", serde_json::to_string_pretty(&brief)?);
    println!("summary: {}", path.display());
    Ok(())
}
