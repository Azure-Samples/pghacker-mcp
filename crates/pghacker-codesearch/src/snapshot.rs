//! The commit a code tool reads and its file table: which files the commit
//! has in the commit history, in tree order, and how tool paths map onto
//! them.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use serde::Serialize;
use tokio_postgres::GenericClient;

/// A commit in the commit history, as the code tools read it: its
/// repository, the major version and ref it is read as, and its SHA.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub repo: String,
    pub version: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    /// Where to read the commit in the history.
    #[serde(skip)]
    pub history: crate::history::History,
}

/// The commit of `repo` that a code tool reads: the one
/// [`crate::rev::resolve`] finds for `version` in the commit history of
/// [`pghacker_core::code::history_schema`]. `version` names a major version
/// (`19`, `PG19`, `9`): its branch head; a release (`18.3`, `9.6.24`) or a
/// release line before 10 (`9.6`); a branch or tag (`REL_18_3`,
/// `REL_19_STABLE`, `master`); or a commit SHA prefix; any of them as of a
/// date (`REL_17_STABLE@2024-06-01`) or commits back (`master~3`). Without
/// one, the head of the newest numbered release line. Every commit in the
/// history is readable.
pub async fn resolve_snapshot(
    client: &impl GenericClient,
    repo: &str,
    version: Option<&str>,
) -> Result<Snapshot> {
    crate::rev::resolve(client, pghacker_core::code::history_schema(), repo, version).await
}

/// A file of a commit: a file version in the history, whose id is the
/// version's.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub id: i32,
    pub path: String,
    pub is_binary: bool,
    pub line_count: i32,
    pub size: i32,
}

/// Where mounted trees live in a table: a first component that sorts after
/// every real one. Tools name them by their virtual roots, as
/// `/branch/REL_17_STABLE/src`, which tables hold as `~v/branch/...`.
pub const MOUNTS: &str = "~v";

/// The virtual roots that mount another commit's tree.
pub const ROOTS: [&str; 3] = ["commit", "branch", "tag"];

/// The tree a tool path mounts: ("branch", "REL_17_STABLE") for
/// `/branch/REL_17_STABLE/src`.
pub fn mount_of(raw: &str) -> Option<(String, String)> {
    let rest = raw.trim().strip_prefix('/')?;
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let root = parts.next()?;
    if !ROOTS.contains(&root) {
        return None;
    }
    Some((root.to_owned(), parts.next()?.to_owned()))
}

/// Whether a tool path is a virtual root without a commit: `/commit`.
pub fn bare_root(raw: &str) -> bool {
    let trimmed = raw.trim().trim_end_matches('/');
    trimmed
        .strip_prefix('/')
        .is_some_and(|root| ROOTS.contains(&root))
}

/// A table path as tools show it: mounted trees under their virtual roots.
pub fn shown(path: &str) -> String {
    match path.strip_prefix("~v/") {
        Some(rest) => format!("/{rest}"),
        None => path.to_owned(),
    }
}

/// A commit's files in tree order (paths compared component by component,
/// the order of a sorted directory walk), with its folders. Mounted trees
/// follow the commit's own files.
#[derive(Debug)]
pub struct FileTable {
    pub files: Vec<FileEntry>,
    /// The commit's own files: `files[..own]`.
    pub own: usize,
    by_path: HashMap<String, usize>,
    dirs: HashSet<String>,
    /// First path components: what an absolute path must contain.
    top: HashSet<String>,
    /// The path prefixes the history keeps; empty means the whole tree.
    pub loaded_paths: Vec<String>,
}

impl FileTable {
    /// The table a call reads: the commit's files, and the trees that the
    /// call's paths mount (`/commit/<rev>`, `/branch/<name>`,
    /// `/tag/<name>`). A mount that names no commit is left out, so its
    /// paths don't exist.
    pub async fn view(
        client: &impl GenericClient,
        snapshot: &Snapshot,
        paths: &[String],
    ) -> Result<Arc<FileTable>> {
        let base = FileTable::load(client, snapshot).await?;
        let h = &snapshot.history;
        let mut mounts: Vec<(String, String)> = paths.iter().filter_map(|p| mount_of(p)).collect();
        mounts.sort();
        mounts.dedup();
        if mounts.is_empty() {
            return Ok(base);
        }
        let mut files = base.files.clone();
        for (root, name) in mounts {
            let mounted =
                match crate::rev::resolve_mount(client, &h.schema, &snapshot.repo, &root, &name)
                    .await
                {
                    Ok(mounted) => mounted,
                    Err(error) => {
                        tracing::debug!(%root, %name, %error, "a mount names no commit");
                        continue;
                    }
                };
            let table = FileTable::load(client, &mounted).await?;
            files.extend(table.files.iter().map(|f| FileEntry {
                path: format!("{MOUNTS}/{root}/{name}/{}", f.path),
                ..f.clone()
            }));
        }
        Ok(Arc::new(FileTable::new(files, base.loaded_paths.clone())))
    }
}

fn cached(key: &CacheKey) -> Option<Arc<FileTable>> {
    CACHE
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, t)| t.clone())
}

fn remember(key: CacheKey, table: FileTable) -> Arc<FileTable> {
    let table = Arc::new(table);
    let mut cache = CACHE.lock().unwrap();
    cache.retain(|(k, _)| *k != key);
    cache.push((key, table.clone()));
    if cache.len() > 16 {
        cache.remove(0);
    }
    table
}

/// Compares paths component by component.
pub fn tree_cmp(a: &str, b: &str) -> Ordering {
    a.split('/').cmp(b.split('/'))
}

/// (build, schema@commit address): a commit's files never change within a
/// build, and the build names the database.
type CacheKey = (String, String);
type Cache = Mutex<Vec<(CacheKey, Arc<FileTable>)>>;

static CACHE: LazyLock<Cache> = LazyLock::new(|| Mutex::new(Vec::new()));

impl FileTable {
    /// The commit's file table, cached per build.
    pub async fn load(client: &impl GenericClient, snapshot: &Snapshot) -> Result<Arc<FileTable>> {
        let h = &snapshot.history;
        let key: CacheKey = (h.build.clone(), format!("{}@{}", h.schema, h.addr));
        if let Some(table) = cached(&key) {
            return Ok(table);
        }
        let (files, loaded_paths) = crate::history::files(client, h).await?;
        Ok(remember(key, FileTable::new(files, loaded_paths)))
    }

    pub fn new(mut files: Vec<FileEntry>, loaded_paths: Vec<String>) -> Self {
        files.sort_by(|a, b| tree_cmp(&a.path, &b.path));
        let own = files.partition_point(|f| !f.path.starts_with("~v/"));
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
            own,
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
            return 0..self.own;
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

    /// A tool path as a table path: `./x`, `/x`, `x/` and `a/../x` are `x`,
    /// and an absolute or `~` path that contains one of the commit's
    /// top-level folders is taken from that folder on
    /// (`/home/me/postgres/src/backend` is `src/backend`). Returns None when
    /// the path cannot be placed in the tree.
    pub fn normalize(&self, repo: &str, raw: &str) -> Option<String> {
        let raw = raw.trim();
        if mount_of(raw).is_some() || bare_root(raw) {
            return lexical_components(raw).map(|c| format!("{MOUNTS}/{}", c.join("/")));
        }
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
    fn mounted_trees_follow_the_commits_own() {
        let mut files: Vec<FileEntry> = table().files.clone();
        files.push(FileEntry {
            id: 1,
            path: "~v/branch/REL_17_STABLE/src/backend/parser/gram.y".into(),
            is_binary: false,
            line_count: 1,
            size: 1,
        });
        let t = FileTable::new(files, vec![]);
        assert_eq!(t.own, 6);
        assert_eq!(t.under("").len(), 6);
        let n = |p: &str| t.normalize("postgres", p);
        assert_eq!(
            n("/branch/REL_17_STABLE/src/").as_deref(),
            Some("~v/branch/REL_17_STABLE/src")
        );
        assert!(t.exists("~v/branch/REL_17_STABLE/src/backend"));
        assert_eq!(n("/commit").as_deref(), Some("~v/commit"));
        assert!(!t.exists("~v/commit"));
        assert_eq!(
            mount_of("/tag/REL_16_0/x"),
            Some(("tag".into(), "REL_16_0".into()))
        );
        assert_eq!(mount_of("/branch"), None);
        assert_eq!(mount_of("/src/branch/x"), None);
        assert!(bare_root("/commit/") && !bare_root("/commit/abc"));
        assert_eq!(
            shown("~v/branch/REL_17_STABLE/src/x.c"),
            "/branch/REL_17_STABLE/src/x.c"
        );
        assert_eq!(shown("src/x.c"), "src/x.c");
    }
}
