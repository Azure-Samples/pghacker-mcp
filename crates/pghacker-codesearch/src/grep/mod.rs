//! The grep tool: a grep command line, with its trailing pipeline, run over a
//! snapshot as GNU grep 3.11 would run it at the source root, output and
//! all.

pub mod format;
pub mod options;
pub mod shell;
pub mod stages;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_postgres::GenericClient;

use crate::engine::{self, Budget, LineQuery, MAX_CANDIDATES};
use crate::glob::fnmatch;
use crate::pattern::{self, Compiled, Dialect, PatternSpec, Semantics};
use crate::snapshot::{FileTable, Snapshot};
use format::Printer;
use options::{Directories, Mode, Options};
use shell::Word;

/// The most output the tool returns.
pub const MAX_OUTPUT: usize = 1 << 20;

/// The grep tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    /// A grep command line, run at the source root, such as `grep -rn "makeNode(" src/backend | head -20`.
    pub command: String,
    /// PostgreSQL version: a major version such as 19 (its latest snapshot), a release such as 18.3, a ref such as REL_18_3, or a commit SHA. Default: the newest release line.
    #[serde(default)]
    pub version: Option<String>,
}

/// Limits for one run.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub budget_ms: u32,
    pub max_output: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            budget_ms: engine::DEFAULT_BUDGET_MS,
            max_output: MAX_OUTPUT,
        }
    }
}

/// What a command printed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<String>,
    pub status: i32,
    /// The search stopped early (time budget or candidate cap).
    pub incomplete: bool,
}

/// A searched file, as grep names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Searched {
    /// The name grep prints.
    pub display: String,
    /// The snapshot path.
    pub path: String,
    /// Selected line numbers (all of them, before -m).
    pub lines: Vec<i32>,
}

/// Runs a grep command line and returns its output as text: standard
/// output, then any error messages.
pub async fn grep(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &GrepArgs,
) -> Result<String> {
    let out = run(client, snapshot, &args.command, Limits::default()).await?;
    if out.incomplete {
        bail!("{}", out.stderr.join("\n"));
    }
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    for line in &out.stderr {
        text.push_str(line);
        text.push('\n');
    }
    Ok(text)
}

/// Runs a command line with explicit limits.
pub async fn run(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    command: &str,
    limits: Limits,
) -> Result<Output> {
    let budget = Budget::new(Some(limits.budget_ms));
    let old = engine::set_timeout(client, budget.remaining_ms()).await?;
    let result = run_inner(client, snapshot, command, limits, &budget).await;
    engine::restore_timeout(client, &old).await;
    result
}

/// The files and selected lines of a command's grep stage, without
/// printing: what grep would select, every line (no -m, no context).
pub async fn select(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    command: &str,
    budget_ms: u32,
) -> Result<(Vec<Searched>, bool)> {
    let budget = Budget::new(Some(budget_ms));
    let old = engine::set_timeout(client, budget.remaining_ms()).await?;
    let result = async {
        let table = FileTable::load(client, snapshot).await?;
        let script = shell::parse(command).map_err(|e| anyhow!(e))?;
        let cwd = cwd_of(&script, &table, &snapshot.repo)?;
        let words = expand(&script.stages[0].words, &table, &snapshot.repo, &cwd);
        check_program(&words)?;
        let mut opts = Options::parse(&words[1..]).map_err(|e| anyhow!("grep: {e}"))?;
        opts.max_count = None;
        let prepared = prepare(&opts, &table, &snapshot.repo, &cwd)?;
        let compiled = compile(&opts)?;
        let selection = selection(
            client,
            (snapshot, &table),
            &opts,
            &compiled,
            &prepared,
            Need::Numbers,
            &budget,
        )
        .await?;
        let searched = prepared
            .items
            .iter()
            .map(|item| Searched {
                display: item.display.clone(),
                path: table.files[item.file].path.clone(),
                lines: selection.selected(item.file),
            })
            .collect();
        Ok((searched, selection.incomplete || budget.exceeded()))
    }
    .await;
    engine::restore_timeout(client, &old).await;
    result
}

fn check_program(words: &[String]) -> Result<()> {
    let name = words.first().map(String::as_str).unwrap_or_default();
    let base = name.rsplit('/').next().unwrap_or(name);
    match base {
        "grep" | "ggrep" => Ok(()),
        "egrep" | "fgrep" => bail!(
            "{base} is not supported: use grep -{} or the search tool",
            if base == "egrep" { "E" } else { "F" }
        ),
        "rg" | "ag" | "ack" | "git" => bail!(
            "{base} is not supported here: use grep, or the search tool for ripgrep-style searches"
        ),
        _ => bail!("the command must start with grep, not {name}"),
    }
}

/// The folder a leading `cd` moves to.
fn cwd_of(script: &shell::Script, table: &FileTable, repo: &str) -> Result<String> {
    let Some(cd) = &script.cd else {
        return Ok(String::new());
    };
    let dir = cd.text();
    table
        .normalize(repo, &dir)
        .filter(|d| table.is_dir(d))
        .ok_or_else(|| anyhow!("cd: {dir}: No such file or directory"))
}

/// Words after brace and wildcard expansion.
fn expand(words: &[Word], table: &FileTable, repo: &str, cwd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in words {
        for w in shell::brace_expand(word) {
            if w.has_wildcards() {
                let matches = glob_paths(&w.glob_pattern(), table, repo, cwd);
                if !matches.is_empty() {
                    out.extend(matches);
                    continue;
                }
            }
            out.push(w.text());
        }
    }
    out
}

/// Bash pathname expansion of `pattern` (a fnmatch pattern, quoted
/// characters escaped) against the snapshot: the matches in byte order, as
/// typed (a `./` or absolute prefix kept).
fn glob_paths(pattern: &str, table: &FileTable, repo: &str, cwd: &str) -> Vec<String> {
    let comps: Vec<&str> = pattern.split('/').collect();
    let first_wild = comps
        .iter()
        .position(|c| unescaped_wildcard(c))
        .unwrap_or(comps.len());
    if first_wild == comps.len() {
        return Vec::new();
    }
    let prefix_typed = comps[..first_wild].join("/");
    let unescaped: String = prefix_typed.replace('\\', "");
    let base = if prefix_typed.is_empty() {
        Some(cwd.to_owned())
    } else if unescaped.starts_with(['/', '~']) {
        table.normalize(repo, &unescaped)
    } else {
        join(cwd, &unescaped).and_then(|p| table.normalize(repo, &p))
    };
    let Some(base) = base.filter(|b| table.is_dir(b)) else {
        return Vec::new();
    };
    let mut found: Vec<(String, String)> = vec![(base, prefix_typed)];
    for (k, comp) in comps[first_wild..].iter().enumerate() {
        let last = first_wild + k == comps.len() - 1;
        let mut next = Vec::new();
        for (dir, typed) in &found {
            let sep = |name: &str| {
                if typed.is_empty() {
                    name.to_owned()
                } else if typed.ends_with('/') {
                    format!("{typed}{name}")
                } else {
                    format!("{typed}/{name}")
                }
            };
            if comp.is_empty() {
                // A trailing slash: directories only.
                if last && table.is_dir(dir) {
                    next.push((dir.clone(), format!("{typed}/")));
                }
                continue;
            }
            if !unescaped_wildcard(comp) {
                let name = comp.replace('\\', "");
                if let Some(path) = join(dir, &name).filter(|p| table.exists(p)) {
                    next.push((path, sep(&name)));
                }
                continue;
            }
            for (name, _) in children(table, dir) {
                if name.starts_with('.') && !comp.starts_with('.') {
                    continue;
                }
                if fnmatch(comp, &name)
                    && let Some(path) = join(dir, &name)
                {
                    next.push((path, sep(&name)));
                }
            }
        }
        found = next;
    }
    let mut out: Vec<String> = found.into_iter().map(|(_, typed)| typed).collect();
    out.sort();
    out.dedup();
    out
}

fn unescaped_wildcard(comp: &str) -> bool {
    let mut escaped = false;
    for c in comp.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
        } else if matches!(c, '*' | '?' | '[') {
            return true;
        }
    }
    false
}

/// The names in folder `dir`: (name, is a folder).
fn children(table: &FileTable, dir: &str) -> Vec<(String, bool)> {
    let mut seen = BTreeMap::new();
    let skip = if dir.is_empty() { 0 } else { dir.len() + 1 };
    for k in table.under(dir) {
        let rest = &table.files[k].path[skip..];
        match rest.split_once('/') {
            Some((name, _)) => seen.insert(name.to_owned(), true),
            None => seen.insert(rest.to_owned(), false),
        };
    }
    seen.into_iter().collect()
}

/// `rel` from folder `dir`, resolved lexically; None above the root.
fn join(dir: &str, rel: &str) -> Option<String> {
    let joined = if dir.is_empty() {
        rel.to_owned()
    } else if rel.is_empty() {
        dir.to_owned()
    } else {
        format!("{dir}/{rel}")
    };
    let mut out: Vec<&str> = Vec::new();
    for comp in joined.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            c => out.push(c),
        }
    }
    Some(out.join("/"))
}

/// A file to search, in output order.
struct Item {
    display: String,
    file: usize,
}

struct Prepared {
    items: Vec<Item>,
    stderr: Vec<String>,
    with_filename: bool,
    errors: bool,
}

/// Resolves operands into the files grep reads, in its order, with the
/// messages it prints for missing files and directories.
fn prepare(opts: &Options, table: &FileTable, repo: &str, cwd: &str) -> Result<Prepared> {
    let mut p = Prepared {
        items: Vec::new(),
        stderr: Vec::new(),
        with_filename: false,
        errors: false,
    };
    let implicit = opts.operands.is_empty();
    // Without file operands GNU grep would read standard input; here, as
    // with -r, it searches the whole tree.
    let recurse = opts.directories == Directories::Recurse || implicit;
    let operands: Vec<String> = if implicit {
        vec![".".into()]
    } else {
        opts.operands.clone()
    };
    // -1: decided by the first operand (a folder means yes).
    let mut out_file: i8 = match opts.with_filename {
        Some(true) => 1,
        Some(false) => 0,
        None if operands.len() > 1 => 1,
        None if recurse => -1,
        None => 0,
    };
    let message = |p: &mut Prepared, text: String| {
        p.errors = true;
        if !opts.no_messages {
            p.stderr.push(text);
        }
    };
    for op in &operands {
        if op == "-" {
            message(
                &mut p,
                "grep: (standard input): reading standard input is not supported".into(),
            );
            continue;
        }
        let path = if op.starts_with(['/', '~']) {
            table.normalize(repo, op)
        } else {
            join(cwd, op)
        };
        let Some(path) = path.filter(|p| table.exists(p)) else {
            message(&mut p, format!("grep: {op}: No such file or directory"));
            continue;
        };
        let is_dir = table.file(&path).is_none();
        if is_dir {
            if opts.directories == Directories::Skip {
                continue;
            }
            if !implicit && recurse && opts.exclude_dir.excluded(op, false) {
                continue;
            }
        } else if opts.files.excluded(op, false) {
            continue;
        }
        if out_file < 0 {
            out_file = i8::from(is_dir);
        }
        if !is_dir {
            p.items.push(Item {
                display: op.clone(),
                file: table.file(&path).unwrap(),
            });
            continue;
        }
        if !recurse {
            message(&mut p, format!("grep: {op}: Is a directory"));
            continue;
        }
        let prefix = if implicit {
            String::new()
        } else {
            let trimmed = op.trim_end_matches('/');
            format!("{}/", if trimmed.is_empty() { "" } else { trimmed })
        };
        let skip = if path.is_empty() { 0 } else { path.len() + 1 };
        let mut pruned: Vec<String> = Vec::new();
        for k in table.under(&path) {
            let f = &table.files[k];
            let rel = &f.path[skip..];
            if pruned.iter().any(|d| rel.starts_with(d.as_str())) {
                continue;
            }
            // Folders between the operand and the file, by base name.
            let mut end = 0;
            let mut excluded = false;
            while let Some(j) = rel[end..].find('/') {
                let dir_rel = &rel[..end + j];
                let base = dir_rel.rsplit('/').next().unwrap_or(dir_rel);
                if !opts.exclude_dir.is_empty() && opts.exclude_dir.excluded(base, true) {
                    pruned.push(format!("{dir_rel}/"));
                    excluded = true;
                    break;
                }
                end += j + 1;
            }
            if excluded || f.is_binary {
                continue;
            }
            let base = rel.rsplit('/').next().unwrap_or(rel);
            if opts.files.excluded(base, true) {
                continue;
            }
            p.items.push(Item {
                display: format!("{prefix}{rel}"),
                file: k,
            });
        }
    }
    p.with_filename = out_file > 0;
    p.items.retain(|item| !table.files[item.file].is_binary);
    Ok(p)
}

fn compile(opts: &Options) -> Result<Compiled> {
    pattern::compile(&PatternSpec {
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
    .map_err(|e| anyhow!("grep: {e}"))
}

/// Selected lines per file.
struct Selection {
    /// Selected lines with their text, per file, in order.
    lines: HashMap<usize, Vec<(i32, String)>>,
    /// For -v without printed lines: matching line counts per file.
    matched: HashMap<usize, u64>,
    /// For -v line numbers: the matching lines per file.
    matched_lines: HashMap<usize, Vec<i32>>,
    invert_counts: bool,
    incomplete: bool,
}

impl Selection {
    fn selected(&self, file: usize) -> Vec<i32> {
        self.lines
            .get(&file)
            .map(|l| l.iter().map(|(n, _)| *n).collect())
            .unwrap_or_default()
    }
}

/// What a selection must give per file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Selected line counts.
    Counts,
    /// Selected line numbers.
    Numbers,
    /// Selected lines with their text.
    Texts,
}

async fn selection(
    client: &impl GenericClient,
    source: (&Snapshot, &FileTable),
    opts: &Options,
    compiled: &Compiled,
    prepared: &Prepared,
    need: Need,
    budget: &Budget,
) -> Result<Selection> {
    let (snapshot, table) = source;
    let need_lines = need == Need::Texts;
    let mut files: Vec<usize> = prepared.items.iter().map(|i| i.file).collect();
    files.sort_unstable();
    files.dedup();
    let ids: Vec<i32> = files.iter().map(|&k| table.files[k].id).collect();
    let by_id: HashMap<i32, usize> = files.iter().map(|&k| (table.files[k].id, k)).collect();
    let mut sel = Selection {
        lines: HashMap::new(),
        matched: HashMap::new(),
        matched_lines: HashMap::new(),
        invert_counts: false,
        incomplete: false,
    };
    if files.is_empty() {
        return Ok(sel);
    }
    let invert_lines = opts.invert && need_lines;
    let require = if invert_lines {
        vec![]
    } else {
        vec![&compiled.filter]
    };
    let scope_lines = files
        .iter()
        .map(|&k| table.files[k].line_count.max(0) as u64)
        .sum();
    let scope_first = engine::scope_first(scope_lines, &require);
    let q = LineQuery {
        snapshot_id: snapshot.id,
        history: snapshot.history.as_ref(),
        file_ids: Some(&ids),
        require,
        reject: if invert_lines {
            vec![&compiled.filter]
        } else {
            vec![]
        },
        cap: MAX_CANDIDATES,
        scope_first,
    };
    let found = engine::candidates(client, &q, budget).await?;
    sel.incomplete = found.capped || found.timed_out;
    for row in found.rows {
        if budget.exceeded() {
            sel.incomplete = true;
            break;
        }
        let Some(&k) = by_id.get(&row.file_id) else {
            continue;
        };
        let is_match = compiled.matcher.is_match(&row.line);
        if opts.invert && !need_lines {
            if is_match {
                *sel.matched.entry(k).or_default() += 1;
                if need == Need::Numbers {
                    sel.matched_lines.entry(k).or_default().push(row.line_no);
                }
            }
        } else if is_match != opts.invert {
            sel.lines
                .entry(k)
                .or_default()
                .push((row.line_no, row.line));
        }
    }
    for lines in sel.lines.values_mut() {
        lines.sort_unstable_by_key(|(n, _)| *n);
    }
    sel.invert_counts = opts.invert && !need_lines;
    if opts.invert && need == Need::Numbers {
        if sel.incomplete {
            return Ok(sel);
        }
        // The selected lines are the complement of the matching ones.
        sel.invert_counts = false;
        for &k in &files {
            let matching = sel.matched_lines.entry(k).or_default();
            matching.sort_unstable();
            let mut lines = Vec::new();
            for n in 1..=table.files[k].line_count {
                if budget.exceeded() {
                    sel.incomplete = true;
                    break;
                }
                if matching.binary_search(&n).is_err() {
                    lines.push((n, String::new()));
                }
            }
            if !lines.is_empty() {
                sel.lines.insert(k, lines);
            }
            if sel.incomplete {
                break;
            }
        }
    }
    Ok(sel)
}

async fn run_inner(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    command: &str,
    limits: Limits,
    budget: &Budget,
) -> Result<Output> {
    let table = FileTable::load(client, snapshot).await?;
    let script = shell::parse(command).map_err(|e| anyhow!(e))?;
    let cwd = cwd_of(&script, &table, &snapshot.repo)?;
    let first = &script.stages[0];
    let words = expand(&first.words, &table, &snapshot.repo, &cwd);
    check_program(&words)?;
    let opts = Options::parse(&words[1..]).map_err(|e| anyhow!("grep: {e}"))?;
    let compiled = compile(&opts)?;
    let prepared = prepare(&opts, &table, &snapshot.repo, &cwd)?;
    // GNU grep gives up at once when nothing can be selected: -m 0, or -v
    // with one empty pattern.
    let hopeless = (opts.max_count == Some(0)
        || (opts.invert && opts.patterns == [""] && !opts.line && !opts.word))
        && opts.mode != Mode::FilesWithoutMatch;
    // Later stages are checked before the search runs.
    let mut later = Vec::new();
    for stage in &script.stages[1..] {
        let words = expand(&stage.words, &table, &snapshot.repo, &cwd);
        check_stage(&words)?;
        later.push(words);
    }
    let need_lines = opts.mode == Mode::Lines;
    let need = if need_lines {
        Need::Texts
    } else {
        Need::Counts
    };
    let prepared = if hopeless {
        Prepared {
            items: Vec::new(),
            stderr: Vec::new(),
            errors: false,
            ..prepared
        }
    } else {
        prepared
    };
    let sel = selection(
        client,
        (snapshot, &table),
        &opts,
        &compiled,
        &prepared,
        need,
        budget,
    )
    .await?;
    if sel.incomplete {
        return Ok(incomplete_output());
    }

    // Context lines, when the printed lines need them.
    let mut extra: HashMap<(usize, i32), String> = HashMap::new();
    if need_lines && (opts.before > 0 || opts.after > 0) {
        let max = opts.max_count.map_or(usize::MAX, |m| m as usize);
        let mut ranges = Vec::new();
        for (&k, lines) in &sel.lines {
            let last = table.files[k].line_count;
            for (n, _) in lines.iter().take(max) {
                let lo = (n - opts.before.max(0) as i32).max(1);
                let hi = (n + opts.after.max(0) as i32).min(last);
                ranges.push((table.files[k].id, lo, hi));
            }
        }
        let id_to_file: HashMap<i32, usize> =
            sel.lines.keys().map(|&k| (table.files[k].id, k)).collect();
        for ((id, n), text) in engine::line_ranges(
            client,
            snapshot.history.as_ref(),
            &engine::merge_ranges(ranges),
            budget,
        )
        .await?
        {
            extra.insert((id_to_file[&id], n), text);
        }
    }

    let mut printer = Printer::new(&opts, &compiled.matcher, prepared.with_filename);
    for item in &prepared.items {
        let k = item.file;
        let f = &table.files[k];
        let empty = Vec::new();
        let lines = sel.lines.get(&k).unwrap_or(&empty);
        let selected: Vec<i32> = lines.iter().map(|(n, _)| *n).collect();
        let count = sel.invert_counts.then(|| {
            (f.line_count as u64).saturating_sub(sel.matched.get(&k).copied().unwrap_or(0))
        });
        let by_line: HashMap<i32, &str> = lines.iter().map(|(n, t)| (*n, t.as_str())).collect();
        let text = |n: i32| {
            by_line
                .get(&n)
                .map(|t| (*t).to_owned())
                .or_else(|| extra.get(&(k, n)).cloned())
        };
        let found = printer.file(&item.display, &selected, count, f.line_count, &text);
        if opts.mode == Mode::Quiet && found > 0 {
            break;
        }
        if budget.exceeded() {
            return Ok(incomplete_output());
        }
    }
    let selected_any = printer.selected > 0;
    let listed = printer.listed;
    let mut stdout = printer.out;
    let mut status = if opts.mode == Mode::FilesWithoutMatch {
        if listed > 0 { 0 } else { 1 }
    } else if selected_any {
        0
    } else {
        1
    };
    if prepared.errors && !(opts.mode == Mode::Quiet && selected_any) {
        status = 2;
    }
    let mut stderr = prepared.stderr.clone();
    if first.stderr_to_stdout() {
        let mut merged: Vec<u8> = stderr
            .iter()
            .flat_map(|l| format!("{l}\n").into_bytes())
            .collect();
        merged.extend(stdout);
        stdout = merged;
        stderr.clear();
    } else if first.stderr_null() {
        stderr.clear();
    }
    for words in later {
        if budget.exceeded() {
            return Ok(incomplete_output());
        }
        let (out, code) = run_stage(&words, &stdout)?;
        stdout = out;
        status = code;
    }
    if budget.exceeded() {
        return Ok(incomplete_output());
    }
    if status != 0
        && let Some(fallback) = &script.fallback
    {
        let words = expand(&fallback.words, &table, &snapshot.repo, &cwd);
        match words[0].as_str() {
            "true" | ":" => status = 0,
            "echo" => {
                let mut args = &words[1..];
                let newline = args.first().map(String::as_str) != Some("-n");
                if !newline || args.first().map(String::as_str) == Some("-e") {
                    args = &args[1..];
                }
                stdout.extend_from_slice(args.join(" ").as_bytes());
                if newline {
                    stdout.push(b'\n');
                }
                status = 0;
            }
            other => bail!("only true, : or echo may follow ||, not {other}"),
        }
    }
    if stdout.len() > limits.max_output {
        let cut = stdout[..limits.max_output]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(limits.max_output, |k| k + 1);
        stdout.truncate(cut);
        stderr.push(format!(
            "grep: (pghacker) output cut at {} bytes: add | head or narrow the search",
            limits.max_output
        ));
    }
    Ok(Output {
        stdout,
        stderr,
        status,
        incomplete: sel.incomplete,
    })
}

fn incomplete_output() -> Output {
    Output {
        stderr: vec![
            "grep: (pghacker) the search stopped early, at the time budget or at 200,000 \
             candidate lines; no result or count was returned: narrow the search"
                .into(),
        ],
        status: 2,
        incomplete: true,
        ..Output::default()
    }
}

const STAGES: &str = "head, tail, wc, sort, uniq, cut, cat and grep";

fn check_stage(words: &[String]) -> Result<()> {
    let name = words[0].rsplit('/').next().unwrap_or(&words[0]);
    match name {
        "head" | "tail" | "wc" | "sort" | "uniq" | "cut" | "cat" | "grep" | "ggrep" => Ok(()),
        "egrep" | "fgrep" => bail!("use grep -E or grep -F after |, not {name}"),
        "xargs" => bail!(
            "xargs is not supported: grep the files directly (grep -r with --include, or file operands)"
        ),
        other => bail!(
            "the pipeline stage {other} is not supported; supported stages: {STAGES}. \
             The search tool counts matches (group_by match), orders by coverage and pages results"
        ),
    }
}

/// Runs one later stage on `input`: its output and exit status.
fn run_stage(words: &[String], input: &[u8]) -> Result<(Vec<u8>, i32)> {
    let name = words[0].rsplit('/').next().unwrap_or(&words[0]).to_owned();
    let args = &words[1..];
    let out = match name.as_str() {
        "head" => stages::head(args, input),
        "tail" => stages::tail(args, input),
        "wc" => stages::wc(args, input),
        "sort" => stages::sort(args, input),
        "uniq" => stages::uniq(args, input),
        "cut" => stages::cut(args, input),
        "cat" => stages::cat(args, input),
        _ => return grep_stage(args, input),
    };
    out.map(|o| (o, 0)).map_err(|e| anyhow!(e))
}

/// A grep after `|`: it reads the previous output.
fn grep_stage(args: &[String], input: &[u8]) -> Result<(Vec<u8>, i32)> {
    let opts = Options::parse(args).map_err(|e| anyhow!("grep: {e}"))?;
    if opts.operands.iter().any(|o| o != "-") || opts.directories == Directories::Recurse {
        bail!("a grep after | reads the previous output; it cannot take files or -r");
    }
    let compiled = compile(&opts)?;
    let text = String::from_utf8_lossy(input);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') || text.is_empty() {
        lines.pop();
    }
    let selected: Vec<i32> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| compiled.matcher.is_match(l) != opts.invert)
        .map(|(k, _)| k as i32 + 1)
        .collect();
    let mut printer = Printer::new(&opts, &compiled.matcher, opts.with_filename == Some(true));
    let get = |n: i32| lines.get(n as usize - 1).map(|s| (*s).to_owned());
    printer.file(
        "(standard input)",
        &selected,
        None,
        lines.len() as i32,
        &get,
    );
    let status = if opts.mode == Mode::FilesWithoutMatch {
        i32::from(printer.listed == 0)
    } else {
        i32::from(printer.selected == 0)
    };
    Ok((printer.out, status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::FileEntry;

    fn table() -> FileTable {
        let paths = [
            "src/backend/parser/gram.y",
            "src/backend/parser/scan.l",
            "src/backend/parser/parse_expr.c",
            "src/backend/utils/adt/json.c",
            "src/include/nodes/nodes.h",
            "src/.hidden.c",
        ];
        FileTable::new(
            paths
                .iter()
                .enumerate()
                .map(|(k, p)| FileEntry {
                    id: k as i32 + 1,
                    path: (*p).to_owned(),
                    is_binary: false,
                    line_count: 1,
                    size: 1,
                })
                .collect(),
            vec![],
        )
    }

    #[test]
    fn wildcards_expand_like_bash() {
        let t = table();
        let g = |p: &str, cwd: &str| glob_paths(p, &t, "postgres", cwd);
        assert_eq!(
            g("src/backend/parser/*.[cly]", ""),
            vec![
                "src/backend/parser/gram.y",
                "src/backend/parser/parse_expr.c",
                "src/backend/parser/scan.l"
            ]
        );
        assert_eq!(g("src/*", ""), vec!["src/backend", "src/include"]);
        assert_eq!(g("src/.*", ""), vec!["src/.hidden.c"]);
        assert_eq!(g("parser/*.y", "src/backend"), vec!["parser/gram.y"]);
        assert_eq!(g("./src/*/nodes", ""), vec!["./src/include/nodes"]);
        assert!(g("/home/me/pg/src/*/adt", "").is_empty());
        assert_eq!(
            g("/home/me/pg/src/backend/*/adt", ""),
            vec!["/home/me/pg/src/backend/utils/adt"]
        );
        assert_eq!(g("src/backend/*/adt", ""), vec!["src/backend/utils/adt"]);
        assert!(g("nothing/*.c", "").is_empty());
    }

    #[test]
    fn operands_resolve_with_gnu_rules() {
        let t = table();
        let words: Vec<String> = ["-r", "--include=*.c", "x", "src/backend/", "src/nope"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let opts = Options::parse(&words).unwrap();
        let p = prepare(&opts, &t, "postgres", "").unwrap();
        let names: Vec<&str> = p.items.iter().map(|i| i.display.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "src/backend/parser/parse_expr.c",
                "src/backend/utils/adt/json.c"
            ]
        );
        assert_eq!(p.stderr, vec!["grep: src/nope: No such file or directory"]);
        assert!(p.with_filename);
        let words: Vec<String> = ["-r", "x", "..", "src/../src/include"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let opts = Options::parse(&words).unwrap();
        let p = prepare(&opts, &t, "postgres", "").unwrap();
        assert_eq!(p.stderr, vec!["grep: ..: No such file or directory"]);
        assert_eq!(p.items.len(), 1);

        let words: Vec<String> = ["-r", "--exclude-dir=parser", "x"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let opts = Options::parse(&words).unwrap();
        let p = prepare(&opts, &t, "postgres", "src/backend").unwrap();
        let names: Vec<&str> = p.items.iter().map(|i| i.display.as_str()).collect();
        assert_eq!(names, vec!["utils/adt/json.c"]);

        let words: Vec<String> = ["x", "src"].iter().map(|s| (*s).to_owned()).collect();
        let opts = Options::parse(&words).unwrap();
        let p = prepare(&opts, &t, "postgres", "").unwrap();
        assert!(p.items.is_empty());
        assert_eq!(p.stderr, vec!["grep: src: Is a directory"]);
        assert!(!p.with_filename);

        // No operands: the whole tree, as with -r.
        let words: Vec<String> = ["-n", "x"].iter().map(|s| (*s).to_owned()).collect();
        let opts = Options::parse(&words).unwrap();
        let p = prepare(&opts, &t, "postgres", "src/include").unwrap();
        let names: Vec<&str> = p.items.iter().map(|i| i.display.as_str()).collect();
        assert_eq!(names, vec!["nodes/nodes.h"]);
        assert!(p.with_filename);
    }

    #[test]
    fn downstream_grep_and_stages() {
        let input = b"a.c:1:foo\nb.c:2:bar\nc.c:3:foo bar\n";
        let words: Vec<String> = ["-v", "bar"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            grep_stage(&words, input).unwrap(),
            (b"a.c:1:foo\n".to_vec(), 0)
        );
        let words: Vec<String> = ["-c", "foo"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(grep_stage(&words, input).unwrap().0, b"2\n".to_vec());
        assert!(check_stage(&["awk".to_owned()]).is_err());
        assert!(check_program(&["egrep".to_owned()]).is_err());
        assert!(check_program(&["/usr/bin/grep".to_owned()]).is_ok());
    }
}
