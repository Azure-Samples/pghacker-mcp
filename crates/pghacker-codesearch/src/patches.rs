//! search_patches: lines of the patches posted to pgsql-hackers, found
//! through patch_texts (each distinct line once, by kind) and each patch's
//! text_ids, grouped by thread with the first and latest patch version that
//! has them, the files they are in, and, with the commit history, the linked
//! commits that added them.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

use crate::engine::{Budget, is_timeout};
use crate::pattern::are::escape_like;
use crate::sqlutil::{Params, path_sql, when};

pub const DEFAULT_THREADS: u32 = 20;
pub const MAX_THREADS: u32 = 100;
/// The most distinct matching lines read.
const MAX_TEXTS: i64 = 50_000;
/// The most patches read.
const MAX_PATCHES: i64 = 20_000;
/// The most matching lines shown per thread.
const LINES_PER_THREAD: usize = 10;

/// The search_patches tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchPatchesArgs {
    /// What to find in patch lines.
    pub query: String,
    /// How query matches: "text" (default, a substring), "regex" (a regular expression) or "identifier" (a whole word).
    #[serde(default, rename = "match")]
    pub match_mode: Option<String>,
    /// Match case. Default false.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Which lines: "changed" (default: added or removed), "added", "removed", "context" or "all" (with headers and text around the diffs).
    #[serde(default)]
    pub lines: Option<String>,
    /// Only patches touching these files or folders, from the source root, or globs such as *.y.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only patches posted by this person (a name, case-insensitively).
    #[serde(default)]
    pub author: Option<String>,
    /// Posted on or after this day (YYYY-MM-DD).
    #[serde(default)]
    pub since: Option<String>,
    /// Posted on or before this day (YYYY-MM-DD).
    #[serde(default)]
    pub until: Option<String>,
    /// Only this thread's patches.
    #[serde(default)]
    pub thread_id: Option<i64>,
    /// The most threads, newest first. Default 20, at most 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Time budget in milliseconds. Default 10000.
    #[serde(default)]
    pub budget_ms: Option<u32>,
}

/// A patch file in a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PatchRef {
    pub message_id: String,
    pub date: String,
    pub author: String,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i32>,
}

/// A matching line, as the latest patch with it has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PatchLineHit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// "+" added, "-" removed, " " context, or "" for other lines.
    pub sign: String,
    pub text: String,
    /// Its line number in that patch file.
    pub line: u32,
}

/// A commit linked to the thread that added a matching line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Landed {
    pub sha: String,
    pub date: String,
    pub branch: String,
    pub subject: String,
}

/// A thread's patches with matching lines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ThreadHit {
    pub thread_id: i64,
    pub subject: String,
    /// Patch files with a matching line.
    pub patches: usize,
    pub first: PatchRef,
    pub latest: PatchRef,
    pub lines: Vec<PatchLineHit>,
    #[serde(skip_serializing_if = "is_zero")]
    pub more_lines: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub landed: Vec<Landed>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// search_patches' answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchPatchesResult {
    pub threads: Vec<ThreadHit>,
    /// More threads match than were returned.
    pub more: bool,
    /// Some matches may be missing: too many matching lines or patches, the
    /// time budget, or patches not yet indexed.
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A regular expression's metacharacters escaped, for ARE.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A patch file found, with where its matching lines are.
struct Found {
    id: i64,
    thread: i64,
    subject: String,
    patch: PatchRef,
    sent: Option<chrono::DateTime<chrono::Utc>>,
    /// (line number from 1, text id)
    at: Vec<(i64, i32)>,
}

/// Searches the patches; with `history` (a history schema), names the
/// linked commits that added matching lines.
pub async fn search_patches(
    client: &impl GenericClient,
    history: Option<&str>,
    args: &SearchPatchesArgs,
) -> Result<SearchPatchesResult> {
    let query = args.query.trim();
    if query.is_empty() {
        bail!("give a query");
    }
    let budget = Budget::new(args.budget_ms);
    let limit = args.limit.unwrap_or(DEFAULT_THREADS).clamp(1, MAX_THREADS) as usize;
    let mut result = SearchPatchesResult {
        threads: Vec::new(),
        more: false,
        incomplete: false,
        notes: Vec::new(),
    };
    let kinds: Vec<String> = match args.lines.as_deref().unwrap_or("changed") {
        "changed" => vec!["+".into(), "-".into()],
        "added" => vec!["+".into()],
        "removed" => vec!["-".into()],
        "context" => vec![" ".into()],
        "all" => vec!["+".into(), "-".into(), " ".into(), "h".into()],
        other => bail!("lines is changed, added, removed, context or all, not {other:?}"),
    };

    // The matching lines.
    let mut params = Params::default();
    let k = params.add(kinds);
    let cond = match args.match_mode.as_deref().unwrap_or("text") {
        "text" => {
            let p = params.add(format!("%{}%", escape_like(query)));
            let op = if args.case_sensitive { "LIKE" } else { "ILIKE" };
            format!("t.line {op} {p}")
        }
        "regex" => {
            let p = params.add(query.to_owned());
            let op = if args.case_sensitive { "~" } else { "~*" };
            format!("t.line {op} {p}")
        }
        "identifier" => {
            let p = params.add(format!("\\m{}\\M", escape_regex(query)));
            let op = if args.case_sensitive { "~" } else { "~*" };
            // The literal narrows through the trigram index first.
            let lit = params.add(format!("%{}%", escape_like(query)));
            format!("t.line ILIKE {lit} AND t.line {op} {p}")
        }
        other => bail!("match is text, regex or identifier, not {other:?}"),
    };
    if !budget.apply(client).await? {
        bail!("the time budget ran out");
    }
    let texts = match client
        .query(
            &format!(
                "SELECT t.id, t.kind, t.line FROM patch_texts t WHERE t.kind = ANY({k}) AND {cond} \
                 LIMIT {}",
                MAX_TEXTS + 1
            ),
            &params.refs(),
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) if is_timeout(&error) => {
            result.incomplete = true;
            result
                .notes
                .push("the time budget ran out finding lines: give a longer query".into());
            return Ok(result);
        }
        Err(error) => return Err(error).context("cannot search the patch lines"),
    };
    if texts.len() as i64 > MAX_TEXTS {
        result.incomplete = true;
        result.notes.push(format!(
            "more than {MAX_TEXTS} distinct matching lines: give a longer query"
        ));
    }
    let texts: HashMap<i32, (String, String)> = texts
        .iter()
        .take(MAX_TEXTS as usize)
        .map(|r| (r.get(0), (r.get(1), r.get(2))))
        .collect();
    if texts.is_empty() {
        unindexed_note(client, &mut result).await?;
        return Ok(result);
    }

    // The patches with them, newest first.
    let mut params = Params::default();
    let ids: Vec<i32> = texts.keys().copied().collect();
    let ids_p = params.add(ids);
    let mut conds = vec![format!("p.text_ids && {ids_p}")];
    if let Some(paths) = path_sql(&args.paths, "pp.path", &mut params) {
        conds.push(format!(
            "EXISTS (SELECT FROM unnest(p.paths) pp(path) WHERE {paths})"
        ));
    }
    if let Some(author) = args
        .author
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let p = params.add(format!("%{}%", escape_like(author)));
        conds.push(format!("m.from_name ILIKE {p}"));
    }
    if let Some(since) = args.since.as_deref().filter(|v| !v.trim().is_empty()) {
        let p = params.add(when(since, false)?);
        conds.push(format!("m.sent_at >= {p}"));
    }
    if let Some(until) = args.until.as_deref().filter(|v| !v.trim().is_empty()) {
        let p = params.add(when(until, true)?);
        conds.push(format!("m.sent_at < {p}"));
    }
    if let Some(thread) = args.thread_id {
        let p = params.add(thread);
        conds.push(format!("m.thread_id = {p}"));
    }
    if !budget.apply(client).await? {
        result.incomplete = true;
        return Ok(result);
    }
    let rows = match client
        .query(
            &format!(
                "SELECT p.id, m.thread_id, th.subject, m.message_id, \
                        to_char(m.sent_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), m.from_name, \
                        p.filename, p.series_version, m.sent_at, \
                        ARRAY(SELECT u.n FROM unnest(p.text_ids) WITH ORDINALITY AS u(id, n) \
                              WHERE u.id = ANY({ids_p}) ORDER BY u.n), \
                        ARRAY(SELECT u.id FROM unnest(p.text_ids) WITH ORDINALITY AS u(id, n) \
                              WHERE u.id = ANY({ids_p}) ORDER BY u.n) \
                 FROM patches p JOIN messages m ON m.id = p.message_id \
                 JOIN threads th ON th.id = m.thread_id \
                 WHERE {} ORDER BY m.sent_at DESC NULLS LAST, p.id DESC LIMIT {}",
                conds.join(" AND "),
                MAX_PATCHES + 1
            ),
            &params.refs(),
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) if is_timeout(&error) => {
            result.incomplete = true;
            result
                .notes
                .push("the time budget ran out reading patches: narrow the search".into());
            return Ok(result);
        }
        Err(error) => return Err(error).context("cannot read the patches"),
    };
    if rows.len() as i64 > MAX_PATCHES {
        result.incomplete = true;
        result.notes.push(format!(
            "more than {MAX_PATCHES} patches match: older ones are left out"
        ));
    }
    let found: Vec<Found> = rows
        .iter()
        .take(MAX_PATCHES as usize)
        .map(|r| {
            let at: Vec<i64> = r.get(9);
            let ids: Vec<i32> = r.get(10);
            Found {
                id: r.get(0),
                thread: r.get(1),
                subject: r.get(2),
                patch: PatchRef {
                    message_id: r.get(3),
                    date: r.get::<_, Option<String>>(4).unwrap_or_default(),
                    author: r.get(5),
                    filename: r.get(6),
                    version: r.get(7),
                },
                sent: r.get(8),
                at: at.into_iter().zip(ids).collect(),
            }
        })
        .collect();

    // By thread, the thread with the newest patch first.
    let mut order: Vec<i64> = Vec::new();
    let mut by_thread: HashMap<i64, Vec<&Found>> = HashMap::new();
    for f in &found {
        if !by_thread.contains_key(&f.thread) {
            order.push(f.thread);
        }
        by_thread.entry(f.thread).or_default().push(f);
    }
    result.more = order.len() > limit;
    order.truncate(limit);

    // Each thread's matching lines, from its latest patch that has each.
    let mut contents: HashMap<i64, String> = HashMap::new();
    let wanted: Vec<i64> = order
        .iter()
        .flat_map(|t| by_thread[t].iter().map(|f| f.id))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for r in client
        .query(
            "SELECT id, content FROM patches WHERE id = ANY($1)",
            &[&wanted],
        )
        .await?
    {
        contents.insert(r.get(0), r.get(1));
    }
    for thread in &order {
        let patches = &by_thread[thread];
        let latest = patches[0];
        let first = patches
            .iter()
            .min_by_key(|f| (f.sent, f.id))
            .expect("a thread has a patch");
        let mut seen: HashSet<i32> = HashSet::new();
        let mut lines: Vec<PatchLineHit> = Vec::new();
        for f in patches.iter() {
            let Some(content) = contents.get(&f.id) else {
                continue;
            };
            let (stats, parsed) = pghacker_mail::patch::parse_diff(content);
            for &(n, id) in &f.at {
                if !seen.insert(id) {
                    continue;
                }
                let (kind, text) = &texts[&id];
                let path = parsed
                    .get((n - 1) as usize)
                    .and_then(|l| l.file)
                    .and_then(|k| stats.paths.get(k).cloned());
                lines.push(PatchLineHit {
                    path,
                    sign: if kind == "h" {
                        String::new()
                    } else {
                        kind.clone()
                    },
                    text: text.clone(),
                    line: n as u32,
                });
            }
        }
        let more_lines = lines.len().saturating_sub(LINES_PER_THREAD);
        lines.truncate(LINES_PER_THREAD);
        result.threads.push(ThreadHit {
            thread_id: *thread,
            subject: latest.subject.clone(),
            patches: patches.len(),
            first: first.patch.clone(),
            latest: latest.patch.clone(),
            lines,
            more_lines,
            landed: Vec::new(),
        });
    }

    if let Some(schema) = history {
        landed(client, schema, &mut result).await?;
    }
    if result.threads.is_empty() {
        unindexed_note(client, &mut result).await?;
    }
    Ok(result)
}

/// The linked commits that added a thread's matching added lines.
async fn landed(
    client: &impl GenericClient,
    schema: &str,
    result: &mut SearchPatchesResult,
) -> Result<()> {
    let s = crate::history::checked(schema)?;
    let threads: Vec<i64> = result.threads.iter().map(|t| t.thread_id).collect();
    let mut commits: HashMap<i64, Vec<(i64, Landed)>> = HashMap::new();
    for r in client
        .query(
            &format!(
                "SELECT DISTINCT t.thread_id, c.addr, c.sha, \
                        to_char(c.committed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), l.name, c.subject \
                 FROM {s}.code_commit_threads t JOIN {s}.code_commits c ON c.sha = t.sha \
                 JOIN {s}.code_lanes l ON l.id = c.lane WHERE t.thread_id = ANY($1)"
            ),
            &[&threads],
        )
        .await?
    {
        commits.entry(r.get(0)).or_default().push((
            r.get(1),
            Landed {
                sha: r.get(2),
                date: r.get(3),
                branch: r.get(4),
                subject: r.get(5),
            },
        ));
    }
    for t in &mut result.threads {
        let Some(linked) = commits.get(&t.thread_id) else {
            continue;
        };
        let added: Vec<&str> = t
            .lines
            .iter()
            .filter(|l| l.sign == "+")
            .map(|l| l.text.as_str())
            .collect();
        if added.is_empty() {
            continue;
        }
        let addrs: Vec<i64> = linked.iter().map(|(a, _)| *a).collect();
        let born: HashSet<i64> = client
            .query(
                &format!(
                    "SELECT DISTINCT l.lo FROM {s}.code_line_instances l \
                     WHERE l.line = ANY($1) AND l.lo = ANY($2)"
                ),
                &[&added, &addrs],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let mut seen = BTreeMap::new();
        for (a, c) in linked {
            if born.contains(a) {
                seen.entry((c.date.clone(), c.sha.clone()))
                    .or_insert_with(|| c.clone());
            }
        }
        t.landed = seen.into_values().collect();
    }
    Ok(())
}

/// A note when patches wait to be indexed.
async fn unindexed_note(
    client: &impl GenericClient,
    result: &mut SearchPatchesResult,
) -> Result<()> {
    let waiting: i64 = client
        .query_one("SELECT count(*) FROM patches WHERE text_ids IS NULL", &[])
        .await?
        .get(0);
    if waiting > 0 {
        result.incomplete = true;
        result.notes.push(format!(
            "{waiting} patches are not indexed yet; their lines aren't searched"
        ));
    }
    Ok(())
}
