// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! search_commits: commits as git log finds them in the history, newest
//! first: in a range (or on every branch), by message, author, files and
//! dates, and by their diffs, as git log -G finds commits that added or
//! removed a line matching a pattern and git log -S those that changed how
//! often a text occurs in a file.
//!
//! A diff filter reads the line instances whose text matches (through the
//! trigram index): each was added by the commit at its birth and removed by
//! each commit at the end of one of its alive spans.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

use crate::commit::PathFilter;
use crate::engine::{Budget, is_timeout};
use crate::history::{Spans, checked};
use crate::pattern::are::escape_like;
use crate::range::{MASTER_STEP, Range, death_at};
use crate::sqlutil::{Params, path_sql, when};

pub const DEFAULT_COMMITS: u32 = 30;
pub const MAX_COMMITS: u32 = 200;
/// The most line instances a diff filter reads.
const MAX_INSTANCES: usize = 300_000;
/// The most matching lines shown per commit.
const LINES_PER_COMMIT: usize = 10;

/// The search_commits tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchCommitsArgs {
    /// The commits to search: a revision and its ancestors (master, REL_17_STABLE, REL_16_3, a SHA) or a range A..B (B's ancestors that A lacks), as git log takes them. Default: every branch.
    #[serde(default)]
    pub range: Option<String>,
    /// A regular expression the commit message matches, case-insensitively.
    #[serde(default)]
    pub message: Option<String>,
    /// A name, case-insensitively, that the commit's author or an Author or Co-authored-by trailer contains.
    #[serde(default)]
    pub author: Option<String>,
    /// Only commits that changed these files or folders, from the source root, or globs such as *.y.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Committed on or after this day (YYYY-MM-DD) or time (RFC 3339).
    #[serde(default)]
    pub since: Option<String>,
    /// Committed on or before this day (YYYY-MM-DD) or before this time (RFC 3339).
    #[serde(default)]
    pub until: Option<String>,
    /// As git log -G: commits that added or removed a line matching this regular expression.
    #[serde(default)]
    pub changed_lines: Option<String>,
    /// As git log -S: commits that changed how many times this text occurs in a file.
    #[serde(default)]
    pub occurrences: Option<String>,
    /// Take `occurrences` as a regular expression (git log -S --pickaxe-regex).
    #[serde(default)]
    pub occurrences_regex: bool,
    /// Match changed_lines and occurrences ignoring case.
    #[serde(default)]
    pub ignore_case: bool,
    /// The most commits, newest first. Default 30, at most 200.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Time budget in milliseconds. Default 10000, at most 15000.
    #[serde(default)]
    pub budget_ms: Option<u32>,
}

/// A line a commit added or removed that the diff filter matched.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct DiffLine {
    pub path: String,
    /// "-" removed or "+" added.
    pub sign: String,
    pub text: String,
}

/// A commit found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitHit {
    pub sha: String,
    pub date: String,
    pub author: String,
    pub branch: String,
    pub subject: String,
    pub files: i64,
    pub added: i64,
    pub removed: i64,
    /// Message-IDs its Discussion trailers name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub discussion: Vec<String>,
    /// pgsql-hackers threads that discussed it (get_thread reads one).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<i64>,
    /// Other branches that got the same change.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backpatched_to: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<DiffLine>,
    #[serde(skip_serializing_if = "is_zero")]
    pub more_lines: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// search_commits' answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchCommitsResult {
    pub repo: String,
    /// The commits searched.
    pub range: String,
    pub commits: Vec<CommitHit>,
    /// More commits match than were returned.
    pub more: bool,
    /// The search stopped early (too many matching lines, or the time
    /// budget): older commits may be missing.
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// SQL that `col` (a commit address) is in the range.
fn range_sql(r: &Range, col: &str) -> String {
    let mut parts = Vec::new();
    if let Some((after, upto)) = r.master {
        parts.push(format!(
            "({col} % {MASTER_STEP} = 0 AND {col} > {after} AND {col} <= {upto})"
        ));
    }
    if let Some((from, to)) = r.branch {
        parts.push(format!("({col} BETWEEN {from} AND {to})"));
    }
    if parts.is_empty() {
        "false".into()
    } else {
        format!("({})", parts.join(" OR "))
    }
}

/// The commits a range names, as git log takes them, and how it reads.
async fn range_of(
    client: &impl GenericClient,
    s: &str,
    repo: &str,
    spec: Option<&str>,
) -> Result<(Option<Range>, String)> {
    let Some(spec) = spec.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok((None, "every branch".into()));
    };
    let addr = |rev: Option<String>| async move {
        crate::rev::resolve(client, s, repo, rev.as_deref())
            .await
            .map(|snapshot| snapshot.history.addr)
    };
    let range = match spec.split_once("..") {
        Some((since, upto)) => {
            let since = since.trim();
            if since.is_empty() {
                bail!("a range needs a start: A..B");
            }
            let upto = upto.trim().trim_start_matches('.');
            let upto = (!upto.is_empty()).then(|| upto.to_owned());
            Range::between(Some(addr(Some(since.to_owned())).await?), addr(upto).await?)
        }
        None => Range::ancestors(addr(Some(spec.to_owned())).await?),
    };
    Ok((Some(range), spec.to_owned()))
}

/// Per commit, the matching lines it added ('+') or removed ('-'), by
/// lineage.
type Events = HashMap<i64, Vec<(i32, char, String)>>;

/// A line instance a diff filter matched, and the commits that added and
/// removed it.
struct Matched {
    lineage: i32,
    line: String,
    added: i64,
    removed: Vec<i64>,
}

/// Searches the history in `schema` for commits.
pub async fn search_commits(
    client: &impl GenericClient,
    schema: &str,
    repo: &str,
    args: &SearchCommitsArgs,
) -> Result<SearchCommitsResult> {
    let budget = Budget::new(args.budget_ms);
    let old = crate::engine::set_timeout(client, budget.remaining_ms()).await?;
    let result = search_commits_inner(client, schema, repo, args, &budget).await;
    crate::engine::restore_timeout(client, &old).await;
    result
}

async fn search_commits_inner(
    client: &impl GenericClient,
    schema: &str,
    repo: &str,
    args: &SearchCommitsArgs,
    budget: &Budget,
) -> Result<SearchCommitsResult> {
    let s = checked(schema)?;
    let limit = args.limit.unwrap_or(DEFAULT_COMMITS).clamp(1, MAX_COMMITS) as usize;
    let (range, range_text) = range_of(client, s, repo, args.range.as_deref()).await?;
    let mut result = SearchCommitsResult {
        repo: repo.to_owned(),
        range: range_text,
        commits: Vec::new(),
        more: false,
        incomplete: false,
        notes: Vec::new(),
    };
    let filter = PathFilter::new(&args.paths)?;
    let changed = args.changed_lines.as_deref().filter(|p| !p.is_empty());
    let occurrences = args.occurrences.as_deref().filter(|p| !p.is_empty());
    if changed.is_some() && occurrences.is_some() {
        bail!("give changed_lines or occurrences, not both");
    }

    // Diff filters: the commits that added or removed matching lines.
    let mut events: Option<Events> = None;
    if let Some(pattern) = changed.or(occurrences) {
        let mut params = Params::default();
        let cond = match (changed, args.occurrences_regex) {
            (Some(_), _) | (None, true) => {
                let p = params.add(pattern.to_owned());
                let op = if args.ignore_case { "~*" } else { "~" };
                format!("l.line {op} {p}")
            }
            (None, false) => {
                let p = params.add(format!("%{}%", escape_like(pattern)));
                let op = if args.ignore_case { "ILIKE" } else { "LIKE" };
                format!("l.line {op} {p}")
            }
        };
        // The paths' lineages, bound as an array (see history::lineages_of).
        let mut path_params = Params::default();
        let scope = match path_sql(&args.paths, "v.path", &mut path_params) {
            Some(paths) => {
                if !budget.apply(client).await? {
                    bail!("the time budget ran out");
                }
                let sql = format!(
                    "SELECT DISTINCT v.lineage FROM {s}.code_file_versions v WHERE {paths}"
                );
                let lineages: Vec<i32> = match client.query(sql.as_str(), &path_params.refs()).await
                {
                    Ok(rows) => rows.iter().map(|r| r.get(0)).collect(),
                    Err(error) if is_timeout(&error) => {
                        result.incomplete = true;
                        result
                            .notes
                            .push("the time budget ran out reading the paths' files".into());
                        return Ok(result);
                    }
                    Err(error) => return Err(error).context("cannot read the paths' files"),
                };
                format!(" AND l.lineage = ANY({}::int4[])", params.add(lineages))
            }
            None => String::new(),
        };
        let sql = format!(
            "SELECT l.lineage, l.lo, l.hi, l.alive::text, l.line FROM {s}.code_line_instances l \
             WHERE {cond}{scope} LIMIT {}",
            MAX_INSTANCES + 1
        );
        if !budget.apply(client).await? {
            bail!("the time budget ran out");
        }
        let rows = match client.query(sql.as_str(), &params.refs()).await {
            Ok(rows) => rows,
            Err(error) if is_timeout(&error) => {
                result.incomplete = true;
                result
                    .notes
                    .push("the time budget ran out reading matching lines: give a longer pattern or paths".into());
                return Ok(result);
            }
            Err(error) => return Err(error).context("cannot read the matching lines"),
        };
        if rows.len() > MAX_INSTANCES {
            result.incomplete = true;
            result.notes.push(format!(
                "more than {MAX_INSTANCES} matching lines: commits may be missing; narrow the pattern or the paths"
            ));
        }
        let mut matched = Vec::with_capacity(rows.len());
        for r in rows.iter().take(MAX_INSTANCES) {
            let (lo, hi): (i64, i64) = (r.get(1), r.get(2));
            let spans = match r.get::<_, Option<String>>(3) {
                Some(text) => Spans::parse(&text)?,
                None => Spans(vec![(lo, hi)]),
            };
            matched.push(Matched {
                lineage: r.get(0),
                line: r.get(4),
                added: lo,
                removed: spans.0.iter().filter_map(|&(_, b)| death_at(b)).collect(),
            });
        }
        let in_range = |a: i64| range.as_ref().is_none_or(|r| r.contains(a));
        let mut by_commit: Events = HashMap::new();
        for m in &matched {
            if in_range(m.added) {
                by_commit
                    .entry(m.added)
                    .or_default()
                    .push((m.lineage, '+', m.line.clone()));
            }
            for &d in m.removed.iter().filter(|&&d| in_range(d)) {
                by_commit
                    .entry(d)
                    .or_default()
                    .push((m.lineage, '-', m.line.clone()));
            }
        }
        if let Some(text) = occurrences {
            // A file's count changes when its added and removed lines hold
            // different numbers of occurrences.
            let count: Box<dyn Fn(&str) -> usize> = if args.occurrences_regex {
                let re = RegexBuilder::new(text)
                    .case_insensitive(args.ignore_case)
                    .build()
                    .map_err(|e| anyhow!("occurrences is not a regular expression: {e}"))?;
                Box::new(move |line: &str| re.find_iter(line).count())
            } else if args.ignore_case {
                let lower = text.to_lowercase();
                Box::new(move |line: &str| line.to_lowercase().matches(lower.as_str()).count())
            } else {
                let text = text.to_owned();
                Box::new(move |line: &str| line.matches(text.as_str()).count())
            };
            by_commit.retain(|_, lines| {
                let mut net: HashMap<i32, i64> = HashMap::new();
                for (lineage, sign, line) in lines.iter() {
                    let n = count(line) as i64;
                    *net.entry(*lineage).or_default() += if *sign == '+' { n } else { -n };
                }
                lines.retain(|(lineage, _, _)| net[lineage] != 0);
                !lines.is_empty()
            });
        }
        events = Some(by_commit);
    }

    // The files the diff filters' lines are in, at each commit.
    let mut paths_at: HashMap<(i64, i32), String> = HashMap::new();
    if let Some(by_commit) = &mut events {
        let addrs: Vec<i64> = by_commit.keys().copied().collect();
        if !addrs.is_empty() {
            for r in client
                .query(
                    &format!(
                        "SELECT f.addr, coalesce(v.lineage, o.lineage), f.path \
                         FROM {s}.code_commit_files f \
                         LEFT JOIN {s}.code_file_versions v ON v.id = f.version \
                         LEFT JOIN {s}.code_file_versions o ON o.id = f.old_version \
                         WHERE f.addr = ANY($1)"
                    ),
                    &[&addrs],
                )
                .await?
            {
                paths_at.insert((r.get(0), r.get(1)), r.get(2));
            }
        }
        // Only lines in the paths asked for.
        by_commit.retain(|addr, lines| {
            lines.retain(|(lineage, _, _)| {
                paths_at
                    .get(&(*addr, *lineage))
                    .is_some_and(|p| filter.covers(p))
            });
            !lines.is_empty()
        });
    }

    // The commits, newest first.
    let mut params = Params::default();
    let mut conds: Vec<String> = Vec::new();
    if let Some(r) = &range {
        conds.push(range_sql(r, "c.addr"));
    }
    if let Some(since) = args.since.as_deref().filter(|v| !v.trim().is_empty()) {
        let p = params.add(when(since, false)?);
        conds.push(format!("c.committed_at >= {p}"));
    }
    if let Some(until) = args.until.as_deref().filter(|v| !v.trim().is_empty()) {
        let p = params.add(when(until, true)?);
        conds.push(format!("c.committed_at < {p}"));
    }
    if let Some(message) = args.message.as_deref().filter(|v| !v.is_empty()) {
        let p = params.add(message.to_owned());
        conds.push(format!("c.message ~* {p}"));
    }
    if let Some(author) = args
        .author
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let p = params.add(format!("%{}%", escape_like(author)));
        conds.push(format!(
            "(c.author_name ILIKE {p} OR EXISTS (SELECT FROM jsonb_each(c.trailers) t, \
             jsonb_array_elements_text(t.value) person \
             WHERE t.key IN ('Author', 'Co-authored-by') AND person ILIKE {p}))"
        ));
    }
    match &events {
        Some(by_commit) => {
            let addrs: Vec<i64> = by_commit.keys().copied().collect();
            let p = params.add(addrs);
            conds.push(format!("c.addr = ANY({p})"));
        }
        None => {
            if let Some(paths) = path_sql(&args.paths, "f.path", &mut params) {
                let old = path_sql(&args.paths, "f.old_path", &mut params).expect("paths");
                conds.push(format!(
                    "EXISTS (SELECT FROM {s}.code_commit_files f \
                     WHERE f.addr = c.addr AND ({paths} OR {old}))"
                ));
            }
        }
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    let sql = format!(
        "SELECT c.addr, c.sha, to_char(c.committed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), \
                c.author_name, l.name, c.subject, c.trailers, c.backpatch \
         FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane \
         {where_sql} ORDER BY c.committed_at DESC, c.addr DESC LIMIT {}",
        limit + 1
    );
    if !budget.apply(client).await? {
        result.incomplete = true;
        result.notes.push("the time budget ran out".into());
        return Ok(result);
    }
    let rows = match client.query(sql.as_str(), &params.refs()).await {
        Ok(rows) => rows,
        Err(error) if is_timeout(&error) => {
            result.incomplete = true;
            result
                .notes
                .push("the time budget ran out finding commits: narrow the search".into());
            return Ok(result);
        }
        Err(error) => return Err(error).context("cannot search the commits"),
    };
    result.more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let addrs: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();

    let mut stats: HashMap<i64, (i64, i64, i64)> = HashMap::new();
    for r in client
        .query(
            &format!(
                "SELECT addr, count(*), coalesce(sum(added), 0)::int8, coalesce(sum(removed), 0)::int8 \
                 FROM {s}.code_commit_files WHERE addr = ANY($1) GROUP BY addr"
            ),
            &[&addrs],
        )
        .await?
    {
        stats.insert(r.get(0), (r.get(1), r.get(2), r.get(3)));
    }
    let groups: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get::<_, Option<i64>>(7))
        .collect();
    let mut lanes_of: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    if !groups.is_empty() {
        for r in client
            .query(
                &format!(
                    "SELECT c.backpatch, c.addr, l.name FROM {s}.code_commits c \
                     JOIN {s}.code_lanes l ON l.id = c.lane WHERE c.backpatch = ANY($1) \
                     ORDER BY c.committed_at, c.addr"
                ),
                &[&groups],
            )
            .await?
        {
            lanes_of
                .entry(r.get(0))
                .or_default()
                .push((r.get(1), r.get(2)));
        }
    }
    let shas: Vec<String> = rows.iter().map(|r| r.get(1)).collect();
    let mut threads_of: HashMap<String, Vec<i64>> = HashMap::new();
    for r in client
        .query(
            &format!(
                "SELECT sha, array_agg(DISTINCT thread_id ORDER BY thread_id) \
                 FROM {s}.code_commit_threads WHERE sha = ANY($1) GROUP BY sha"
            ),
            &[&shas],
        )
        .await?
    {
        threads_of.insert(r.get(0), r.get(1));
    }
    for r in rows {
        let addr: i64 = r.get(0);
        let branch: String = r.get(4);
        let trailers: BTreeMap<String, Vec<String>> =
            serde_json::from_value(r.get::<_, serde_json::Value>(6)).unwrap_or_default();
        let mut backpatched_to: Vec<String> = Vec::new();
        if let Some(group) = r.get::<_, Option<i64>>(7) {
            for (a, lane) in lanes_of.get(&group).into_iter().flatten() {
                if *a != addr && *lane != branch && !backpatched_to.contains(lane) {
                    backpatched_to.push(lane.clone());
                }
            }
        }
        let mut lines: Vec<DiffLine> = events
            .as_ref()
            .and_then(|e| e.get(&addr))
            .into_iter()
            .flatten()
            .map(|(lineage, sign, text)| DiffLine {
                path: paths_at.get(&(addr, *lineage)).cloned().unwrap_or_default(),
                sign: sign.to_string(),
                text: text.clone(),
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        lines.sort();
        let more_lines = lines.len().saturating_sub(LINES_PER_COMMIT);
        lines.truncate(LINES_PER_COMMIT);
        let (files, added, removed) = stats.get(&addr).copied().unwrap_or_default();
        result.commits.push(CommitHit {
            sha: r.get(1),
            date: r.get(2),
            author: r.get(3),
            branch,
            subject: r.get(5),
            files,
            added,
            removed,
            discussion: trailers
                .get("Discussion")
                .into_iter()
                .flatten()
                .filter_map(|v| pghacker_core::text::discussion_message_id(v))
                .collect(),
            threads: threads_of
                .remove(&r.get::<_, String>(1))
                .unwrap_or_default(),
            backpatched_to,
            lines,
            more_lines,
        });
    }
    Ok(result)
}
