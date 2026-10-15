//! read_file and list_files: a commit's files with line numbers, and its
//! folders.

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

use crate::snapshot::{FileTable, Snapshot, shown};

/// The most lines and target byte cap for read_file's formatted content;
/// the first line is always returned whole, even if it exceeds the byte cap.
pub const MAX_READ_LINES: u32 = 2_000;
pub const MAX_READ_BYTES: usize = 200_000;
/// Default and largest list_files pages.
pub const DEFAULT_LIST_LIMIT: u32 = 500;
pub const MAX_LIST_LIMIT: u32 = 5_000;

/// The read_file tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    /// A file path relative to the source root, such as src/backend/parser/gram.y.
    pub path: String,
    /// The first line to read, from 1. Default 1.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// The last line to read. Default: as many as fit (2000 lines, 200000 formatted bytes); the first line is always returned whole.
    #[serde(default)]
    pub end_line: Option<u32>,
    /// PostgreSQL version: a major version such as 19 (the head of its branch), a release such as 18.3, a ref such as REL_18_3, or a commit SHA. Default: the newest release line.
    #[serde(default)]
    pub version: Option<String>,
}

/// A file's lines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReadFileResult {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    pub path: String,
    pub start_line: u32,
    /// The last line returned.
    pub end_line: u32,
    pub total_lines: u32,
    /// The lines, each as its number, a tab and its text (like cat -n).
    pub content: String,
    /// More lines were asked for than fit: read on from end_line + 1.
    pub cut: bool,
}

/// The list_files tool's arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListFilesArgs {
    /// A folder (or file) relative to the source root. Default: the root.
    #[serde(default)]
    pub path: Option<String>,
    /// Only files matching this glob below path, such as *.c (any depth) or nodes/*.h.
    #[serde(default)]
    pub glob: Option<String>,
    /// Folder levels to list below path. Default 1, or every level with a glob.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Entries to return: default 500, at most 5000.
    #[serde(default)]
    pub limit: Option<u32>,
    /// PostgreSQL version: a major version such as 19 (the head of its branch), a release such as 18.3, a ref such as REL_18_3, or a commit SHA. Default: the newest release line.
    #[serde(default)]
    pub version: Option<String>,
}

/// A folder or file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    /// `dir` or `file`.
    pub kind: &'static str,
    /// Lines (files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// Files below (folders).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u32>,
    /// Not text to the tools: read_file refuses it and commit-tree searches skip it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// A commit's date and subject, in a listing of /commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// A listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListFilesResult {
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    /// The folders the history keeps; the rest of the tree is not indexed.
    pub loaded_paths: Vec<String>,
    pub path: String,
    pub total: u64,
    pub cut: bool,
    pub entries: Vec<Entry>,
}

/// Paths like a missing one, for the error.
fn suggestions(table: &FileTable, wanted: &str) -> String {
    let base = wanted.rsplit('/').next().unwrap_or(wanted);
    let mut near: Vec<String> = table
        .files
        .iter()
        .map(|f| f.path.as_str())
        .filter(|p| !base.is_empty() && p.rsplit('/').next() == Some(base))
        .take(5)
        .map(shown)
        .collect();
    if near.is_empty() && base.len() >= 3 {
        near = table
            .files
            .iter()
            .map(|f| f.path.as_str())
            .filter(|p| p.contains(base))
            .take(5)
            .map(shown)
            .collect();
    }
    if near.is_empty() {
        String::new()
    } else {
        format!("; similar paths: {}", near.join(", "))
    }
}

fn not_found(table: &FileTable, raw: &str, path: Option<&str>) -> anyhow::Error {
    let loaded = if table.loaded_paths.is_empty() {
        String::new()
    } else {
        format!("; loaded: {}", table.loaded_paths.join(", "))
    };
    anyhow!(
        "no such file: {raw} (paths are relative to the source root{loaded}){}",
        suggestions(table, path.unwrap_or(raw))
    )
}

/// Reads lines of a file.
pub async fn read_file(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &ReadFileArgs,
) -> Result<ReadFileResult> {
    let table = FileTable::view(client, snapshot, std::slice::from_ref(&args.path)).await?;
    let path = table.normalize(&snapshot.repo, &args.path);
    let Some(path) = path.filter(|p| table.exists(p)) else {
        let normalized = table.normalize(&snapshot.repo, &args.path);
        return Err(not_found(&table, &args.path, normalized.as_deref()));
    };
    let Some(k) = table.file(&path) else {
        bail!("{} is a folder: use list_files", args.path);
    };
    let file = &table.files[k];
    if file.is_binary {
        bail!("{path} is not a text file: it has no lines");
    }
    let total = file.line_count.max(0) as u32;
    let start = args.start_line.unwrap_or(1).max(1);
    if total == 0 {
        return Ok(ReadFileResult {
            repo: snapshot.repo.clone(),
            version: snapshot.version.clone(),
            git_ref: snapshot.git_ref.clone(),
            sha: snapshot.sha.clone(),
            path: shown(&path),
            start_line: 1,
            end_line: 0,
            total_lines: 0,
            content: String::new(),
            cut: false,
        });
    }
    if start > total {
        bail!("{path} has {total} lines; start_line {start} is past the end");
    }
    let wanted_end = args.end_line.unwrap_or(u32::MAX).min(total);
    if wanted_end < start {
        bail!("end_line {wanted_end} is before start_line {start}");
    }
    let end = wanted_end.min(start + MAX_READ_LINES - 1);
    let mut rows: Vec<(i32, String)> = crate::history::line_ranges(
        client,
        &snapshot.history,
        &[(file.id, start as i32, end as i32)],
    )
    .await
    .context("cannot read the file's lines")?
    .into_iter()
    .map(|((_, n), line)| (n, line))
    .collect();
    rows.sort_unstable_by_key(|(n, _)| *n);
    let mut content = String::new();
    let mut last = start - 1;
    for (n, line) in rows {
        let entry = format!("{n:>6}\t{line}\n");
        if !content.is_empty() && content.len() + entry.len() > MAX_READ_BYTES {
            break;
        }
        content.push_str(&entry);
        last = n as u32;
    }
    Ok(ReadFileResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        path: shown(&path),
        start_line: start,
        end_line: last,
        total_lines: total,
        content,
        cut: last < wanted_end,
    })
}

/// Lists folders and files.
pub async fn list_files(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &ListFilesArgs,
) -> Result<ListFilesResult> {
    let raw = args.path.as_deref().unwrap_or("").trim();
    if crate::snapshot::bare_root(raw) {
        let root = raw.trim_matches('/');
        return virtual_listing(client, snapshot, root, args).await;
    }
    let table = FileTable::view(client, snapshot, &[raw.to_owned()]).await?;
    let base = if raw.is_empty() || raw == "." || raw == "/" {
        String::new()
    } else {
        table
            .normalize(&snapshot.repo, raw)
            .filter(|p| table.exists(p))
            .ok_or_else(|| {
                let normalized = table.normalize(&snapshot.repo, raw);
                not_found(&table, raw, normalized.as_deref())
            })?
    };
    let limit = args
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT) as usize;
    let mut entries: Vec<Entry> = Vec::new();
    if let Some(k) = table.file(&base) {
        let f = &table.files[k];
        entries.push(file_entry(f));
    } else {
        let glob = match args
            .glob
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            Some(g) => Some(crate::glob::RgGlob::new(g).map_err(|e| anyhow!(e))?),
            None => None,
        };
        let depth = args
            .depth
            .map(|d| d.max(1) as usize)
            .unwrap_or(if glob.is_some() { usize::MAX } else { 1 });
        let skip = if base.is_empty() { 0 } else { base.len() + 1 };
        let mut dirs: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for k in table.under(&base) {
            let f = &table.files[k];
            let rel = &f.path[skip..];
            let comps: Vec<&str> = rel.split('/').collect();
            if let Some(g) = &glob {
                if comps.len() <= depth && g.is_match(rel, false) {
                    entries.push(file_entry(f));
                }
                continue;
            }
            // Folders down to the depth, each counted once per file below.
            for d in 1..comps.len().min(depth + 1) {
                let dir = format!("{}{}", &f.path[..skip], comps[..d].join("/"));
                *dirs.entry(dir).or_default() += 1;
            }
            if comps.len() <= depth {
                entries.push(file_entry(f));
            }
        }
        for (path, files) in dirs {
            entries.push(Entry {
                path: shown(&path),
                kind: "dir",
                lines: None,
                files: Some(files),
                binary: false,
                date: None,
                subject: None,
            });
        }
        entries.sort_by(|a, b| crate::snapshot::tree_cmp(&a.path, &b.path));
    }
    let total = entries.len() as u64;
    let cut = entries.len() > limit;
    entries.truncate(limit);
    Ok(ListFilesResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        loaded_paths: table.loaded_paths.clone(),
        path: shown(&base),
        total,
        cut,
        entries,
    })
}

fn file_entry(f: &crate::snapshot::FileEntry) -> Entry {
    Entry {
        path: shown(&f.path),
        kind: "file",
        lines: Some(f.line_count.max(0) as u32),
        files: None,
        binary: f.is_binary,
        date: None,
        subject: None,
    }
}

/// /branch and /tag (filtered by the glob), or /commit, newest first: what
/// can be mounted.
async fn virtual_listing(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    root: &str,
    args: &ListFilesArgs,
) -> Result<ListFilesResult> {
    let s = crate::history::checked(&snapshot.history.schema)?;
    let limit = args
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT) as usize;
    let glob = match args
        .glob
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        Some(g) => Some(crate::glob::RgGlob::new(g).map_err(|e| anyhow!(e))?),
        None => None,
    };
    let (rows, total) = if root == "commit" {
        let total: i64 = client
            .query_one(&format!("SELECT count(*) FROM {s}.code_commits"), &[])
            .await?
            .get(0);
        let rows = client
            .query(
                &format!(
                    "SELECT left(sha, 12), to_char(committed_at, 'YYYY-MM-DD'), subject \
                     FROM {s}.code_commits ORDER BY committed_at DESC, addr DESC LIMIT $1"
                ),
                &[&(limit as i64)],
            )
            .await?;
        (rows, total as u64)
    } else {
        let rows = client
            .query(
                &format!(
                    "SELECT r.name, to_char(c.committed_at, 'YYYY-MM-DD'), c.subject \
                     FROM {s}.code_refs r JOIN {s}.code_commits c ON c.addr = r.addr \
                     WHERE r.kind = $1 ORDER BY r.name COLLATE \"C\""
                ),
                &[&root],
            )
            .await?;
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| glob.as_ref().is_none_or(|g| g.is_match(r.get(0), false)))
            .collect();
        let total = rows.len() as u64;
        (rows, total)
    };
    let entries: Vec<Entry> = rows
        .iter()
        .take(limit)
        .map(|r| Entry {
            path: format!("/{root}/{}", r.get::<_, String>(0)),
            kind: "dir",
            lines: None,
            files: None,
            binary: false,
            date: Some(r.get(1)),
            subject: Some(r.get(2)),
        })
        .collect();
    Ok(ListFilesResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        loaded_paths: Vec::new(),
        path: format!("/{root}"),
        cut: total > entries.len() as u64,
        total,
        entries,
    })
}
