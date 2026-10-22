// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! get_commit: a commit as git show gives it, from the history: its message
//! and trailers, the discussion threads its Discussion trailers name, its
//! back-patch siblings on other branches, the first release on each branch
//! that has the change, the files it changed, and a unified diff rendered
//! from the weave.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

use crate::diff::{Side, WeaveLine, hunks};
use crate::glob::RgGlob;
use crate::history::{alive_at, checked};
use pghacker_core::text::discussion_message_id;

/// The most diff lines a call returns.
pub const MAX_DIFF_LINES: usize = 3_000;

/// The get_commit tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCommitArgs {
    /// The commit: a SHA or a prefix of 7 or more hexadecimal digits, a branch or tag (its newest commit), or a revision such as REL_17_STABLE~2 or master@2024-06-01.
    pub commit: String,
    /// Only these files' diffs: paths or folders from the source root, or globs such as *.y.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Lines of context around each change. Default 3, as git's, at most 1000.
    #[serde(default)]
    pub context: Option<u32>,
    /// Include the unified diff. Default true; false gives the message, links and changed files only.
    #[serde(default)]
    pub diff: Option<bool>,
}

/// A commit, briefly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitRef {
    pub sha: String,
    pub date: String,
    pub branch: String,
    pub subject: String,
}

/// The first release on a branch that has a commit's change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FirstRelease {
    pub branch: String,
    pub tag: String,
    pub date: String,
}

/// A pgsql-hackers thread linked to a commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ThreadLink {
    /// "trailer": a Discussion trailer names it; "anchor": it cites the
    /// commit.
    pub via: String,
    /// The message named, or the first citing the commit.
    pub message_id: String,
    /// The thread, when the mail archive has the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// A posted patch, and how much of a commit's added lines it has.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommittedPatch {
    pub message_id: String,
    pub thread_id: i64,
    pub date: String,
    pub author: String,
    pub filename: String,
    /// The posted version it belongs to, as written: `49`, `3.1`, `20261003`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The share of the commit's distinct added lines the patch adds.
    pub share: f64,
}

/// The most added lines compared with patches.
const MAX_COMPARED_LINES: i64 = 5_000;

/// The linked threads' patch whose added lines best cover the commit's, if
/// any covers at least a tenth. Mail tables or the patch index may be
/// missing: then none.
async fn committed_patch(
    client: &impl GenericClient,
    s: &str,
    addr: i64,
    threads: &[i64],
) -> Result<Option<CommittedPatch>> {
    if threads.is_empty() {
        return Ok(None);
    }
    // The commit's added lines are the instances born at it, all in the
    // lineages of the versions it made and alive past it: the scope index
    // (lineage, hi, lo) finds them. The count and the matches come from the
    // same lines.
    let Ok(row) = client
        .query_one(
            &format!(
                "WITH added AS MATERIALIZED ( \
                     SELECT DISTINCT l.line FROM {s}.code_line_instances l \
                     WHERE l.lineage = ANY (ARRAY( \
                               SELECT v.lineage FROM {s}.code_commit_files f \
                               JOIN {s}.code_file_versions v ON v.id = f.version \
                               WHERE f.addr = $1)) \
                       AND l.hi > $1 AND l.lo = $1 AND btrim(l.line) <> '' \
                     LIMIT {MAX_COMPARED_LINES}) \
                 SELECT (SELECT count(*) FROM added), \
                        ARRAY(SELECT DISTINCT t.id FROM added a \
                              JOIN patch_texts t ON t.kind = '+' AND md5(t.line) = md5(a.line) \
                                   AND t.line = a.line)"
            ),
            &[&addr],
        )
        .await
    else {
        return Ok(None);
    };
    let added: i64 = row.get(0);
    let ids: Vec<i32> = row.get(1);
    if ids.is_empty() || added == 0 {
        return Ok(None);
    }
    let best = client
        .query_opt(
            "SELECT m.message_id, m.thread_id, to_char(m.sent_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), \
                    m.from_name, p.filename, p.series_version, \
                    (SELECT count(DISTINCT u) FROM unnest(p.text_ids) u WHERE u = ANY($2)) AS n \
             FROM patches p JOIN messages m ON m.id = p.message_id \
             WHERE m.thread_id = ANY($1) AND p.text_ids && $2 \
             ORDER BY n DESC, m.sent_at DESC NULLS LAST, p.id DESC LIMIT 1",
            &[&threads, &ids],
        )
        .await?;
    Ok(best.and_then(|r| {
        let share = r.get::<_, i64>(6) as f64 / added as f64;
        (share >= 0.1).then(|| CommittedPatch {
            message_id: r.get(0),
            thread_id: r.get(1),
            date: r.get::<_, Option<String>>(2).unwrap_or_default(),
            author: r.get(3),
            filename: r.get(4),
            version: r.get(5),
            share: (share * 1000.0).round() / 1000.0,
        })
    }))
}

/// A file the commit changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangedFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// A(dded), D(eleted), M(odified) or R(enamed).
    pub status: String,
    pub added: i32,
    pub removed: i32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
}

/// get_commit's answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommitResult {
    pub repo: String,
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub branch: String,
    pub author: String,
    pub authored_at: String,
    pub committer: String,
    pub committed_at: String,
    pub subject: String,
    pub message: String,
    pub trailers: BTreeMap<String, Vec<String>>,
    pub threads: Vec<ThreadLink>,
    /// The patch in a linked thread whose added lines best cover the
    /// commit's: the version that was committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_patch: Option<CommittedPatch>,
    /// The same change on other branches.
    pub backpatches: Vec<CommitRef>,
    pub releases: Vec<FirstRelease>,
    pub files: Vec<ChangedFile>,
    pub added: i64,
    pub removed: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub diff: String,
    /// The diff stopped at the line limit: ask for fewer paths.
    pub cut: bool,
}

/// Whether a path filter covers `path`: a glob, or a path or folder.
pub(crate) struct PathFilter(Vec<Result<RgGlob, String>>);

impl PathFilter {
    pub(crate) fn new(paths: &[String]) -> Result<Self> {
        let mut out = Vec::new();
        for raw in paths.iter().map(|p| p.trim().trim_start_matches('/')) {
            if raw.is_empty() {
                continue;
            }
            if raw.contains(['*', '?', '[', '{']) {
                out.push(Ok(
                    RgGlob::new(raw).map_err(|e| anyhow!("bad glob {raw}: {e}"))?
                ));
            } else {
                out.push(Err(raw.trim_end_matches('/').to_owned()));
            }
        }
        Ok(Self(out))
    }

    pub(crate) fn covers(&self, path: &str) -> bool {
        self.0.is_empty()
            || self.0.iter().any(|p| match p {
                Ok(glob) => glob.is_match(path, false),
                Err(prefix) => {
                    path == prefix
                        || path
                            .strip_prefix(prefix.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                }
            })
    }
}

const DATE: &str = "to_char(c.committed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')";

/// A commit's first tag on its own branch, or for a master commit the first
/// tag anywhere that has it.
async fn first_release(
    client: &impl GenericClient,
    s: &str,
    addr: i64,
) -> Result<Option<FirstRelease>> {
    let (lo, hi) = crate::range::descendants(addr);
    let order = if crate::range::on_master(addr) {
        "c.committed_at, c.addr"
    } else {
        "c.addr"
    };
    let row = client
        .query_opt(
            &format!(
                "SELECT l.name, r.name, {DATE} FROM {s}.code_refs r \
                 JOIN {s}.code_commits c ON c.addr = r.addr \
                 JOIN {s}.code_lanes l ON l.id = c.lane \
                 WHERE r.kind = 'tag' AND r.addr BETWEEN $1 AND $2 \
                 ORDER BY {order} LIMIT 1"
            ),
            &[&lo, &hi],
        )
        .await?;
    Ok(row.map(|r| FirstRelease {
        branch: r.get(0),
        tag: r.get(1),
        date: r.get(2),
    }))
}

/// A file version's place and form.
struct VersionInfo {
    lineage: i32,
    lo: i64,
    side: Side,
    blob: String,
}

/// Gets a commit from the history in `schema`.
pub async fn get_commit(
    client: &impl GenericClient,
    schema: &str,
    repo: &str,
    args: &GetCommitArgs,
) -> Result<CommitResult> {
    let s = checked(schema)?;
    let addr = crate::rev::resolve(client, s, repo, Some(&args.commit))
        .await?
        .history
        .addr;
    let row = client
        .query_one(
            &format!(
                "SELECT c.sha, l.name, c.author_name, \
                        to_char(c.authored_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                        c.committer_name, \
                        to_char(c.committed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                        c.subject, c.message, c.trailers, c.backpatch \
                 FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane \
                 WHERE c.addr = $1"
            ),
            &[&addr],
        )
        .await?;
    let trailers: BTreeMap<String, Vec<String>> =
        serde_json::from_value(row.get::<_, serde_json::Value>(8)).unwrap_or_default();
    let backpatch: Option<i64> = row.get(9);
    let parent = match crate::rev::parent(addr) {
        Some(p) => client
            .query_opt(
                &format!("SELECT sha FROM {s}.code_commits WHERE addr = $1"),
                &[&p],
            )
            .await?
            .map(|r| r.get(0)),
        None => None,
    };

    let mut backpatches = Vec::new();
    let mut lane_commits = vec![addr];
    if let Some(group) = backpatch {
        for r in client
            .query(
                &format!(
                    "SELECT c.addr, c.sha, {DATE}, l.name, c.subject \
                     FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane \
                     WHERE c.backpatch = $1 AND c.addr <> $2 ORDER BY c.committed_at, c.addr"
                ),
                &[&group, &addr],
            )
            .await?
        {
            lane_commits.push(r.get(0));
            backpatches.push(CommitRef {
                sha: r.get(1),
                date: r.get(2),
                branch: r.get(3),
                subject: r.get(4),
            });
        }
    }
    let mut releases = Vec::new();
    for a in lane_commits {
        if let Some(release) = first_release(client, s, a).await?
            && !releases.contains(&release)
        {
            releases.push(release);
        }
    }

    // Linked threads; the mail tables may be absent where only code is
    // loaded.
    let sha: String = row.get(0);
    let mut threads: Vec<ThreadLink> = Vec::new();
    if let Ok(rows) = client
        .query(
            &format!(
                "SELECT DISTINCT ON (t.thread_id, t.via) t.via, m.message_id, t.thread_id, \
                        th.subject \
                 FROM {s}.code_commit_threads t \
                 JOIN messages m ON m.id = t.message_id JOIN threads th ON th.id = t.thread_id \
                 WHERE t.sha = $1 ORDER BY t.thread_id, t.via, m.sent_at"
            ),
            &[&sha],
        )
        .await
    {
        for r in rows {
            threads.push(ThreadLink {
                via: r.get(0),
                message_id: r.get(1),
                thread_id: Some(r.get(2)),
                subject: Some(r.get(3)),
            });
        }
    }
    threads.sort_by_key(|t| (t.via != "trailer", t.thread_id));
    // Discussion messages the archive lacks.
    for id in trailers
        .get("Discussion")
        .into_iter()
        .flatten()
        .filter_map(|v| discussion_message_id(v))
    {
        if !threads.iter().any(|t| t.message_id == id) {
            threads.push(ThreadLink {
                via: "trailer".into(),
                message_id: id,
                thread_id: None,
                subject: None,
            });
        }
    }

    let file_rows = client
        .query(
            &format!(
                "SELECT path, old_path, status, added, removed, is_binary, version, old_version \
                 FROM {s}.code_commit_files WHERE addr = $1 ORDER BY path COLLATE \"C\""
            ),
            &[&addr],
        )
        .await?;
    let files: Vec<ChangedFile> = file_rows
        .iter()
        .map(|r| ChangedFile {
            path: r.get(0),
            old_path: r.get(1),
            status: r.get(2),
            added: r.get(3),
            removed: r.get(4),
            binary: r.get(5),
        })
        .collect();
    let mut result = CommitResult {
        repo: repo.to_owned(),
        sha,
        parent,
        branch: row.get(1),
        author: row.get(2),
        authored_at: row.get(3),
        committer: row.get(4),
        committed_at: row.get(5),
        subject: row.get(6),
        message: row.get(7),
        trailers,
        committed_patch: None,
        threads,
        backpatches,
        releases,
        added: files.iter().map(|f| i64::from(f.added)).sum(),
        removed: files.iter().map(|f| i64::from(f.removed)).sum(),
        files,
        diff: String::new(),
        cut: false,
    };
    let linked: Vec<i64> = result.threads.iter().filter_map(|t| t.thread_id).collect();
    result.committed_patch = committed_patch(client, s, addr, &linked).await?;
    if args.diff == Some(false) {
        return Ok(result);
    }

    let filter = PathFilter::new(&args.paths)?;
    let context = args.context.unwrap_or(3).min(1_000) as usize;
    let wanted: Vec<usize> = (0..file_rows.len())
        .filter(|&k| {
            let f = &result.files[k];
            filter.covers(&f.path) || f.old_path.as_deref().is_some_and(|p| filter.covers(p))
        })
        .collect();
    let ids: Vec<i32> = wanted
        .iter()
        .flat_map(|&k| [file_rows[k].get::<_, Option<i32>>(6), file_rows[k].get(7)])
        .flatten()
        .collect();
    let versions: HashMap<i32, VersionInfo> = client
        .query(
            &format!(
                "SELECT id, lineage, lo, final_newline, bom, blob_sha \
                 FROM {s}.code_file_versions WHERE id = ANY($1)"
            ),
            &[&ids],
        )
        .await?
        .iter()
        .map(|r| {
            (
                r.get(0),
                VersionInfo {
                    lineage: r.get(1),
                    lo: r.get(2),
                    side: Side {
                        final_newline: r.get(3),
                        bom: r.get(4),
                    },
                    blob: r.get(5),
                },
            )
        })
        .collect();

    let mut out: Vec<String> = Vec::new();
    for k in wanted {
        let f = &result.files[k];
        let new = file_rows[k]
            .get::<_, Option<i32>>(6)
            .and_then(|v| versions.get(&v));
        let old = file_rows[k]
            .get::<_, Option<i32>>(7)
            .and_then(|v| versions.get(&v));
        let a = f.old_path.as_deref().unwrap_or(&f.path);
        let mut block = vec![format!("diff --git a/{a} b/{}", f.path)];
        match f.status.as_str() {
            "A" => block.push("new file".into()),
            "D" => block.push("deleted file".into()),
            "R" => {
                block.push(format!("rename from {a}"));
                block.push(format!("rename to {}", f.path));
            }
            _ => {}
        }
        let short = |v: Option<&VersionInfo>| {
            v.map_or_else(|| "0000000".to_owned(), |v| v.blob[..7].to_owned())
        };
        let same_blob = matches!((old, new), (Some(o), Some(n)) if o.blob == n.blob);
        if !same_blob {
            block.push(format!("index {}..{}", short(old), short(new)));
        }
        let from = if old.is_some() {
            format!("a/{a}")
        } else {
            "/dev/null".into()
        };
        let to = if new.is_some() {
            format!("b/{}", f.path)
        } else {
            "/dev/null".into()
        };
        if f.binary {
            block.push(format!("Binary files {from} and {to} differ"));
        } else if !same_blob {
            let lineage = new
                .or(old)
                .map(|v| v.lineage)
                .context("a change without versions")?;
            let old_at = old.map(|v| v.lo);
            let new_at = new.map(|v| v.lo);
            let rows = client
                .query(
                    &format!(
                        "SELECT coalesce({} , false), coalesce({}, false), l.line \
                         FROM {s}.code_line_instances l \
                         WHERE l.lineage = $1 AND (coalesce({0}, false) OR coalesce({1}, false)) \
                         ORDER BY l.weave, l.id",
                        alive_at("l", "$2::int8"),
                        alive_at("l", "$3::int8"),
                    ),
                    &[&lineage, &old_at, &new_at],
                )
                .await?;
            let lines: Vec<WeaveLine<'_>> = rows
                .iter()
                .map(|r| WeaveLine {
                    old: r.get(0),
                    new: r.get(1),
                    text: Cow::Borrowed(r.get::<_, &str>(2)),
                })
                .collect();
            let body = hunks(
                &lines,
                old.map(|v| v.side).unwrap_or_default(),
                new.map(|v| v.side).unwrap_or_default(),
                context,
            );
            if !body.is_empty() {
                block.push(format!("--- {from}"));
                block.push(format!("+++ {to}"));
                block.extend(body);
            }
        }
        if out.len() + block.len() > MAX_DIFF_LINES && !out.is_empty() {
            result.cut = true;
            break;
        }
        out.extend(block);
        if out.len() > MAX_DIFF_LINES {
            out.truncate(MAX_DIFF_LINES);
            result.cut = true;
            break;
        }
    }
    if !out.is_empty() {
        result.diff = out.join("\n");
        result.diff.push('\n');
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_filters() {
        let f = PathFilter::new(&["src/backend/".into(), "*.y".into()]).unwrap();
        assert!(f.covers("src/backend/parser/scan.l"));
        assert!(f.covers("src/interfaces/ecpg/preproc/preproc.y"));
        assert!(!f.covers("src/backendx/a.c"));
        assert!(PathFilter::new(&[]).unwrap().covers("anything"));
    }
}
