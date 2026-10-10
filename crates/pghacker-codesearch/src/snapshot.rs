//! Snapshots and their file tables: which files a snapshot has, in tree
//! order, and how tool paths map onto them.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use pghacker_core::code::{LATEST_FIRST, newest_first, release_line};
use serde::Serialize;
use tokio_postgres::GenericClient;

/// A loaded source snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub id: i32,
    pub repo: String,
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
}

/// The snapshot of `repo` that a code tool reads. `version` names a major
/// version (`19`, `PG19`, `9`), a release (`18.3`, `9.6.24`) or a release
/// line before 10 (`9.6`), a ref (`REL_18_3`, `REL_19_STABLE`, `master`) or a
/// commit SHA prefix. Of the snapshots it names, the latest wins: the newest
/// commit, so a release branch's head before its releases. Without one, the
/// latest of the newest numbered release line. Every loaded snapshot stays
/// readable by its ref or SHA.
pub async fn resolve_snapshot(
    client: &impl GenericClient,
    repo: &str,
    version: Option<&str>,
) -> Result<Snapshot> {
    let rows = client
        .query(
            &format!(
                "SELECT id, repo, version, ref, sha FROM code_snapshots \
                 WHERE repo = $1 AND state = 'ready' ORDER BY {LATEST_FIRST}"
            ),
            &[&repo],
        )
        .await
        .context("cannot read code_snapshots")?;
    let snapshots: Vec<Snapshot> = rows
        .iter()
        .map(|row| Snapshot {
            id: row.get(0),
            repo: row.get(1),
            version: row.get(2),
            git_ref: row.get(3),
            sha: row.get(4),
        })
        .collect();
    let wanted = version.map(str::trim).filter(|v| !v.is_empty());
    let found = match wanted {
        None => latest_release(&snapshots),
        Some(v) => named(&snapshots, v),
    };
    if let Some(snapshot) = found {
        return Ok(snapshot.clone());
    }
    let mut versions: Vec<&str> = snapshots
        .iter()
        .filter_map(|s| s.version.as_deref())
        .collect();
    versions.sort_by(|a, b| newest_first(a, b));
    versions.dedup();
    match wanted {
        Some(v) if !versions.is_empty() => bail!(
            "no loaded snapshot of {repo} for version {v}; versions: {} (or a release such as 18.3, \
             a ref such as REL_18_STABLE, or a commit SHA)",
            versions.join(", ")
        ),
        _ => bail!("no loaded snapshot of {repo}"),
    }
}

/// The latest snapshot of the newest numbered release line, or the latest of
/// all when none is numbered. `snapshots` are latest first.
fn latest_release(snapshots: &[Snapshot]) -> Option<&Snapshot> {
    let line = |s: &Snapshot| s.version.as_deref().and_then(release_line);
    match snapshots.iter().filter_map(line).max() {
        Some(newest) => snapshots.iter().find(|s| line(s) == Some(newest)),
        None => snapshots.first(),
    }
}

/// The latest snapshot that `v` names: a ref, a release or release line, a
/// commit SHA prefix, then a major version. `snapshots` are latest first.
fn named<'a>(snapshots: &'a [Snapshot], v: &str) -> Option<&'a Snapshot> {
    let with_ref = |name: &str| {
        snapshots
            .iter()
            .find(|s| s.git_ref.eq_ignore_ascii_case(name))
    };
    if let Some(found) = with_ref(v) {
        return Some(found);
    }
    let norm = normalize_version(v);
    let parts: Vec<&str> = norm.split('.').collect();
    if parts.len() > 1
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        let major: u32 = parts[0].parse().ok()?;
        return match (major >= 10, parts.len()) {
            // 18.3 is the release REL_18_3.
            (true, 2) => with_ref(&format!("REL_{}_{}", parts[0], parts[1])),
            // 9.6.24 is the release REL9_6_24.
            (false, 3) => with_ref(&format!("REL{}_{}_{}", parts[0], parts[1], parts[2])),
            // 9.6 is a release line: its branch head, or its newest release.
            (false, 2) => {
                let line = format!("REL{}_{}", parts[0], parts[1]);
                snapshots.iter().find(|s| {
                    let r = s.git_ref.to_ascii_uppercase();
                    r == line || r.starts_with(&format!("{line}_"))
                })
            }
            _ => None,
        };
    }
    let lower = v.to_ascii_lowercase();
    if v.len() >= 7
        && let Some(found) = snapshots.iter().find(|s| s.sha.starts_with(&lower))
    {
        return Some(found);
    }
    snapshots
        .iter()
        .find(|s| s.version.as_deref().map(normalize_version).as_deref() == Some(norm.as_str()))
}

/// `PG19`, `pg 19`, `v19` and `REL_19_STABLE` all mean `19`.
fn normalize_version(v: &str) -> String {
    let lower = v.trim().to_ascii_lowercase();
    let lower = lower.strip_prefix("rel_").unwrap_or(&lower);
    let lower = lower.strip_suffix("_stable").unwrap_or(lower);
    let lower = lower
        .trim_start_matches("postgresql")
        .trim_start_matches("postgres")
        .trim_start_matches("pg")
        .trim_start_matches('v')
        .trim();
    lower.replace('_', ".")
}

/// A file of a snapshot.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub id: i32,
    pub path: String,
    pub is_binary: bool,
    pub line_count: i32,
    pub size: i32,
}

/// A snapshot's files in tree order (paths compared component by
/// component, the order of a sorted directory walk), with its folders.
#[derive(Debug)]
pub struct FileTable {
    pub files: Vec<FileEntry>,
    by_path: HashMap<String, usize>,
    dirs: HashSet<String>,
    /// First path components: what an absolute path must contain.
    top: HashSet<String>,
    /// The path prefixes the snapshot loaded; empty means the whole tree.
    pub loaded_paths: Vec<String>,
}

/// Compares paths component by component.
pub fn tree_cmp(a: &str, b: &str) -> Ordering {
    a.split('/').cmp(b.split('/'))
}

/// (database, snapshot id, loaded_at): a snapshot's files never change
/// once it is ready.
type CacheKey = (String, i32, String);
type Cache = Mutex<Vec<(CacheKey, Arc<FileTable>)>>;

static CACHE: LazyLock<Cache> = LazyLock::new(|| Mutex::new(Vec::new()));

impl FileTable {
    /// The snapshot's file table, cached per database and load.
    pub async fn load(client: &impl GenericClient, snapshot: &Snapshot) -> Result<Arc<FileTable>> {
        let row = client
            .query_opt(
                "SELECT current_database()::text, coalesce(loaded_at::text, ''), paths \
                 FROM code_snapshots WHERE id = $1",
                &[&snapshot.id],
            )
            .await
            .context("cannot read code_snapshots")?
            .with_context(|| format!("snapshot {} does not exist", snapshot.id))?;
        let key: CacheKey = (row.get(0), snapshot.id, row.get(1));
        let loaded_paths: Vec<String> = row.get(2);
        if let Some(table) = CACHE
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, t)| t.clone())
        {
            return Ok(table);
        }
        let rows = client
            .query(
                "SELECT id, path, is_binary, line_count, size FROM code_files \
                 WHERE snapshot_id = $1",
                &[&snapshot.id],
            )
            .await
            .context("cannot read code_files")?;
        let files = rows
            .iter()
            .map(|row| FileEntry {
                id: row.get(0),
                path: row.get(1),
                is_binary: row.get(2),
                line_count: row.get(3),
                size: row.get(4),
            })
            .collect();
        let table = Arc::new(FileTable::new(files, loaded_paths));
        let mut cache = CACHE.lock().unwrap();
        cache.retain(|(k, _)| *k != key);
        cache.push((key, table.clone()));
        if cache.len() > 16 {
            cache.remove(0);
        }
        Ok(table)
    }

    pub fn new(mut files: Vec<FileEntry>, loaded_paths: Vec<String>) -> Self {
        files.sort_by(|a, b| tree_cmp(&a.path, &b.path));
        let by_path = files
            .iter()
            .enumerate()
            .map(|(k, f)| (f.path.clone(), k))
            .collect();
        let mut dirs = HashSet::new();
        let mut top = HashSet::new();
        for f in &files {
            let mut end = 0;
            while let Some(k) = f.path[end..].find('/') {
                end += k;
                dirs.insert(f.path[..end].to_owned());
                end += 1;
            }
            top.insert(f.path.split('/').next().unwrap_or_default().to_owned());
        }
        Self {
            files,
            by_path,
            dirs,
            top,
            loaded_paths,
        }
    }

    /// The file at `path`.
    pub fn file(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied()
    }

    /// Whether `path` is a folder ("" is the root).
    pub fn is_dir(&self, path: &str) -> bool {
        path.is_empty() || self.dirs.contains(path)
    }

    /// The files below folder `dir`, as a range of indices.
    pub fn under(&self, dir: &str) -> std::ops::Range<usize> {
        if dir.is_empty() {
            return 0..self.files.len();
        }
        let start = self
            .files
            .partition_point(|f| f.path.split('/').lt(dir.split('/')));
        let end = start
            + self.files[start..].partition_point(|f| {
                f.path.starts_with(dir) && f.path.as_bytes().get(dir.len()) == Some(&b'/')
            });
        start..end
    }

    /// The folders that hold files, for listings.
    pub fn dirs(&self) -> impl Iterator<Item = &str> {
        self.dirs.iter().map(String::as_str)
    }

    /// A tool path as a snapshot path: `./x`, `/x`, `x/` and `a/../x` are
    /// `x`, and an absolute or `~` path that contains one of the
    /// snapshot's top-level folders is taken from that folder on
    /// (`/home/me/postgres/src/backend` is `src/backend`). Returns None when
    /// the path cannot be placed in the snapshot.
    pub fn normalize(&self, repo: &str, raw: &str) -> Option<String> {
        let raw = raw.trim();
        let absolute = raw.starts_with('/') || raw.starts_with('~') || raw.starts_with('$');
        let comps = lexical_components(raw)?;
        if !absolute {
            return Some(comps.join("/"));
        }
        let joined = comps.join("/");
        if self.exists(&joined) && !joined.is_empty() {
            return Some(joined);
        }
        let mut fallback = None;
        for k in 0..comps.len() {
            if self.top.contains(&comps[k]) {
                let rest = comps[k..].join("/");
                if self.exists(&rest) {
                    return Some(rest);
                }
                fallback.get_or_insert(rest);
            }
        }
        if fallback.is_none()
            && comps
                .last()
                .is_some_and(|last| last.eq_ignore_ascii_case(repo))
        {
            return Some(String::new());
        }
        fallback
    }

    /// Whether `path` is a file or a folder.
    pub fn exists(&self, path: &str) -> bool {
        self.file(path).is_some() || self.is_dir(path)
    }
}

/// `path` split into components, with `.`, empty components and `..` resolved
/// lexically; None when `..` climbs above the root.
fn lexical_components(path: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            c => out.push(c.to_owned()),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> FileTable {
        let paths = [
            "src/backend/parser/gram.y",
            "src/backend/parser/scan.l",
            "src/backend/utils/adt/json.c",
            "src/include/nodes/nodes.h",
            "contrib/pg_trgm/trgm_op.c",
            "src/backend-x/a.c",
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
    fn files_are_in_tree_order() {
        let t = table();
        let order: Vec<&str> = t.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "contrib/pg_trgm/trgm_op.c",
                "src/backend/parser/gram.y",
                "src/backend/parser/scan.l",
                "src/backend/utils/adt/json.c",
                "src/backend-x/a.c",
                "src/include/nodes/nodes.h",
            ]
        );
        let under: Vec<&str> = t
            .under("src/backend")
            .map(|k| t.files[k].path.as_str())
            .collect();
        assert_eq!(under.len(), 3);
        assert!(under.iter().all(|p| p.starts_with("src/backend/")));
        assert_eq!(t.under("").len(), 6);
        assert!(t.under("src/backend/parser/gram.y").is_empty());
    }

    #[test]
    fn normalizes_tool_paths() {
        let t = table();
        let n = |p: &str| t.normalize("postgres", p);
        assert_eq!(n("./src/backend/").as_deref(), Some("src/backend"));
        assert_eq!(n("/src/backend").as_deref(), Some("src/backend"));
        assert_eq!(n("src//backend/../include").as_deref(), Some("src/include"));
        assert_eq!(
            n("/home/me/src/postgres/src/backend/parser").as_deref(),
            Some("src/backend/parser")
        );
        assert_eq!(
            n("~/postgres/contrib/pg_trgm").as_deref(),
            Some("contrib/pg_trgm")
        );
        assert_eq!(n("/home/me/postgres").as_deref(), Some(""));
        assert_eq!(n(".").as_deref(), Some(""));
        assert_eq!(n("/home/me/other"), None);
        assert!(t.is_dir("src/backend") && !t.is_dir("src/back"));
    }

    #[test]
    fn versions_normalize() {
        for v in ["19", "PG19", "pg 19", "v19", "REL_19_STABLE", "postgres19"] {
            assert_eq!(normalize_version(v), "19", "{v}");
        }
    }
}
