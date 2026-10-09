//! The study replay: every recorded agent search with status `ok` in the
//! study's data/trgm/replay.json, on the commits the study used, through
//! `search` (arguments mapped from the parsed request) and, for grep command
//! lines, through `grep`; each compared with ripgrep's lines computed as the
//! study computed them. With GNU grep in Docker, every grep-path command is
//! also run by GNU grep, so that a difference from ripgrep is classified as
//! GNU grep's own semantics or as an engine error.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use futures_util::{StreamExt, stream};
use pghacker_code::{RepoSource, SnapshotSpec};
use pghacker_codesearch::grep::options::Options;
use pghacker_codesearch::grep::shell::{self, Word};
use pghacker_codesearch::grep::{self as grep_tool, Limits};
use pghacker_codesearch::search::{MatchMode, OutputMode, Strings};
use pghacker_codesearch::{SearchArgs, Snapshot, search};
use pghacker_core::db;
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use crate::gitstats::{self, StudyStats};
use crate::rg::{self, RgLines, RgOpts};
use crate::{Ctx, gnu, percentile, vacuum};

/// The study's time, near which its clones were made.
const STUDY_TIME: &str = "2026-10-09T01:00:00Z";

#[derive(Deserialize)]
struct LoadJson {
    stats: HashMap<String, StudyStats>,
}

#[derive(Deserialize)]
struct ReplayJson {
    rows: Vec<RowIn>,
}

#[derive(Deserialize)]
struct RowIn {
    key: String,
    family: String,
    repo: String,
    agent: String,
    status: String,
}

/// A parsed search request, as the study recorded it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Req {
    pub engine: String,
    pub patterns: Vec<String>,
    pub flavor: String,
    pub ignore_case: bool,
    pub word: bool,
    pub invert: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub types: Vec<String>,
}

struct Case {
    index: usize,
    repo: String,
    rel: Vec<String>,
    rel_globs: Vec<String>,
    req: Req,
    family: String,
    agent: String,
}

/// grep's basic and extended regex as ripgrep syntax, exactly as the study
/// rewrote them for its ground truth.
pub fn study_grep_to_rust(p: &str, flavor: &str) -> String {
    if flavor != "bre" {
        return p.replace("\\<", "\\b").replace("\\>", "\\b");
    }
    let c: Vec<char> = p.chars().collect();
    let n = c.len();
    let mut out = String::new();
    let mut at_start = true;
    let mut i = 0;
    while i < n {
        let ch = c[i];
        if ch == '\\' && i + 1 < n {
            let d = c[i + 1];
            i += 2;
            if d == '(' || d == '|' {
                out.push(d);
                at_start = true;
                continue;
            }
            if "){}+?".contains(d) {
                out.push(d);
            } else if d == '<' || d == '>' {
                out.push_str("\\b");
            } else {
                out.push(ch);
                out.push(d);
            }
            at_start = false;
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
            while j < n && c[j] != ']' {
                if c[j] == '['
                    && let Some(&k) = c.get(j + 1).filter(|k| ":.=".contains(**k))
                {
                    let close =
                        (j + 2..n.saturating_sub(1)).find(|&e| c[e] == k && c[e + 1] == ']');
                    if let Some(e) = close {
                        j = e + 1;
                    }
                }
                j += 1;
            }
            out.extend(&c[i..(j + 1).min(n)]);
            i = j + 1;
            at_start = false;
            continue;
        }
        if ch == '^' {
            out.push_str(if at_start { "^" } else { "\\^" });
            i += 1;
            continue;
        }
        if ch == '$' {
            let rest: String = c[i + 1..].iter().collect();
            let end = i == n - 1 || rest.starts_with("\\)") || rest.starts_with("\\|");
            out.push_str(if end { "$" } else { "\\$" });
            at_start = false;
            i += 1;
            continue;
        }
        if ch == '*' && at_start {
            out.push_str("\\*");
            at_start = false;
            i += 1;
            continue;
        }
        if "(){}|+?".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
        at_start = false;
        i += 1;
    }
    out
}

/// ripgrep file types the study's requests use, as globs.
fn type_globs(t: &str) -> Vec<String> {
    let globs: &[&str] = match t {
        "rust" => &["*.rs"],
        "py" => &["*.py", "*.pyi"],
        "ts" => &["*.ts", "*.tsx", "*.cts", "*.mts"],
        "js" => &["*.js", "*.jsx", "*.cjs", "*.mjs", "*.vue"],
        "go" => &["*.go"],
        "md" => &[
            "*.markdown",
            "*.md",
            "*.mdown",
            "*.mdwn",
            "*.mkd",
            "*.mkdn",
            "*.mdx",
        ],
        "json" => &["*.json", "*.sarif", "composer.lock"],
        "sql" => &["*.psql", "*.sql"],
        "sh" => &["*.bash", "*.sh", "*.zsh"],
        _ => &[],
    };
    globs.iter().map(|g| (*g).to_owned()).collect()
}

/// The search arguments for a recorded request.
fn search_args(case: &Case) -> SearchArgs {
    let req = &case.req;
    let pats: Vec<String> = req
        .patterns
        .iter()
        .flat_map(|p| p.split('\n').map(str::to_owned))
        .collect();
    let (mode, query) = match req.flavor.as_str() {
        "fixed" => (MatchMode::Text, pats),
        "bre" | "ere" => (
            MatchMode::Regex,
            pats.iter()
                .map(|p| study_grep_to_rust(p, &req.flavor))
                .collect(),
        ),
        _ => (MatchMode::Regex, pats),
    };
    let mut args = SearchArgs::new(Strings::Many(query.clone()));
    args.match_mode = mode;
    args.case_sensitive = Some(!req.ignore_case);
    args.whole_word = req.word;
    if req.invert {
        args.query = Strings::One(String::new());
        args.not = Strings::Many(query);
    }
    let root = case.rel.iter().any(String::is_empty);
    if !root {
        let mut paths: Vec<String> = case.rel.clone();
        // Shell wildcards: bash without globstar reads ** as *.
        paths.extend(case.rel_globs.iter().map(|g| g.replace("**", "*")));
        args.paths = Strings::Many(paths);
    }
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for g in &req.include {
        match g.strip_prefix('!') {
            Some(rest) => exclude.push(rest.to_owned()),
            None => include.push(g.clone()),
        }
    }
    for t in &req.types {
        include.extend(type_globs(t));
    }
    exclude.extend(
        req.exclude
            .iter()
            .filter(|e| !e.starts_with("type:"))
            .cloned(),
    );
    args.include = Strings::Many(include);
    args.exclude = Strings::Many(exclude);
    args.output = OutputMode::Lines;
    args.limit = Some(pghacker_codesearch::search::MAX_LIMIT);
    args.budget_ms = Some(10_000);
    args
}

type Lines = HashSet<(String, u32)>;

/// Ground truth for a case.
enum Truth {
    Lines(Lines),
    NoScope,
    Error,
    TooLarge,
}

fn ground_truth(
    rg_bin: &Path,
    tree: &Path,
    tracked: &HashSet<String>,
    case: &Case,
) -> Result<Truth> {
    let req = &case.req;
    let mut globs = req.include.clone();
    globs.extend(
        req.exclude
            .iter()
            .filter(|e| !e.starts_with("type:"))
            .map(|e| format!("!{e}")),
    );
    let Some(scope) = rg::scope_of(
        rg_bin,
        tree,
        tracked,
        &case.rel,
        &case.rel_globs,
        &globs,
        &req.types,
    )?
    else {
        return Ok(Truth::NoScope);
    };
    let pats: Vec<String> = req
        .patterns
        .iter()
        .flat_map(|p| p.split('\n').map(str::to_owned))
        .collect();
    let pats = if req.flavor == "fixed" {
        pats
    } else {
        pats.iter()
            .map(|p| study_grep_to_rust(p, &req.flavor))
            .collect()
    };
    let opts = RgOpts {
        fixed: req.flavor == "fixed",
        ignore_case: req.ignore_case,
        word: req.word,
        pcre2: req.flavor == "pcre",
        invert: req.invert,
        paths: scope.rg_paths.clone(),
        globs,
        types: req.types.clone(),
    };
    let lines = match rg::rg_lines(rg_bin, tree, &pats, &opts, 1_000_000)? {
        RgLines::Lines(l) => l,
        RgLines::Error => return Ok(Truth::Error),
        RgLines::TooLarge => return Ok(Truth::TooLarge),
    };
    let in_scope: HashSet<&String> = scope.files.iter().collect();
    let mut set = HashSet::new();
    for (path, nums) in lines {
        if in_scope.contains(&path) {
            for n in nums {
                set.insert((path.clone(), n));
            }
        }
    }
    Ok(Truth::Lines(set))
}

/// Every line `search` returns for the arguments, page by page: the
/// lines, whether every page was complete, and whether the totals agree
/// with the lines paged through.
async fn search_lines(
    client: &Client,
    snapshot: &Snapshot,
    args: &SearchArgs,
) -> Result<(Lines, bool, bool, f64)> {
    let mut args = args.clone();
    let mut set = HashSet::new();
    let mut complete = true;
    let mut first_ms = None;
    loop {
        let t = Instant::now();
        let r = search(client, snapshot, &args).await?;
        first_ms.get_or_insert(t.elapsed().as_secs_f64() * 1e3);
        complete &= r.complete && !r.budget_exceeded;
        for h in r.lines {
            set.insert((h.path, h.line));
        }
        if !r.cut {
            let totals_agree = r.total_lines as usize == set.len();
            return Ok((set, complete, totals_agree, first_ms.unwrap_or(0.0)));
        }
        args.offset += pghacker_codesearch::search::MAX_LIMIT;
    }
}

/// Why a grep command line is not replayed.
fn grep_exclusion(case: &Case) -> Option<&'static str> {
    if case.req.engine != "grep" || !case.agent.trim_start().starts_with("grep ") {
        return Some("not a grep command line");
    }
    if case.agent.chars().count() >= 240 {
        return Some("the recorded command is cut at 240 characters");
    }
    const MARKS: [&str; 7] = [
        "<email>",
        "<secret>",
        "<redacted>",
        "<guid>",
        "<name>",
        "<ip>",
        "<blob>",
    ];
    if MARKS.iter().any(|m| case.agent.contains(m)) {
        return Some("the recorded command is redacted");
    }
    None
}

/// A word re-quoted for bash: quoted parts in single quotes, unquoted parts
/// with their wildcards and braces left active.
fn quote_word(w: &Word) -> String {
    let mut out = String::new();
    for p in &w.parts {
        if p.quoted {
            out.push('\'');
            out.push_str(&p.text.replace('\'', "'\\''"));
            out.push('\'');
        } else {
            for c in p.text.chars() {
                if " \t\"'$`\\|&;<>()#!".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    if out.is_empty() {
        out.push_str("''");
    }
    out
}

fn plain_word(text: &str, wildcards: bool) -> Word {
    let mut w = Word::default();
    w.parts.push(shell::Part {
        text: text.to_owned(),
        quoted: !wildcards,
    });
    w
}

/// Indices of a grep command's operand words (the pattern excluded).
fn operand_indices(words: &[Word]) -> Vec<usize> {
    let texts: Vec<String> = words.iter().map(Word::text).collect();
    let mut operands = Vec::new();
    let mut explicit_pattern = false;
    let mut i = 1;
    let mut done = false;
    while i < texts.len() {
        let t = &texts[i];
        if !done && t == "--" {
            done = true;
            i += 1;
            continue;
        }
        if !done && t.len() > 1 && t.starts_with('-') {
            if let Some(long) = t.strip_prefix("--") {
                let name = long.split('=').next().unwrap_or("");
                let takes = [
                    "regexp",
                    "file",
                    "include",
                    "exclude",
                    "exclude-dir",
                    "exclude-from",
                    "after-context",
                    "before-context",
                    "context",
                    "max-count",
                    "directories",
                    "devices",
                    "binary-files",
                    "label",
                    "group-separator",
                ];
                if name == "regexp" || name == "file" {
                    explicit_pattern = true;
                }
                if !long.contains('=')
                    && takes
                        .iter()
                        .any(|n| n.starts_with(name) && !name.is_empty())
                {
                    i += 1;
                }
            } else {
                let chars: Vec<char> = t[1..].chars().collect();
                for (k, c) in chars.iter().enumerate() {
                    if "ABCDXdefm".contains(*c) {
                        if *c == 'e' || *c == 'f' {
                            explicit_pattern = true;
                        }
                        if k == chars.len() - 1 {
                            i += 1;
                        }
                        break;
                    }
                }
            }
            i += 1;
            continue;
        }
        operands.push(i);
        i += 1;
    }
    if !explicit_pattern && !operands.is_empty() {
        operands.remove(0);
    }
    operands
}

/// The command after the last `&&` outside quotes: the recorder parsed
/// `a && b | c` as `(a && b) | c` and kept the whole text, with `b`'s request.
fn last_command(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut single, mut double) = (false, false);
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if !single => i += 1,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'&' if !single && !double && b.get(i + 1) == Some(&b'&') => {
                start = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    &line[start..]
}

/// The grep command line to replay: the first stage only, absolute paths
/// mapped to the repository, and a `cd` to the folder the agent ran it in.
fn grep_command(case: &Case, dirs: &HashSet<String>) -> std::result::Result<String, &'static str> {
    let script = shell::parse(last_command(case.agent.trim()).trim()).map_err(|e| {
        if e.contains("variables") {
            "excluded: the command uses a shell variable"
        } else if e.contains("substitution") {
            "excluded: the command uses command substitution"
        } else if e.contains("one grep pipeline") {
            "excluded: the command chains several commands"
        } else {
            "the command does not parse"
        }
    })?;
    let mut words = script.stages[0].words.clone();
    let named: Vec<String> = case.rel.iter().chain(&case.rel_globs).cloned().collect();
    let known = |r: &str| named.iter().any(|n| n == r);
    let mut relative = Vec::new();
    for k in operand_indices(&words) {
        let w = &words[k];
        let text = w.text();
        let wild = w.has_wildcards();
        if text.starts_with('/') || text.starts_with('~') {
            let comps: Vec<&str> = text
                .split('/')
                .filter(|c| !c.is_empty() && *c != ".")
                .collect();
            let mapped = (0..=comps.len()).find_map(|j| {
                let rest = comps[j..].join("/");
                known(&rest).then_some(rest)
            });
            if let Some(rest) = mapped {
                let rest = if rest.is_empty() {
                    ".".to_owned()
                } else {
                    rest
                };
                words[k] = plain_word(&rest, wild);
            }
        } else {
            relative.push(text);
        }
    }
    // The folder the agent was in: one that puts every relative operand on
    // a recorded path.
    let candidates = |op: &str| -> HashSet<String> {
        let mut op = op.trim_start_matches("./").trim_end_matches('/');
        let mut up = 0;
        while let Some(rest) = op.strip_prefix("../") {
            op = rest;
            up += 1;
        }
        let op = if op == "." || op == ".." { "" } else { op };
        let parents: HashSet<String> = named
            .iter()
            .filter_map(|n| {
                if op.is_empty() {
                    Some(n.clone())
                } else if n == op {
                    Some(String::new())
                } else {
                    n.strip_suffix(op)
                        .and_then(|p| p.strip_suffix('/'))
                        .map(str::to_owned)
                }
            })
            .collect();
        if up == 0 {
            return parents;
        }
        // `../x`: any folder `up` levels below the parent will do.
        dirs.iter()
            .filter(|d| {
                let comps: Vec<&str> = d.split('/').collect();
                comps.len() >= up && parents.contains(&comps[..comps.len() - up].join("/"))
            })
            .cloned()
            .collect()
    };
    let opts = Options::parse(&words[1..].iter().map(Word::text).collect::<Vec<_>>())
        .map_err(|_| "grep rejects the options")?;
    let cwd = if relative.is_empty() {
        if opts.operands.is_empty() && case.rel.len() == 1 && case.rel_globs.is_empty() {
            case.rel[0].clone()
        } else {
            String::new()
        }
    } else {
        let mut common: Option<HashSet<String>> = None;
        for op in &relative {
            let c = candidates(op);
            common = Some(match common {
                None => c,
                Some(prev) => prev.intersection(&c).cloned().collect(),
            });
        }
        let common = common.unwrap_or_default();
        if common.contains("") {
            String::new()
        } else {
            let mut sorted: Vec<String> = common.into_iter().collect();
            sorted.sort();
            sorted.into_iter().next().ok_or(if relative.iter().any(|r| r.contains("..")) {
                "excluded: a path climbs out of the folder the agent was in, which was not recorded"
            } else {
                "the agent's folder is unknown"
            })?
        }
    };
    // Operands outside the repository were not in the study's scope.
    for op in &relative {
        let mut depth: i64 = cwd.split('/').filter(|c| !c.is_empty()).count() as i64;
        for comp in op.split('/') {
            match comp {
                "" | "." => {}
                ".." => depth -= 1,
                _ => depth += 1,
            }
            if depth < 0 {
                return Err("excluded: an operand is outside the repository");
            }
        }
    }
    let body: Vec<String> = words.iter().map(quote_word).collect();
    let mut command = body.join(" ");
    if !cwd.is_empty() {
        command = format!("cd {} && {command}", quote_word(&plain_word(&cwd, false)));
    }
    Ok(command)
}

#[derive(Default, Serialize)]
struct PathResult {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    missing: usize,
    extra: usize,
    ms: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing_samples: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extra_samples: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gnu: Option<String>,
}

#[derive(Serialize)]
struct Record {
    index: usize,
    repo: String,
    family: String,
    flavor: String,
    engine: String,
    truth: String,
    truth_lines: Option<usize>,
    search: PathResult,
    grep: PathResult,
    #[serde(skip)]
    grep_output: Option<Vec<u8>>,
}

fn compare(truth: &Lines, got: &Lines, r: &mut PathResult) {
    let mut missing: Vec<&(String, u32)> = truth.difference(got).collect();
    let mut extra: Vec<&(String, u32)> = got.difference(truth).collect();
    missing.sort();
    extra.sort();
    r.missing = missing.len();
    r.extra = extra.len();
    r.missing_samples = missing
        .iter()
        .take(3)
        .map(|(p, l)| format!("{p}:{l}"))
        .collect();
    r.extra_samples = extra
        .iter()
        .take(3)
        .map(|(p, l)| format!("{p}:{l}"))
        .collect();
    r.status = if missing.is_empty() && extra.is_empty() {
        "exact".into()
    } else {
        "mismatch".into()
    };
}

struct Repo {
    snapshot: Snapshot,
    tree: PathBuf,
    tracked: HashSet<String>,
    /// Folders of tracked files.
    dirs: HashSet<String>,
}

/// The environment variable that names the clone of the study's repository
/// `name`: `PGHACKER_REPLAY_` and the name in upper-case letters, digits and
/// underscores, such as `PGHACKER_REPLAY_SCENARIOS` for `scenarios`.
fn repo_var(name: &str) -> String {
    let suffix: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("PGHACKER_REPLAY_{suffix}")
}

/// Serves each repository that the study loaded (load.json's statistics),
/// from the clone that its environment variable names ([`repo_var`]), at the
/// commit that has the study's statistics.
async fn load_repos(ctx: &Ctx, study_dir: &Path) -> Result<HashMap<String, Arc<Repo>>> {
    let load: LoadJson = serde_json::from_str(
        &std::fs::read_to_string(study_dir.join("data/trgm/load.json"))
            .context("cannot read load.json")?,
    )?;
    let around: chrono::DateTime<chrono::Utc> = STUDY_TIME.parse()?;
    let mut names: Vec<&String> = load.stats.keys().collect();
    names.sort();
    let mut dirs = Vec::new();
    for name in names {
        let var = repo_var(name);
        let dir =
            ctx.env.get(&var).map(PathBuf::from).ok_or_else(|| {
                anyhow!("set {var} to the clone of the study's repository {name}")
            })?;
        dirs.push((name, dir));
    }
    let mut indexer = db::connect(&ctx.indexer_url()?, "pghacker-replay").await?;
    let mut repos = HashMap::new();
    for (name, dir) in dirs {
        let want = load.stats[name];
        let started = Instant::now();
        let d = dir.clone();
        let sha =
            tokio::task::spawn_blocking(move || gitstats::find_commit(&d, &want, around)).await??;
        println!(
            "{name}: commit {} has the study's statistics ({} files, {} lines; found in {:.1}s)",
            &sha[..12],
            want.files,
            want.lines,
            started.elapsed().as_secs_f64()
        );
        let spec = SnapshotSpec {
            repo: name.to_owned(),
            git_ref: "main".into(),
            sha: sha.clone(),
            version: None,
            paths: Vec::new(),
            source: RepoSource::Local { path: dir.clone() },
            source_id: None,
        };
        let started = Instant::now();
        let mut stats = pghacker_code::load_snapshot(&mut indexer, &spec).await?;
        crate::count_reused(&indexer, &mut stats).await?;
        println!(
            "{name}: {} snapshot {}: {} files, {} text, {} binary, {} lines in {:.1}s",
            if stats.reused { "reused" } else { "loaded" },
            stats.snapshot_id,
            stats.files,
            stats.text_files,
            stats.binary_files,
            stats.lines,
            started.elapsed().as_secs_f64()
        );
        let tree = ctx
            .runtime
            .join("trees")
            .join(format!("{name}-{}", &sha[..12]));
        let started = Instant::now();
        let (s, t) = (spec.clone(), tree.clone());
        let written =
            tokio::task::spawn_blocking(move || pghacker_code::materialize(&s, &t)).await??;
        println!(
            "{name}: materialized {written} files in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["ls-tree", "-r", "-z", "--name-only", &sha])
            .output()?;
        let tracked: HashSet<String> = out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        let mut dirs = HashSet::new();
        for t in &tracked {
            let mut end = 0;
            while let Some(k) = t[end..].find('/') {
                end += k;
                dirs.insert(t[..end].to_owned());
                end += 1;
            }
        }
        repos.insert(
            name.to_owned(),
            Arc::new(Repo {
                snapshot: Snapshot {
                    id: stats.snapshot_id,
                    repo: name.to_owned(),
                    version: None,
                    git_ref: "main".into(),
                    sha,
                },
                tree,
                tracked,
                dirs,
            }),
        );
    }
    let started = Instant::now();
    vacuum(ctx).await?;
    println!("vacuumed in {:.1}s", started.elapsed().as_secs_f64());
    Ok(repos)
}

fn read_cases(study_dir: &Path, limit: Option<usize>) -> Result<Vec<Case>> {
    let replay: ReplayJson = serde_json::from_str(
        &std::fs::read_to_string(study_dir.join("data/trgm/replay.json"))
            .context("cannot read replay.json")?,
    )?;
    let mut cases = Vec::new();
    for (index, row) in replay.rows.into_iter().enumerate() {
        if row.status != "ok" {
            continue;
        }
        let key: serde_json::Value = serde_json::from_str(&row.key)?;
        let strings = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        cases.push(Case {
            index,
            repo: row.repo,
            rel: strings(&key[1]),
            rel_globs: strings(&key[2]),
            req: serde_json::from_value(key[3].clone())?,
            family: row.family,
            agent: row.agent,
        });
    }
    if let Some(n) = limit {
        cases.truncate(n);
    }
    Ok(cases)
}

/// GNU grep's reading of a grep-path difference, for the summary.
fn gnu_reason(case: &Case, command: &str) -> String {
    let words = shell::parse(command)
        .map(|s| s.stages[0].words.iter().map(Word::text).collect::<Vec<_>>())
        .unwrap_or_default();
    let opts = Options::parse(&words[1.min(words.len())..]).ok();
    let flags: String = words
        .iter()
        .filter(|w| w.starts_with('-') && !w.starts_with("--"))
        .map(|w| w[1..].to_owned())
        .collect();
    let pats = case.req.patterns.join("\n");
    if let Some(o) = &opts
        && o.patterns.join("\n") != pats
    {
        return "the study's shell parser recorded other patterns than the command's \
                (a $ or a tab inside double quotes was lost)"
            .into();
    }
    if case.req.flavor == "ere"
        && !flags.contains('E')
        && !words.iter().any(|w| w == "--extended-regexp")
    {
        return "a bare | in a basic regex: GNU grep reads a literal |, the study alternation"
            .into();
    }
    if let Some(o) = &opts {
        if o.line {
            return "-x: the study's request does not record it".into();
        }
        if o.directories != pghacker_codesearch::grep::options::Directories::Recurse {
            return "a folder operand without -r: GNU grep skips it, ripgrep searches it".into();
        }
        if !o.files.is_empty() || !o.exclude_dir.is_empty() {
            return "--include/--exclude/--exclude-dir: GNU grep matches base names, ripgrep's -g globs paths".into();
        }
    }
    if case.req.flavor != "fixed" && regex::Regex::new(r"\\[dDtnrfvxu]").unwrap().is_match(&pats) {
        return "an escape such as \\d or \\t: GNU grep reads the letter, ripgrep the class".into();
    }
    if case.req.flavor != "fixed" && (pats.contains("\\<") || pats.contains("\\>")) {
        return "\\< or \\>: a word start or end in GNU grep, \\b in the study".into();
    }
    "another GNU grep reading".into()
}

pub async fn run(
    ctx: &Ctx,
    study_dir: &Path,
    limit: Option<usize>,
    jobs: usize,
    skip_gnu: bool,
) -> Result<()> {
    let started = Instant::now();
    let repos = load_repos(ctx, study_dir).await?;
    let cases = read_cases(study_dir, limit)?;
    println!("replaying {} searches with {jobs} at a time", cases.len());
    let pool = db::pool(&ctx.reader_url()?, jobs, "pghacker-replay")?;
    let rg_bin = Arc::new(ctx.rg.clone());
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = cases.len();
    let mut records: Vec<(Record, Option<String>)> = stream::iter(cases.into_iter().map(|case| {
        let pool = pool.clone();
        let repo = repos.get(&case.repo).cloned();
        let rg_bin = rg_bin.clone();
        let done = done.clone();
        async move {
            let r = replay_case(&pool, repo, &rg_bin, &case).await;
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_multiple_of(1000) {
                eprintln!("  {n}/{total}");
            }
            r
        }
    }))
    .buffer_unordered(jobs)
    .collect::<Vec<Result<(Record, Option<String>)>>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    records.sort_by_key(|(r, _)| r.index);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = ctx.runtime.join(format!("study-{stamp}.jsonl"));
    write_records(&path, &records)?;

    // GNU grep's own output for every replayed grep command line.
    if !skip_gnu {
        let mut jobs_gnu = Vec::new();
        let mut which = Vec::new();
        for (k, (rec, command)) in records.iter().enumerate() {
            if let (Some(command), true) = (command, rec.grep_output.is_some()) {
                jobs_gnu.push(gnu::Job {
                    tree: rec.repo.clone(),
                    cwd: String::new(),
                    command: command.clone(),
                });
                which.push(k);
            }
        }
        let trees: Vec<(String, PathBuf)> = repos
            .iter()
            .map(|(n, r)| (n.clone(), r.tree.clone()))
            .collect();
        let gnu_started = Instant::now();
        let ran = gnu::run(&jobs_gnu, &trees, &ctx.runtime.join("gnu-study"), 8, "")?;
        println!(
            "GNU grep ran {} commands in {:.0}s",
            jobs_gnu.len(),
            gnu_started.elapsed().as_secs_f64()
        );
        let cases_by_index: HashMap<usize, Case> = read_cases(study_dir, limit)?
            .into_iter()
            .map(|c| (c.index, c))
            .collect();
        for (k, ran) in which.into_iter().zip(ran) {
            let (rec, command) = &mut records[k];
            let same = rec.grep_output.as_deref() == Some(ran.stdout.as_slice());
            rec.grep.gnu = Some(
                if same {
                    "same output"
                } else {
                    "different output"
                }
                .into(),
            );
            if rec.grep.status == "mismatch" && same {
                rec.grep.status = "excluded".into();
                let case = &cases_by_index[&rec.index];
                rec.grep.reason = Some(format!(
                    "GNU grep semantics: {}",
                    gnu_reason(case, command.as_deref().unwrap_or(""))
                ));
            }
        }
    }

    // Details to .runtime/replay; aggregates to the console.
    write_records(&path, &records)?;
    summarize(&records.iter().map(|(r, _)| r).collect::<Vec<_>>());
    println!("details: {}", path.display());
    println!("done in {:.0}s", started.elapsed().as_secs_f64());
    Ok(())
}

fn write_records(path: &Path, records: &[(Record, Option<String>)]) -> Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    for (rec, _) in records {
        serde_json::to_writer(&mut file, rec)?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    Ok(())
}

async fn replay_case(
    pool: &deadpool_postgres::Pool,
    repo: Option<Arc<Repo>>,
    rg_bin: &Arc<PathBuf>,
    case: &Case,
) -> Result<(Record, Option<String>)> {
    let mut rec = Record {
        index: case.index,
        repo: case.repo.clone(),
        family: case.family.clone(),
        flavor: case.req.flavor.clone(),
        engine: case.req.engine.clone(),
        truth: String::new(),
        truth_lines: None,
        search: PathResult::default(),
        grep: PathResult::default(),
        grep_output: None,
    };
    let Some(repo) = repo else {
        rec.truth = "unknown repository".into();
        return Ok((rec, None));
    };
    let (rg_bin2, repo2, case_req) = (
        rg_bin.clone(),
        repo.clone(),
        Case {
            index: case.index,
            repo: case.repo.clone(),
            rel: case.rel.clone(),
            rel_globs: case.rel_globs.clone(),
            req: case.req.clone(),
            family: String::new(),
            agent: String::new(),
        },
    );
    let truth = tokio::task::spawn_blocking(move || {
        ground_truth(&rg_bin2, &repo2.tree, &repo2.tracked, &case_req)
    })
    .await??;
    let truth = match truth {
        Truth::Lines(l) => l,
        Truth::NoScope => {
            rec.truth = "no scope".into();
            return Ok((rec, None));
        }
        Truth::Error => {
            rec.truth = "ripgrep error".into();
            return Ok((rec, None));
        }
        Truth::TooLarge => {
            rec.truth = "too large".into();
            return Ok((rec, None));
        }
    };
    rec.truth = "ok".into();
    rec.truth_lines = Some(truth.len());
    let client = pool
        .get()
        .await
        .context("cannot get a database connection")?;
    let client: &Client = &client;

    let args = search_args(case);
    match search_lines(client, &repo.snapshot, &args).await {
        Ok((got, complete, totals_agree, first_ms)) => {
            rec.search.ms = first_ms;
            compare(&truth, &got, &mut rec.search);
            if !complete && rec.search.status == "mismatch" {
                rec.search.reason = Some("incomplete: time budget or candidate cap".into());
            } else if !totals_agree {
                rec.search.status = "mismatch".into();
                rec.search.reason = Some("total_lines disagrees with the lines paged".into());
            }
        }
        Err(error) => {
            rec.search.status = "error".into();
            rec.search.reason = Some(format!("{error:#}"));
        }
    }

    let mut command_out = None;
    if let Some(reason) = grep_exclusion(case) {
        rec.grep.status = if reason == "not a grep command line" {
            "n/a".into()
        } else {
            "excluded".into()
        };
        rec.grep.reason = Some(reason.into());
        return Ok((rec, None));
    }
    match grep_command(case, &repo.dirs) {
        Err(reason) => match reason.strip_prefix("excluded: ") {
            Some(why) => {
                rec.grep.status = "excluded".into();
                rec.grep.reason = Some(why.into());
            }
            None => {
                rec.grep.status = "error".into();
                rec.grep.reason = Some(reason.into());
            }
        },
        Ok(command) => {
            rec.grep.command = Some(command.clone());
            let t = Instant::now();
            match grep_tool::select(client, &repo.snapshot, &command, 10_000).await {
                Ok((searched, incomplete)) => {
                    rec.grep.ms = t.elapsed().as_secs_f64() * 1e3;
                    let got: Lines = searched
                        .into_iter()
                        .flat_map(|s| s.lines.into_iter().map(move |n| (s.path.clone(), n as u32)))
                        .collect();
                    compare(&truth, &got, &mut rec.grep);
                    if incomplete && rec.grep.status == "mismatch" {
                        rec.grep.reason = Some("incomplete: time budget or candidate cap".into());
                    }
                    let limits = Limits {
                        budget_ms: 10_000,
                        max_output: usize::MAX,
                    };
                    if let Ok(out) = grep_tool::run(client, &repo.snapshot, &command, limits).await
                    {
                        rec.grep_output = Some(out.stdout);
                    }
                    command_out = Some(command);
                }
                Err(error) => {
                    rec.grep.status = "error".into();
                    rec.grep.reason = Some(format!("{error:#}"));
                }
            }
        }
    }
    Ok((rec, command_out))
}

fn summarize(records: &[&Record]) {
    let compared: Vec<&&Record> = records.iter().filter(|r| r.truth == "ok").collect();
    let mut truth_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in records {
        *truth_counts.entry(r.truth.as_str()).or_default() += 1;
    }
    println!(
        "\nsearches: {}; ground truth: {truth_counts:?}",
        records.len()
    );
    for (name, pick) in [
        (
            "search",
            (|r: &Record| &r.search) as fn(&Record) -> &PathResult,
        ),
        ("grep", |r: &Record| &r.grep),
    ] {
        let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_flavor: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut by_family: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        let mut ms: Vec<f64> = Vec::new();
        for r in &compared {
            let p = pick(r);
            *by_status.entry(p.status.clone()).or_default() += 1;
            if p.status == "exact" || p.status == "mismatch" {
                let e = by_flavor.entry(r.flavor.clone()).or_default();
                e.0 += usize::from(p.status == "exact");
                e.1 += 1;
                let e = by_family.entry(r.family.clone()).or_default();
                e.0 += usize::from(p.status == "exact");
                e.1 += 1;
                ms.push(p.ms);
            }
            if p.status != "exact" && p.status != "n/a" {
                let reason = p.reason.clone().unwrap_or_else(|| "unexplained".into());
                *reasons
                    .entry(format!("{}: {reason}", p.status))
                    .or_default() += 1;
            }
        }
        ms.sort_by(f64::total_cmp);
        let exact = by_status.get("exact").copied().unwrap_or(0);
        let compared_n: usize = by_flavor.values().map(|v| v.1).sum();
        println!("\n{name}: exact {exact} of {compared_n} compared; {by_status:?}");
        println!(
            "  latency p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
            percentile(&ms, 0.5),
            percentile(&ms, 0.95),
            ms.last().copied().unwrap_or(0.0)
        );
        let fmt = |m: &BTreeMap<String, (usize, usize)>| {
            m.iter()
                .map(|(k, (e, n))| format!("{k} {e}/{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("  by dialect: {}", fmt(&by_flavor));
        println!("  by family: {}", fmt(&by_family));
        for (reason, n) in &reasons {
            println!("  {n:>5}  {reason}");
        }
        if name == "grep" {
            let mut gnu: BTreeMap<String, usize> = BTreeMap::new();
            for r in &compared {
                if let Some(g) = &r.grep.gnu {
                    *gnu.entry(format!("{} ({})", g, r.grep.status)).or_default() += 1;
                }
            }
            if !gnu.is_empty() {
                println!("  GNU grep check: {gnu:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn study_rewrites_basic_regexes() {
        assert_eq!(study_grep_to_rust(r"a\|b", "bre"), "a|b");
        assert_eq!(study_grep_to_rust(r"^x(y)+$", "bre"), r"^x\(y\)\+$");
        assert_eq!(study_grep_to_rust(r"\(a\)*", "bre"), "(a)*");
        assert_eq!(study_grep_to_rust("*a^b", "bre"), r"\*a\^b");
        assert_eq!(study_grep_to_rust(r"\<w\>", "ere"), r"\bw\b");
        assert_eq!(study_grep_to_rust("[]a]x", "bre"), "[]a]x");
    }

    #[test]
    fn operands_skip_the_pattern_and_option_values() {
        let s = shell::parse("grep -rn -A 2 --include '*.c' pat src a.c").unwrap();
        let words = &s.stages[0].words;
        let ops: Vec<String> = operand_indices(words)
            .into_iter()
            .map(|k| words[k].text())
            .collect();
        assert_eq!(ops, vec!["src", "a.c"]);
        let s = shell::parse("grep -e pat -n src").unwrap();
        let words = &s.stages[0].words;
        let ops: Vec<String> = operand_indices(words)
            .into_iter()
            .map(|k| words[k].text())
            .collect();
        assert_eq!(ops, vec!["src"]);
    }
}
