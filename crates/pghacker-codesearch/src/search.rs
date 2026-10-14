//! The first-class search operation: text, regex or identifier queries over a
//! snapshot's lines, with scope, filters, output shapes, paging, coverage
//! order, a time budget and similar names when nothing matches.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

use crate::engine::{self, Budget, LineQuery, MAX_CANDIDATES};
use crate::glob::RgFilter;
use crate::pattern::{self, Compiled, Dialect, PatternSpec, Semantics};
use crate::snapshot::{FileTable, Snapshot};

/// Default and largest page sizes, and the most context lines.
pub const DEFAULT_LIMIT: u32 = 100;
pub const MAX_LIMIT: u32 = 2_000;
pub const MAX_CONTEXT: u32 = 20;
/// Longer lines are cut in results.
pub const MAX_LINE_CHARS: usize = 1_000;

/// One string or a list of strings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Strings {
    One(String),
    Many(Vec<String>),
}

impl Default for Strings {
    fn default() -> Self {
        Strings::Many(Vec::new())
    }
}

impl Strings {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Strings::One(s) => vec![s.clone()],
            Strings::Many(v) => v.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Strings::One(_) => false,
            Strings::Many(v) => v.is_empty(),
        }
    }
}

impl From<&str> for Strings {
    fn from(s: &str) -> Self {
        Strings::One(s.to_owned())
    }
}

impl From<Vec<String>> for Strings {
    fn from(v: Vec<String>) -> Self {
        Strings::Many(v)
    }
}

/// How the query is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Plain text.
    #[default]
    Text,
    /// A ripgrep (Rust) regex; lookarounds and backreferences are accepted.
    Regex,
    /// A name in any spelling.
    Identifier,
}

/// What the result lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    #[default]
    Lines,
    Files,
    Counts,
}

/// What counts are grouped by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    #[default]
    File,
    Match,
}

/// Result order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    #[default]
    Path,
    Coverage,
}

/// The search tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// What to find: text, a regex or a name (see `match`). A list finds lines with any of them; "" matches every line.
    pub query: Strings,
    /// `text` (default): plain text. `regex`: ripgrep (Rust) syntax. `identifier`: a name in any spelling ("parse node" finds parseNode, parse_node, PARSE_NODE).
    #[serde(default, rename = "match")]
    pub match_mode: MatchMode,
    /// Match case exactly. Default true, but false for identifiers.
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// Whole words only (grep -w).
    #[serde(default)]
    pub whole_word: bool,
    /// Files, folders or globs to search, relative to the source root, e.g. src/backend/parser or src/include/nodes/*.h. Default: everything loaded.
    #[serde(default)]
    pub paths: Strings,
    /// Only files whose names match these globs, e.g. *.c or *.{c,h}; a glob with a slash matches the path from the root.
    #[serde(default)]
    pub include: Strings,
    /// Skip files and folders whose names match these globs, e.g. *test*.
    #[serde(default)]
    pub exclude: Strings,
    /// Patterns each line must also match (| grep), read like the query.
    #[serde(default)]
    pub also: Strings,
    /// Patterns each line must not match (| grep -v). With query "", the lines matching none of them (grep -v).
    #[serde(default)]
    pub not: Strings,
    /// `lines` (default), `files` (with matching line counts) or `counts`.
    #[serde(default)]
    pub output: OutputMode,
    /// `file` (default) or `match`: counts per distinct matched text (grep -o | sort | uniq -c); with lines, the matched parts (grep -o).
    #[serde(default)]
    pub group_by: GroupBy,
    /// Lines of context around each matching line (grep -C), at most 20.
    #[serde(default)]
    pub context: u32,
    /// Items per page: default 100, at most 2000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Items to skip, for the next page.
    #[serde(default)]
    pub offset: u32,
    /// `path` (default) or `coverage`: files matching the most query terms first.
    #[serde(default)]
    pub order: Order,
    /// Time limit in milliseconds: default 4000, at most 10000.
    #[serde(default)]
    pub budget_ms: Option<u32>,
    /// PostgreSQL version: a major version such as 19 (its latest snapshot), a release such as 18.3, a ref such as REL_18_3, or a commit SHA. Default: the newest release line.
    #[serde(default)]
    pub version: Option<String>,
    /// Search the commit history instead of one version: `all`, a branch, tag or commit with its history (REL_17_STABLE), or a range A..B (B's history that isn't A's). Gives each line once per path and text, with the commit that added it, those that removed it, and the branches and releases that have it.
    #[serde(default)]
    pub history: Option<String>,
}

impl SearchArgs {
    /// A text search for `query`, with every other argument at its default.
    pub fn new(query: impl Into<Strings>) -> Self {
        Self {
            query: query.into(),
            ..Self::default()
        }
    }
}

/// A matching line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LineHit {
    pub path: String,
    pub line: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// A file with matches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileHit {
    pub path: String,
    /// Matching lines in the file.
    pub lines: u64,
    /// Query terms the file matches (coverage order).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms: Option<u32>,
}

/// A distinct matched text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MatchCount {
    pub text: String,
    pub count: u64,
}

/// A name found near an unmatched query.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NameCount {
    pub name: String,
    pub lines: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
}

/// Names like the query, when nothing matched.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Similar {
    /// The query's name in other spellings or cases.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spellings: Vec<NameCount>,
    /// Similar names (pg_trgm word similarity).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<NameCount>,
}

/// A search's result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchResult {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    /// Matching lines in scope.
    pub total_lines: u64,
    /// Files with matching lines.
    pub total_files: u64,
    /// Matches counted (group_by match).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_matches: Option<u64>,
    pub offset: u32,
    pub returned: u32,
    /// More items than returned: ask again with a higher offset.
    pub cut: bool,
    /// The time budget ran out: totals are lower bounds.
    pub budget_exceeded: bool,
    /// Every line in scope was checked (false after the budget or the
    /// 200,000 candidate-line cap).
    pub complete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<LineHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<MatchCount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similar: Option<Similar>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Lines in history (with `history`), one per path and text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<crate::history_search::HistoryHit>,
    /// The commits a history search covered: `all` or the range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    pub elapsed_ms: u64,
}

/// A matching line found by the engine: (file index, line number, text).
pub(crate) type Hit = (usize, i32, String);

/// The verified lines a set of patterns finds in a scope.
pub(crate) struct Found {
    pub hits: Vec<Hit>,
    pub capped: bool,
    pub timed_out: bool,
}

/// The patterns a line must match: the query, every `also`, and not `not`.
pub(crate) struct Patterns<'a> {
    pub query: &'a Compiled,
    pub also: &'a [Compiled],
    pub not: Option<&'a Compiled>,
}

impl Patterns<'_> {
    pub fn is_match(&self, line: &str) -> bool {
        self.query.matcher.is_match(line)
            && self.also.iter().all(|a| a.matcher.is_match(line))
            && !self.not.is_some_and(|n| n.matcher.is_match(line))
    }
}

/// Runs the line query for `files` (indices into `table`; `whole` when they
/// are all of the snapshot's text files) and keeps the lines the patterns
/// match.
pub(crate) async fn find_lines(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    table: &FileTable,
    files: &[usize],
    whole: bool,
    patterns: &Patterns<'_>,
    budget: &Budget,
) -> Result<Found> {
    if files.is_empty() {
        return Ok(Found {
            hits: Vec::new(),
            capped: false,
            timed_out: false,
        });
    }
    // Mounted trees share versions: one row can be a line of several files.
    let mut by_id: HashMap<i32, Vec<usize>> = HashMap::new();
    for &k in files {
        by_id.entry(table.files[k].id).or_default().push(k);
    }
    let mut ids: Vec<i32> = by_id.keys().copied().collect();
    ids.sort_unstable();
    let mut require = vec![&patterns.query.filter];
    require.extend(patterns.also.iter().map(|c| &c.filter));
    let scope_lines = files
        .iter()
        .map(|&k| table.files[k].line_count.max(0) as u64)
        .sum();
    let scope_first = engine::scope_first(scope_lines, &require);
    let q = LineQuery {
        snapshot_id: snapshot.id,
        history: snapshot.history.as_ref(),
        mounted: files.iter().any(|&k| k >= table.own),
        file_ids: if whole { None } else { Some(&ids) },
        require,
        reject: patterns.not.map(|n| vec![&n.filter]).unwrap_or_default(),
        cap: MAX_CANDIDATES,
        scope_first,
    };
    let found = engine::candidates(client, &q, budget).await?;
    let mut timed_out = found.timed_out;
    let mut hits = Vec::new();
    for row in found.rows {
        if budget.exceeded() {
            timed_out = true;
            break;
        }
        if let Some(ks) = by_id.get(&row.file_id)
            && patterns.is_match(&row.line)
        {
            for &k in ks {
                hits.push((k, row.line_no, row.line.clone()));
            }
        }
    }
    hits.sort_unstable_by_key(|h| (h.0, h.1));
    Ok(Found {
        hits,
        capped: found.capped,
        timed_out,
    })
}

/// Searches `snapshot`. Pass a client in autocommit mode: the budget is a
/// statement timeout, and a timeout inside a caller's transaction aborts it.
pub async fn search(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &SearchArgs,
) -> Result<SearchResult> {
    let budget = Budget::new(args.budget_ms);
    let old = engine::set_timeout(client, budget.remaining_ms()).await?;
    let result = search_inner(client, snapshot, args, &budget).await;
    engine::restore_timeout(client, &old).await;
    result
}

struct Plan {
    query: Compiled,
    also: Vec<Compiled>,
    not: Option<Compiled>,
    terms: Vec<Compiled>,
    every_line: bool,
}

fn plan(args: &SearchArgs) -> Result<Plan> {
    let ci = !args
        .case_sensitive
        .unwrap_or(args.match_mode != MatchMode::Identifier);
    let compile_list = |items: &[String]| -> Result<Compiled> {
        let (patterns, dialect) = match args.match_mode {
            MatchMode::Text => (items.to_vec(), Dialect::Fixed),
            MatchMode::Regex => (items.to_vec(), Dialect::Rust),
            MatchMode::Identifier => (
                items
                    .iter()
                    .map(|q| {
                        pattern::identifier_pattern(q)
                            .ok_or_else(|| anyhow!("{q:?} has no letters or digits to match"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                Dialect::Rust,
            ),
        };
        pattern::compile(&PatternSpec {
            patterns: &patterns,
            dialect,
            ignore_case: ci,
            word: args.whole_word,
            line: false,
            semantics: Semantics::LeftmostFirst,
        })
        .map_err(|error| anyhow!("invalid pattern: {error}"))
    };
    let items = args.query.to_vec();
    let every_line = items.iter().all(|q| q.is_empty());
    let query = if every_line {
        pattern::compile(&PatternSpec {
            patterns: &[String::new()],
            dialect: Dialect::Fixed,
            ignore_case: false,
            word: false,
            line: false,
            semantics: Semantics::LeftmostFirst,
        })
        .map_err(|error| anyhow!(error))?
    } else {
        compile_list(&items)?
    };
    let also = args
        .also
        .to_vec()
        .iter()
        .filter(|a| !a.is_empty())
        .map(|a| compile_list(std::slice::from_ref(a)))
        .collect::<Result<Vec<_>>>()?;
    let not_items: Vec<String> = args
        .not
        .to_vec()
        .into_iter()
        .filter(|n| !n.is_empty())
        .collect();
    let not = if not_items.is_empty() {
        None
    } else {
        Some(compile_list(&not_items)?)
    };
    let mut terms = Vec::new();
    if args.order == Order::Coverage && !every_line {
        let mut term_items = items.clone();
        if args.match_mode == MatchMode::Regex && items.len() == 1 {
            let alts = split_rust_alts(&items[0]);
            if alts.len() > 1 {
                term_items = alts;
            }
        }
        for item in &term_items {
            match compile_list(std::slice::from_ref(item)) {
                Ok(c) => terms.push(c),
                Err(_) => {
                    terms.clear();
                    break;
                }
            }
        }
    }
    Ok(Plan {
        query,
        also,
        not,
        terms,
        every_line,
    })
}

/// The files a search covers: named files (always), files below named
/// folders and files matching path globs, filtered by include and exclude
/// globs as ripgrep's -g filters them. Also whether that is every file.
pub(crate) fn search_scope(
    table: &FileTable,
    repo: &str,
    paths: &[String],
    include: &[String],
    exclude: &[String],
) -> Result<(Vec<usize>, bool)> {
    let filter = RgFilter::new(include, exclude).map_err(|e| anyhow!(e))?;
    let paths: Vec<&String> = paths.iter().filter(|p| !p.trim().is_empty()).collect();
    if paths.is_empty() {
        let files: Vec<usize> = (0..table.own)
            .filter(|&k| !table.files[k].is_binary && filter.allows("", &table.files[k].path))
            .collect();
        return Ok((files, filter.is_empty()));
    }
    let mut chosen: Vec<bool> = vec![false; table.files.len()];
    for raw in paths {
        if raw.contains(['*', '?', '[', '{']) {
            let glob = path_glob(table, repo, raw)?;
            // A glob reaches into a mounted tree only when it names one.
            let range = if crate::snapshot::mount_of(raw).is_some() {
                table.own..table.files.len()
            } else {
                0..table.own
            };
            for k in range {
                let f = &table.files[k];
                if glob_covers(&glob, &f.path) && filter.allows("", &f.path) {
                    chosen[k] = true;
                }
            }
            continue;
        }
        let path = table
            .normalize(repo, raw)
            .filter(|p| table.exists(p))
            .ok_or_else(|| missing_path(table, raw))?;
        if let Some(k) = table.file(&path) {
            chosen[k] = true;
        } else {
            for k in table.under(&path) {
                if filter.allows(&path, &table.files[k].path) {
                    chosen[k] = true;
                }
            }
        }
    }
    let files = (0..table.files.len())
        .filter(|&k| chosen[k] && !table.files[k].is_binary)
        .collect();
    Ok((files, false))
}

fn missing_path(table: &FileTable, raw: &str) -> anyhow::Error {
    let loaded = if table.loaded_paths.is_empty() {
        String::new()
    } else {
        format!("; loaded: {}", table.loaded_paths.join(", "))
    };
    anyhow!(
        "no such file or folder: {raw} (paths are relative to the source root, \
         like src/backend/parser{loaded})"
    )
}

/// A path glob from the root: `*` within a name, `**` across folders.
fn path_glob(table: &FileTable, repo: &str, raw: &str) -> Result<globset::GlobMatcher> {
    let raw = raw.trim();
    let first_glob = raw.find(['*', '?', '[', '{']).unwrap_or(raw.len());
    let (dir, rest) = match raw[..first_glob].rfind('/') {
        Some(k) => (&raw[..k], &raw[k + 1..]),
        None => ("", raw),
    };
    let dir = if dir.is_empty() {
        String::new()
    } else {
        table
            .normalize(repo, dir)
            .ok_or_else(|| missing_path(table, raw))?
    };
    let pattern = if dir.is_empty() {
        rest.to_owned()
    } else {
        format!("{dir}/{rest}")
    };
    let pattern = pattern.trim_end_matches('/');
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|error| anyhow!("invalid path glob {raw}: {}", error.kind()))
}

/// Whether a path glob names the file or a folder above it.
fn glob_covers(glob: &globset::GlobMatcher, path: &str) -> bool {
    if glob.is_match(path) {
        return true;
    }
    let mut end = 0;
    while let Some(k) = path[end..].find('/') {
        end += k;
        if glob.is_match(&path[..end]) {
            return true;
        }
        end += 1;
    }
    false
}

/// A Rust regex split at its top-level `|`.
fn split_rust_alts(p: &str) -> Vec<String> {
    let c: Vec<char> = p.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut class) = (0usize, 0usize);
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
        if class > 0 {
            if ch == '[' {
                class += 1;
            } else if ch == ']' {
                class -= 1;
            }
        } else if ch == '[' {
            class = 1;
            cur.push(ch);
            i += 1;
            for skip in ['^', ']'] {
                if c.get(i) == Some(&skip) {
                    cur.push(skip);
                    i += 1;
                }
            }
            continue;
        } else if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth = depth.saturating_sub(1);
        } else if ch == '|' && depth == 0 {
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

fn cut_text(text: &str, notes: &mut BTreeMap<&'static str, ()>) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text.to_owned();
    }
    notes.insert("lines longer than 1000 characters are cut", ());
    let mut cut: String = text.chars().take(MAX_LINE_CHARS).collect();
    cut.push('…');
    cut
}

async fn search_inner(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &SearchArgs,
    budget: &Budget,
) -> Result<SearchResult> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let offset = args.offset as usize;
    let context = args.context.min(MAX_CONTEXT) as i32;
    if args.context > MAX_CONTEXT {
        bail!("context is at most {MAX_CONTEXT} lines");
    }
    let plan = plan(args)?;
    if args.history.is_some() {
        if args.output != OutputMode::Lines || args.group_by != GroupBy::File || args.context > 0 {
            bail!("a history search gives lines, without context or grouping by match");
        }
        if args
            .paths
            .to_vec()
            .iter()
            .any(|p| crate::snapshot::mount_of(p).is_some())
        {
            bail!("a history search takes paths from the root, such as src/backend");
        }
        let patterns = Patterns {
            query: &plan.query,
            also: &plan.also,
            not: plan.not.as_ref(),
        };
        return crate::history_search::search(client, snapshot, args, &patterns, budget).await;
    }
    let table = FileTable::view(client, snapshot, &args.paths.to_vec()).await?;
    let (files, whole) = search_scope(
        &table,
        &snapshot.repo,
        &args.paths.to_vec(),
        &args.include.to_vec(),
        &args.exclude.to_vec(),
    )?;
    if plan.every_line && plan.also.is_empty() && args.group_by == GroupBy::File {
        let scope = Scope {
            table: &table,
            files: &files,
            whole,
        };
        return complement(client, snapshot, args, &plan, &scope, budget).await;
    }
    let patterns = Patterns {
        query: &plan.query,
        also: &plan.also,
        not: plan.not.as_ref(),
    };
    let found = find_lines(client, snapshot, &table, &files, whole, &patterns, budget).await?;
    let mut notes: BTreeMap<&'static str, ()> = BTreeMap::new();
    let hits = found.hits;
    let total_lines = hits.len() as u64;
    let mut per_file: Vec<(usize, u64)> = Vec::new();
    for h in &hits {
        match per_file.last_mut() {
            Some(last) if last.0 == h.0 => last.1 += 1,
            _ => per_file.push((h.0, 1)),
        }
    }
    let total_files = per_file.len() as u64;

    // Coverage: the distinct query terms each file matches.
    let mut cover: HashMap<usize, u32> = HashMap::new();
    if !plan.terms.is_empty() {
        let mut seen: HashMap<usize, HashSet<usize>> = HashMap::new();
        for (file, _, line) in &hits {
            let set = seen.entry(*file).or_default();
            for (t, term) in plan.terms.iter().enumerate() {
                if !set.contains(&t) && term.matcher.is_match(line) {
                    set.insert(t);
                }
            }
        }
        cover = seen.into_iter().map(|(f, s)| (f, s.len() as u32)).collect();
        per_file.sort_by_key(|(f, _)| (std::cmp::Reverse(cover.get(f).copied().unwrap_or(0)), *f));
    }
    let file_rank: HashMap<usize, usize> = per_file
        .iter()
        .enumerate()
        .map(|(rank, (f, _))| (*f, rank))
        .collect();

    let mut result = SearchResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        total_lines,
        total_files,
        total_matches: None,
        offset: args.offset,
        returned: 0,
        cut: false,
        budget_exceeded: found.timed_out,
        complete: !found.timed_out && !found.capped,
        lines: Vec::new(),
        files: Vec::new(),
        matches: Vec::new(),
        similar: None,
        notes: Vec::new(),
        history: Vec::new(),
        range: None,
        elapsed_ms: 0,
    };
    let terms_of = |f: &usize| (!plan.terms.is_empty()).then(|| cover.get(f).copied().unwrap_or(0));
    match (args.output, args.group_by) {
        (OutputMode::Lines, GroupBy::File) => {
            let mut ordered: Vec<&Hit> = hits.iter().collect();
            if !plan.terms.is_empty() {
                ordered.sort_by_key(|h| (file_rank[&h.0], h.1));
            }
            let page: Vec<&Hit> = ordered.iter().skip(offset).take(limit).copied().collect();
            result.cut = offset + page.len() < ordered.len();
            let mut context_lines = HashMap::new();
            if context > 0 && !page.is_empty() {
                let ranges = page
                    .iter()
                    .map(|(f, n, _)| {
                        let last = table.files[*f].line_count;
                        (
                            table.files[*f].id,
                            (n - context).max(1),
                            (n + context).min(last),
                        )
                    })
                    .collect();
                context_lines = engine::line_ranges(
                    client,
                    snapshot.history.as_ref(),
                    &engine::merge_ranges(ranges),
                    budget,
                )
                .await?;
            }
            for (f, n, text) in page {
                let id = table.files[*f].id;
                let last = table.files[*f].line_count;
                let get = |k: i32| {
                    context_lines
                        .get(&(id, k))
                        .map(|t| cut_text(t, &mut BTreeMap::new()))
                };
                let before = if context > 0 {
                    ((n - context).max(1)..*n).filter_map(get).collect()
                } else {
                    Vec::new()
                };
                let after = if context > 0 {
                    (n + 1..=(n + context).min(last)).filter_map(get).collect()
                } else {
                    Vec::new()
                };
                result.lines.push(LineHit {
                    path: crate::snapshot::shown(&table.files[*f].path),
                    line: *n as u32,
                    text: cut_text(text, &mut notes),
                    before,
                    after,
                });
            }
        }
        (OutputMode::Lines, GroupBy::Match) => {
            let mut parts: Vec<(usize, i32, String)> = Vec::new();
            let mut ordered: Vec<&Hit> = hits.iter().collect();
            if !plan.terms.is_empty() {
                ordered.sort_by_key(|h| (file_rank[&h.0], h.1));
            }
            for (f, n, line) in ordered {
                if budget.exceeded() {
                    result.budget_exceeded = true;
                    break;
                }
                for (s, e) in plan.query.matcher.matches(line) {
                    parts.push((*f, *n, line[s..e].to_owned()));
                }
            }
            result.total_matches = Some(parts.len() as u64);
            result.cut = offset + limit < parts.len();
            for (f, n, text) in parts.into_iter().skip(offset).take(limit) {
                result.lines.push(LineHit {
                    path: crate::snapshot::shown(&table.files[f].path),
                    line: n as u32,
                    text: cut_text(&text, &mut notes),
                    before: Vec::new(),
                    after: Vec::new(),
                });
            }
        }
        (OutputMode::Files, _) | (OutputMode::Counts, GroupBy::File) => {
            result.cut = offset + limit < per_file.len();
            for (f, n) in per_file.iter().skip(offset).take(limit) {
                result.files.push(FileHit {
                    path: crate::snapshot::shown(&table.files[*f].path),
                    lines: *n,
                    terms: terms_of(f),
                });
            }
        }
        (OutputMode::Counts, GroupBy::Match) => {
            let mut counts: HashMap<String, u64> = HashMap::new();
            let mut total = 0;
            for (_, _, line) in &hits {
                if budget.exceeded() {
                    result.budget_exceeded = true;
                    break;
                }
                for (s, e) in plan.query.matcher.matches(line) {
                    *counts.entry(line[s..e].to_owned()).or_default() += 1;
                    total += 1;
                }
            }
            let mut counts: Vec<(String, u64)> = counts.into_iter().collect();
            counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            result.total_matches = Some(total);
            result.cut = offset + limit < counts.len();
            for (text, count) in counts.into_iter().skip(offset).take(limit) {
                result.matches.push(MatchCount {
                    text: cut_text(&text, &mut notes),
                    count,
                });
            }
        }
    }
    result.returned = (result.lines.len() + result.files.len() + result.matches.len()) as u32;
    if total_lines == 0 && !plan.every_line && result.complete && !budget.exceeded() {
        let (similar, timed_out) =
            similar_names(client, &table, &files, whole, snapshot, args, budget).await?;
        result.similar = similar;
        result.budget_exceeded |= timed_out;
    }
    if found.capped {
        notes.insert(
            "more than 200,000 candidate lines: narrow the search for exact totals",
            (),
        );
    }
    result.budget_exceeded |= budget.exceeded();
    result.complete &= !result.budget_exceeded;
    if result.budget_exceeded {
        notes.insert("time budget exhausted: totals are lower bounds", ());
    }
    result.notes = notes.into_keys().map(str::to_owned).collect();
    result.elapsed_ms = budget.elapsed_ms();
    Ok(result)
}

/// The files a search covers.
struct Scope<'a> {
    table: &'a FileTable,
    files: &'a [usize],
    whole: bool,
}

/// Every line in scope that matches none of the `not` patterns (grep -v),
/// as the complement of the lines that do match: per-file line counts give
/// the totals, and only the page's lines are read.
async fn complement(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &SearchArgs,
    plan: &Plan,
    scope: &Scope<'_>,
    budget: &Budget,
) -> Result<SearchResult> {
    let table = scope.table;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let offset = args.offset as usize;
    let context = args.context.min(MAX_CONTEXT) as i32;
    let mut matched: HashMap<usize, Vec<i32>> = HashMap::new();
    if let Some(not) = &plan.not {
        let patterns = Patterns {
            query: not,
            also: &[],
            not: None,
        };
        let found = find_lines(
            client,
            snapshot,
            table,
            scope.files,
            scope.whole,
            &patterns,
            budget,
        )
        .await?;
        if found.capped || found.timed_out {
            bail!(
                "cannot compute inverted results from an incomplete search: \
                 the time budget or 200,000 candidate-line cap was reached; narrow the search"
            );
        }
        for (k, n, _) in found.hits {
            matched.entry(k).or_default().push(n);
        }
    }
    let per_file: Vec<(usize, u64)> = scope
        .files
        .iter()
        .filter_map(|&k| {
            let total = table.files[k].line_count.max(0) as u64;
            let n = total - matched.get(&k).map_or(0, |m| m.len() as u64);
            (n > 0).then_some((k, n))
        })
        .collect();
    let total_lines: u64 = per_file.iter().map(|(_, n)| n).sum();
    let mut result = SearchResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        total_lines,
        total_files: per_file.len() as u64,
        total_matches: None,
        offset: args.offset,
        returned: 0,
        cut: false,
        budget_exceeded: false,
        complete: true,
        lines: Vec::new(),
        files: Vec::new(),
        matches: Vec::new(),
        similar: None,
        notes: Vec::new(),
        history: Vec::new(),
        range: None,
        elapsed_ms: 0,
    };
    let mut notes: BTreeMap<&'static str, ()> = BTreeMap::new();
    if args.output == OutputMode::Lines {
        let mut page: Vec<(usize, i32)> = Vec::new();
        let mut skip = offset as u64;
        for &(k, n) in &per_file {
            if page.len() == limit {
                break;
            }
            if skip >= n {
                skip -= n;
                continue;
            }
            let empty = Vec::new();
            let excluded = matched.get(&k).unwrap_or(&empty);
            let mut seen = 0;
            for line in 1..=table.files[k].line_count {
                if excluded.binary_search(&line).is_ok() {
                    continue;
                }
                if seen >= skip {
                    page.push((k, line));
                    if page.len() == limit {
                        break;
                    }
                }
                seen += 1;
            }
            skip = 0;
        }
        result.cut = ((offset + page.len()) as u64) < total_lines;
        let ranges = page
            .iter()
            .map(|&(k, n)| {
                let last = table.files[k].line_count;
                (
                    table.files[k].id,
                    (n - context).max(1),
                    (n + context).min(last),
                )
            })
            .collect();
        let text = engine::line_ranges(
            client,
            snapshot.history.as_ref(),
            &engine::merge_ranges(ranges),
            budget,
        )
        .await?;
        for (k, n) in page {
            let id = table.files[k].id;
            let last = table.files[k].line_count;
            let get = |l: i32| {
                text.get(&(id, l))
                    .map(|t| cut_text(t, &mut BTreeMap::new()))
            };
            result.lines.push(LineHit {
                path: crate::snapshot::shown(&table.files[k].path),
                line: n as u32,
                text: cut_text(text.get(&(id, n)).map_or("", String::as_str), &mut notes),
                before: ((n - context).max(1)..n).filter_map(get).collect(),
                after: (n + 1..=(n + context).min(last)).filter_map(get).collect(),
            });
        }
    } else {
        result.cut = offset + limit < per_file.len();
        for (k, n) in per_file.iter().skip(offset).take(limit) {
            result.files.push(FileHit {
                path: crate::snapshot::shown(&table.files[*k].path),
                lines: *n,
                terms: None,
            });
        }
    }
    result.returned = (result.lines.len() + result.files.len()) as u32;
    result.budget_exceeded = budget.exceeded();
    result.complete = !result.budget_exceeded;
    if result.budget_exceeded {
        notes.insert("time budget exhausted: totals are lower bounds", ());
    }
    result.notes = notes.into_keys().map(str::to_owned).collect();
    result.elapsed_ms = budget.elapsed_ms();
    Ok(result)
}

/// The query's name in other spellings, and similar names, for queries of
/// one to three names that found nothing.
async fn similar_names(
    client: &impl GenericClient,
    table: &FileTable,
    files: &[usize],
    whole: bool,
    snapshot: &Snapshot,
    args: &SearchArgs,
    budget: &Budget,
) -> Result<(Option<Similar>, bool)> {
    let names: Vec<String> = args
        .query
        .to_vec()
        .into_iter()
        .filter(|q| {
            let q = q.trim();
            q.chars().count() >= 4
                && q.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
                && q.chars()
                    .all(|c| c.is_alphanumeric() || "_$ -.".contains(c))
        })
        .take(3)
        .collect();
    if names.is_empty() {
        return Ok((None, false));
    }
    let ids: Vec<i32> = files.iter().map(|&k| table.files[k].id).collect();
    let scope = if whole {
        "l.file_id IN (SELECT id FROM code_files WHERE snapshot_id = $1)"
    } else {
        "l.file_id = ANY($1::int4[])"
    };
    // In the history, $1 is the commit (or the files) and $2 the pattern;
    // $3 is the files' lineages.
    let history = snapshot.history.as_ref();
    let lineages = match (history, whole) {
        (Some(h), false) => crate::history::lineages_of(client, h, &ids).await?,
        _ => Vec::new(),
    };
    let mut similar = Similar::default();
    let mut timed_out = false;
    for name in &names {
        let Some(id) = pattern::identifier_pattern(name) else {
            continue;
        };
        let spell_sql = match history {
            Some(h) => format!(
                "SELECT m[1], count(DISTINCT l.id) FROM {}, \
                 regexp_matches(l.line, $2, 'gi') m GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 5",
                crate::history::lines_matching(h, "l.line ~* $2", whole)?
            ),
            None => format!(
                "SELECT m[1], count(DISTINCT (l.file_id, l.line_no)) FROM code_lines l, \
                 regexp_matches(l.line, $2, 'gi') m WHERE {scope} AND l.line ~* $2 \
                 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 5"
            ),
        };
        if !budget.apply(client).await? {
            timed_out = true;
            break;
        }
        let rows = match (history, whole) {
            (Some(h), true) => client.query(spell_sql.as_str(), &[&h.addr, &id]).await,
            (Some(_), false) => {
                client
                    .query(spell_sql.as_str(), &[&ids, &id, &lineages])
                    .await
            }
            (None, true) => client.query(spell_sql.as_str(), &[&snapshot.id, &id]).await,
            (None, false) => client.query(spell_sql.as_str(), &[&ids, &id]).await,
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) if engine::is_timeout(&error) => {
                timed_out = true;
                break;
            }
            Err(error) => return Err(error).context("cannot find alternative name spellings"),
        };
        for row in rows {
            let spelling: String = row.get(0);
            if spelling != *name {
                similar.spellings.push(NameCount {
                    name: spelling,
                    lines: row.get::<_, i64>(1) as u64,
                    similarity: None,
                });
            }
        }
        let word = name.trim();
        let source = match history {
            Some(h) => crate::history::lines_matching(h, "l.line %> $2", whole)?,
            None => format!("code_lines l WHERE {scope} AND l.line %> $2"),
        };
        let sim_sql = format!(
            "SELECT w, round(similarity(w, $2)::numeric, 2)::float8, count(*) FROM \
             (SELECT (regexp_matches(l.line, '[A-Za-z_$][A-Za-z0-9_$]*', 'g'))[1] AS w \
              FROM {source}) x \
             WHERE similarity(w, $2) >= 0.5 AND w <> $2 GROUP BY w ORDER BY 2 DESC, 3 DESC, 1 LIMIT 5"
        );
        if !budget.apply(client).await? {
            timed_out = true;
            break;
        }
        let rows = match (history, whole) {
            (Some(h), true) => client.query(sim_sql.as_str(), &[&h.addr, &word]).await,
            (Some(_), false) => {
                client
                    .query(sim_sql.as_str(), &[&ids, &word, &lineages])
                    .await
            }
            (None, true) => client.query(sim_sql.as_str(), &[&snapshot.id, &word]).await,
            (None, false) => client.query(sim_sql.as_str(), &[&ids, &word]).await,
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) if engine::is_timeout(&error) => {
                timed_out = true;
                break;
            }
            Err(error) => return Err(error).context("cannot find similar names"),
        };
        for row in rows {
            let w: String = row.get(0);
            if !similar.names.iter().any(|n| n.name == w) {
                similar.names.push(NameCount {
                    name: w,
                    lines: row.get::<_, i64>(2) as u64,
                    similarity: Some(row.get(1)),
                });
            }
        }
    }
    Ok((
        (!similar.spellings.is_empty() || !similar.names.is_empty()).then_some(similar),
        timed_out,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_rust_alternatives() {
        assert_eq!(
            split_rust_alts("a|b(c|d)|[|]e"),
            vec!["a", "b(c|d)", "[|]e"]
        );
        assert_eq!(split_rust_alts(r"x\|y|[]|]"), vec![r"x\|y", "[]|]"]);
        assert_eq!(split_rust_alts("[[a|b]c]|d"), vec!["[[a|b]c]", "d"]);
    }

    #[test]
    fn args_deserialize_with_defaults() {
        let args: SearchArgs = serde_json::from_value(serde_json::json!({
            "query": ["a", "b"], "match": "identifier", "paths": "src/backend",
            "output": "counts", "group_by": "match", "order": "coverage"
        }))
        .unwrap();
        assert_eq!(args.query.to_vec(), vec!["a", "b"]);
        assert_eq!(args.match_mode, MatchMode::Identifier);
        assert_eq!(args.paths.to_vec(), vec!["src/backend"]);
        assert_eq!(args.output, OutputMode::Counts);
        let error = serde_json::from_value::<SearchArgs>(serde_json::json!({
            "query": "a", "glob": "*.c"
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `glob`"));
    }
}
