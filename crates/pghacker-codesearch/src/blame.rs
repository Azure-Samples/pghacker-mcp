//! blame: the commit that added each line of a file, as git blame gives it,
//! following renames, and passing through the reformatting commits that
//! PostgreSQL's .git-blame-ignore-revs names (pgindent runs), as
//! `git blame --ignore-revs-file` does for lines replaced one for one.
//! With `reverse`, the commit that removed each line after this version.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

use crate::files::MAX_READ_LINES;
use crate::history::{Spans, alive_at, checked, keys, line_ranges};
use crate::snapshot::{FileTable, MOUNTS, Snapshot, mount_of, shown};

/// The file PostgreSQL lists its reformatting commits in.
pub const IGNORE_REVS_FILE: &str = ".git-blame-ignore-revs";

/// The blame tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameArgs {
    /// A file path relative to the source root, or below /branch/<name>/, /tag/<name>/ or /commit/<rev>/.
    pub path: String,
    /// The first line, from 1. Default 1.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// The last line. Default: 2000 lines on.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// Pass through the commits in .git-blame-ignore-revs (pgindent and other reformatting) to the commit that wrote each line, as git blame --ignore-revs-file does. Default true.
    #[serde(default)]
    pub ignore_revs: Option<bool>,
    /// For each line, the commit that removed it after this version, on this branch (git blame --reverse); none while the branch's head still has it.
    #[serde(default)]
    pub reverse: bool,
    /// For each line, the earliest patch posted to the lists that added it to this file (under any of its names): where the line was first proposed.
    #[serde(default)]
    pub proposals: bool,
    /// PostgreSQL version: a major version such as 19, a release such as 18.3, a branch or tag, a commit SHA, or <ref>@<date>. Default: the newest release line.
    #[serde(default)]
    pub version: Option<String>,
}

/// A line and its commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BlameLine {
    pub line: u32,
    /// The commit's SHA prefix, a key of `commits`; empty with `reverse`
    /// while the line remains.
    pub commit: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<Proposal>,
}

/// The earliest posted patch that added a line to its file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Proposal {
    pub thread_id: i64,
    pub message_id: String,
    pub date: String,
    pub author: String,
}

/// A line shorter than this, trimmed, is too common to trace.
const MIN_TRACED: usize = 8;
/// The most candidate patches read when tracing proposals.
const MAX_PROPOSAL_PATCHES: i64 = 200;

/// The earliest posted patch that added each line's text to the file, which
/// was at `paths` over its history. Mail tables or the patch index may be
/// missing: then none.
async fn proposals(
    client: &impl GenericClient,
    texts: &[&str],
    paths: &[String],
) -> Result<HashMap<String, Proposal>> {
    let mut out = HashMap::new();
    let traced: Vec<&str> = texts
        .iter()
        .copied()
        .filter(|t| t.trim().len() >= MIN_TRACED && t.chars().any(char::is_alphanumeric))
        .collect();
    if traced.is_empty() {
        return Ok(out);
    }
    let Ok(rows) = client
        .query(
            "SELECT t.id, t.line FROM unnest($1::text[]) w(line) \
             JOIN patch_texts t ON t.kind = '+' AND md5(t.line) = md5(w.line) AND t.line = w.line",
            &[&traced],
        )
        .await
    else {
        return Ok(out);
    };
    let text_of: HashMap<i32, String> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    let ids: Vec<i32> = text_of.keys().copied().collect();
    if ids.is_empty() {
        return Ok(out);
    }
    let candidates = client
        .query(
            &format!(
                "SELECT p.id, m.thread_id, m.message_id, \
                        to_char(m.sent_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), m.from_name, p.content, \
                        ARRAY(SELECT u.n FROM unnest(p.text_ids) WITH ORDINALITY u(id, n) \
                              WHERE u.id = ANY($1) ORDER BY u.n), \
                        ARRAY(SELECT u.id FROM unnest(p.text_ids) WITH ORDINALITY u(id, n) \
                              WHERE u.id = ANY($1) ORDER BY u.n) \
                 FROM patches p JOIN messages m ON m.id = p.message_id \
                 WHERE p.text_ids && $1 AND p.paths && $2 \
                 ORDER BY m.sent_at NULLS LAST, p.id LIMIT {MAX_PROPOSAL_PATCHES}"
            ),
            &[&ids, &paths],
        )
        .await?;
    for r in candidates {
        if out.len() == text_of.len() {
            break;
        }
        let at: Vec<i64> = r.get(6);
        let found: Vec<i32> = r.get(7);
        if found.iter().all(|id| out.contains_key(&text_of[id])) {
            continue;
        }
        // Only lines the patch adds to this file count.
        let (stats, lines) = pghacker_mail::patch::parse_diff(r.get(5));
        for (n, id) in at.into_iter().zip(found) {
            let text = &text_of[&id];
            if out.contains_key(text) {
                continue;
            }
            let in_file = lines
                .get((n - 1) as usize)
                .and_then(|l| l.file)
                .and_then(|k| stats.paths.get(k))
                .is_some_and(|p| paths.contains(p));
            if in_file {
                out.insert(
                    text.clone(),
                    Proposal {
                        thread_id: r.get(1),
                        message_id: r.get(2),
                        date: r.get::<_, Option<String>>(3).unwrap_or_default(),
                        author: r.get(4),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// A commit, once per blame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BlameCommit {
    pub sha: String,
    pub date: String,
    pub author: String,
    pub branch: String,
    pub subject: String,
}

/// A blame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BlameResult {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub reverse: bool,
    /// Commits passed through: how many of .git-blame-ignore-revs applied.
    pub ignored_revs: u32,
    pub lines: Vec<BlameLine>,
    pub commits: BTreeMap<String, BlameCommit>,
    /// More lines remain after end_line.
    pub cut: bool,
}

/// A line instance, as blame reads it.
#[derive(Clone)]
struct Inst {
    lo: i64,
    spans: Spans,
    replaces: Option<i32>,
    line: String,
}

async fn instances(
    client: &impl GenericClient,
    s: &str,
    ids: &[i32],
) -> Result<HashMap<i32, Inst>> {
    let rows = client
        .query(
            &format!(
                "SELECT id, lo, hi, alive::text, replaces, line FROM {s}.code_line_instances \
                 WHERE id = ANY($1)"
            ),
            &[&ids],
        )
        .await?;
    let mut out = HashMap::new();
    for r in rows {
        let (lo, hi): (i64, i64) = (r.get(1), r.get(2));
        let spans = match r.get::<_, Option<String>>(3) {
            Some(text) => Spans::parse(&text)?,
            None => Spans(vec![(lo, hi)]),
        };
        out.insert(
            r.get(0),
            Inst {
                lo,
                spans,
                replaces: r.get(4),
                line: r.get(5),
            },
        );
    }
    Ok(out)
}

/// The reformatting commits that the .git-blame-ignore-revs at `list`
/// names, as addresses.
async fn ignored(
    client: &impl GenericClient,
    s: &str,
    h: &crate::history::History,
    table: &FileTable,
    list: &str,
) -> Result<HashSet<i64>> {
    let Some(k) = table.file(list) else {
        return Ok(HashSet::new());
    };
    let f = &table.files[k];
    let lines = line_ranges(client, h, &[(f.id, 1, f.line_count)]).await?;
    let shas: Vec<String> = lines
        .values()
        .map(|l| l.trim())
        .filter(|l| l.len() == 40 && l.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .collect();
    Ok(client
        .query(
            &format!("SELECT addr FROM {s}.code_commits WHERE sha = ANY($1)"),
            &[&shas],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Blames lines of a file in the history.
pub async fn blame(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &BlameArgs,
) -> Result<BlameResult> {
    let Some(h) = &snapshot.history else {
        bail!("blame reads the commit history, which this deployment doesn't serve yet");
    };
    let s = checked(&h.schema)?;
    let mount = mount_of(&args.path);
    let table = FileTable::view(client, snapshot, std::slice::from_ref(&args.path)).await?;
    let path = table
        .normalize(&snapshot.repo, &args.path)
        .filter(|p| table.exists(p))
        .with_context(|| format!("no such file: {}", args.path))?;
    let Some(k) = table.file(&path) else {
        bail!("{} is a folder", args.path);
    };
    let file = &table.files[k];
    if file.is_binary {
        bail!("{} is not a text file: it has no lines", args.path);
    }
    let total = file.line_count.max(0) as u32;
    let start = args.start_line.unwrap_or(1).max(1);
    let wanted_end = args.end_line.unwrap_or(u32::MAX).min(total);
    let end = wanted_end.min(start.saturating_add(MAX_READ_LINES - 1));
    let mut result = BlameResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        path: shown(&path),
        start_line: start,
        end_line: end.max(start.saturating_sub(1)),
        total_lines: total,
        reverse: args.reverse,
        ignored_revs: 0,
        lines: Vec::new(),
        commits: BTreeMap::new(),
        cut: end < wanted_end,
    };
    if total == 0 || start > end {
        if start > total && total > 0 {
            bail!(
                "{} has {total} lines; start_line {start} is past the end",
                args.path
            );
        }
        return Ok(result);
    }

    // The lines' instances, in order.
    let weave = keys(client, h, &[file.id]).await?;
    let weave = weave.get(&file.id).context("the file has no lines")?;
    let wanted: Vec<i64> = weave[(start - 1) as usize..end as usize].to_vec();
    let rows = client
        .query(
            &format!(
                "SELECT l.weave, l.id FROM {s}.code_file_versions v \
                 JOIN {s}.code_line_instances l ON l.lineage = v.lineage \
                     AND l.weave = ANY($2) AND {} \
                 WHERE v.id = $1",
                alive_at("l", "v.lo")
            ),
            &[&file.id, &wanted],
        )
        .await?;
    let by_weave: HashMap<i64, i32> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    let ids: Vec<i32> = wanted
        .iter()
        .map(|w| by_weave.get(w).copied().context("a line has no instance"))
        .collect::<Result<_>>()?;
    let mut known = instances(client, s, &ids).await?;

    let mut blamed: Vec<Option<i64>> = Vec::with_capacity(ids.len());
    if args.reverse {
        // The commit this version is read at: the call's, or its mount's.
        let at = match &mount {
            Some((root, name)) => crate::rev::resolve_mount(client, s, &snapshot.repo, root, name)
                .await?
                .history
                .map_or(h.addr, |m| m.addr),
            None => h.addr,
        };
        for id in &ids {
            blamed.push(crate::range::death_after(&known[id].spans.0, at));
        }
    } else {
        let ignore = if args.ignore_revs.unwrap_or(true) {
            // A mounted commit's own list: back branches name their own runs.
            let list = match &mount {
                Some((root, name)) => format!("{MOUNTS}/{root}/{name}/{IGNORE_REVS_FILE}"),
                None => IGNORE_REVS_FILE.to_owned(),
            };
            ignored(client, s, h, &table, &list).await?
        } else {
            HashSet::new()
        };
        let mut current: Vec<i32> = ids.clone();
        let mut passed: HashSet<i64> = HashSet::new();
        // Follow one-for-one replacements back through ignored commits.
        for _ in 0..64 {
            let step: Vec<(usize, i32)> = current
                .iter()
                .enumerate()
                .filter_map(|(i, id)| {
                    let inst = &known[id];
                    ignore
                        .contains(&inst.lo)
                        .then_some(inst.replaces)
                        .flatten()
                        .map(|r| (i, r))
                })
                .collect();
            if step.is_empty() {
                break;
            }
            let mut missing: Vec<i32> = step
                .iter()
                .map(|&(_, r)| r)
                .filter(|r| !known.contains_key(r))
                .collect();
            missing.sort_unstable();
            missing.dedup();
            if !missing.is_empty() {
                known.extend(instances(client, s, &missing).await?);
            }
            for (i, r) in step {
                if known.contains_key(&r) {
                    passed.insert(known[&current[i]].lo);
                    current[i] = r;
                }
            }
        }
        result.ignored_revs = passed.len() as u32;
        blamed.extend(current.iter().map(|id| Some(known[id].lo)));
    }

    let addrs: Vec<i64> = blamed.iter().flatten().copied().collect();
    let commits: HashMap<i64, BlameCommit> = client
        .query(
            &format!(
                "SELECT c.addr, c.sha, to_char(c.committed_at, 'YYYY-MM-DD'), c.author_name, \
                        l.name, c.subject \
                 FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane \
                 WHERE c.addr = ANY($1)"
            ),
            &[&addrs],
        )
        .await?
        .iter()
        .map(|r| {
            (
                r.get(0),
                BlameCommit {
                    sha: r.get(1),
                    date: r.get(2),
                    author: r.get(3),
                    branch: r.get(4),
                    subject: r.get(5),
                },
            )
        })
        .collect();
    for (n, (id, addr)) in ids.iter().zip(&blamed).enumerate() {
        let commit = match addr.and_then(|a| commits.get(&a)) {
            Some(c) => {
                let key = c.sha[..12].to_owned();
                result
                    .commits
                    .entry(key.clone())
                    .or_insert_with(|| c.clone());
                key
            }
            None => String::new(),
        };
        result.lines.push(BlameLine {
            line: start + n as u32,
            commit,
            text: known[id].line.clone(),
            proposed: None,
        });
    }
    if args.proposals && !result.lines.is_empty() {
        let paths: Vec<String> = client
            .query(
                &format!(
                    "SELECT DISTINCT v.path FROM {s}.code_file_versions v \
                     WHERE v.lineage = (SELECT lineage FROM {s}.code_file_versions WHERE id = $1)"
                ),
                &[&file.id],
            )
            .await?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let texts: Vec<&str> = result.lines.iter().map(|l| l.text.as_str()).collect();
        let found = proposals(client, &texts, &paths).await?;
        for line in &mut result.lines {
            line.proposed = found.get(&line.text).cloned();
        }
    }
    Ok(result)
}
