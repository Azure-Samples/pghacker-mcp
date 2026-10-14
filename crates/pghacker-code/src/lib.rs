//! Loads a git commit's files into `code_snapshots`, `code_files` and
//! `code_lines`: one row per line, which pg_trgm indexes for code search.
//!
//! The exactness replay relies on these rules: every tracked file under
//! the configured path prefixes;
//! symlinks and submodules skipped; a blob with a NUL byte, or that isn't
//! UTF-8, is binary (a file row without lines); a leading UTF-8 BOM is
//! dropped; lines split on `\n`, a trailing `\r` staying in its line; the empty
//! element after a final newline is dropped; an empty file has no lines.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use tokio_postgres::Client;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::Type;

pub mod history;

/// Where a repository comes from.
#[derive(Clone, Debug)]
pub enum RepoSource {
    /// Fetched, only the pinned commit, into `cache_dir/<repo>-<sha>.git`.
    Remote { url: String, cache_dir: PathBuf },
    /// An existing clone that has the commit.
    Local { path: PathBuf },
}

/// A commit to load.
#[derive(Clone, Debug)]
pub struct SnapshotSpec {
    /// The repository's name: `postgres`, or a replay repository's.
    pub repo: String,
    /// The branch or tag the SHA was pinned from.
    pub git_ref: String,
    pub sha: String,
    /// The PostgreSQL major version, such as `19`.
    pub version: Option<String>,
    /// Path prefixes to load; empty loads the whole tree.
    pub paths: Vec<String>,
    pub source: RepoSource,
    pub source_id: Option<i64>,
}

/// What a load did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LoadStats {
    pub snapshot_id: i32,
    pub files: u64,
    pub text_files: u64,
    pub binary_files: u64,
    pub skipped_links: u64,
    pub lines: u64,
    pub bytes: u64,
    /// The snapshot was already loaded.
    pub reused: bool,
}

/// A file read from the commit.
struct TreeFile {
    path: String,
    blob_sha: String,
    data: Vec<u8>,
}

struct Tree {
    files: Vec<TreeFile>,
    skipped_links: u64,
    committed_at: DateTime<Utc>,
}

/// A text file's lines and whether it ends with a newline, per the loader's
/// rules; `None` for binary content.
pub fn split_lines(data: &[u8]) -> Option<(Vec<&str>, bool)> {
    if data.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.is_empty() {
        return Some((Vec::new(), false));
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    let final_newline = lines.len() > 1 && lines.last() == Some(&"");
    if final_newline {
        lines.pop();
    }
    Some((lines, final_newline))
}

/// The subsystem a PostgreSQL path belongs to: `parser` for
/// `src/backend/parser/gram.y`, `utils/adt` for `src/backend/utils/adt/json.c`,
/// `nodes` for `src/include/nodes/parsenodes.h`, `contrib/pg_trgm`, and so on.
pub fn subsystem_of(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let dir = |n: usize| (parts.len() > n + 1).then(|| parts[n]);
    match parts.as_slice() {
        ["src", "backend" | "include", "utils", ..] => {
            Some(dir(3).map_or_else(|| "utils".to_owned(), |sub| format!("utils/{sub}")))
        }
        ["src", "backend" | "include", ..] => dir(2).map(str::to_owned),
        ["src", "bin" | "interfaces" | "pl", ..] => dir(2).map(|sub| format!("{}/{sub}", parts[1])),
        ["src", "test", ..] => Some("test".to_owned()),
        ["src", ..] => dir(1).map(str::to_owned),
        ["contrib", ..] => dir(1).map(|sub| format!("contrib/{sub}")),
        [top, _, ..] => Some((*top).to_owned()),
        _ => None,
    }
}

pub(crate) fn in_paths(path: &str, prefixes: &[String]) -> bool {
    prefixes.is_empty()
        || prefixes.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// Where a [`RepoSource::Remote`] fetch of `repo` at `sha` goes in `cache_dir`.
pub fn remote_clone_dir(cache_dir: &Path, repo: &str, sha: &str) -> PathBuf {
    cache_dir.join(format!("{repo}-{sha}.git"))
}

/// The repository with the spec's commit, fetching it if needed (blocking).
fn open_repo(spec: &SnapshotSpec) -> Result<gix::Repository> {
    let id = ObjectId::from_hex(spec.sha.as_bytes())
        .with_context(|| format!("{} is not a full SHA", spec.sha))?;
    match &spec.source {
        RepoSource::Local { path } => {
            let repo =
                gix::open(path).with_context(|| format!("cannot open {}", path.display()))?;
            repo.find_commit(id)
                .with_context(|| format!("{} has no commit {}", path.display(), spec.sha))?;
            Ok(repo)
        }
        RepoSource::Remote { url, cache_dir } => {
            let dir = remote_clone_dir(cache_dir, &spec.repo, &spec.sha);
            if dir.exists() {
                if let Ok(repo) = gix::open(&dir)
                    && repo.find_commit(id).is_ok()
                {
                    return Ok(repo);
                }
                std::fs::remove_dir_all(&dir).with_context(|| {
                    format!("cannot remove the incomplete clone {}", dir.display())
                })?;
            }
            std::fs::create_dir_all(cache_dir)?;
            tracing::info!(%url, sha = %spec.sha, "fetching the commit");
            let interrupt = AtomicBool::new(false);
            let (repo, _) = gix::prepare_clone_bare(url.as_str(), &dir)
                .context("cannot prepare the clone")?
                .with_revision(Some(spec.sha.as_str()))
                .context("cannot pin the revision")?
                .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(NonZeroU32::MIN))
                .fetch_only(gix::progress::Discard, &interrupt)
                .with_context(|| format!("cannot fetch {} from {url}", spec.sha))?;
            repo.find_commit(id)
                .with_context(|| format!("the fetch from {url} has no commit {}", spec.sha))?;
            Ok(repo)
        }
    }
}

/// When the commit was committed: its branch's state at that moment.
fn commit_time(commit: &gix::Commit<'_>) -> Result<DateTime<Utc>> {
    let time = commit.time().context("cannot read the commit's time")?;
    DateTime::from_timestamp(time.seconds, 0)
        .ok_or_else(|| anyhow!("the commit's time is out of range"))
}

/// When the spec's commit was committed (blocking).
fn spec_commit_time(spec: &SnapshotSpec) -> Result<DateTime<Utc>> {
    let repo = open_repo(spec)?;
    let commit = repo.find_commit(ObjectId::from_hex(spec.sha.as_bytes())?)?;
    commit_time(&commit)
}

/// The commit's files under the spec's paths, sorted by path (blocking).
fn read_tree(spec: &SnapshotSpec) -> Result<Tree> {
    let repo = open_repo(spec)?;
    let id = ObjectId::from_hex(spec.sha.as_bytes())?;
    let commit = repo.find_commit(id)?;
    let committed_at = commit_time(&commit)?;
    let tree = commit.tree()?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .context("cannot walk the commit's tree")?;
    let mut files = Vec::new();
    let mut skipped_links = 0;
    for entry in recorder.records {
        let path = entry
            .filepath
            .to_str()
            .map_err(|_| anyhow!("a path isn't UTF-8: {:?}", entry.filepath))?
            .to_owned();
        if !in_paths(&path, &spec.paths) {
            continue;
        }
        if entry.mode.is_link() || entry.mode.is_commit() {
            skipped_links += 1;
            continue;
        }
        if !entry.mode.is_blob() {
            continue;
        }
        let blob = repo
            .find_blob(entry.oid)
            .with_context(|| format!("cannot read {path}"))?;
        files.push(TreeFile {
            path,
            blob_sha: entry.oid.to_string(),
            data: blob.data.clone(),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Tree {
        files,
        skipped_links,
        committed_at,
    })
}

/// Loads the snapshot, or reuses it when it's already loaded. A snapshot left
/// loading or failed by an earlier attempt is replaced.
pub async fn load_snapshot(client: &mut Client, spec: &SnapshotSpec) -> Result<LoadStats> {
    if let Some(row) = client
        .query_opt(
            "SELECT s.id, s.file_count, s.line_count, s.bytes,
                    count(f.id) FILTER (WHERE NOT f.is_binary), count(f.id) FILTER (WHERE f.is_binary),
                    s.committed_at IS NULL
             FROM code_snapshots s LEFT JOIN code_files f ON f.snapshot_id = s.id
             WHERE s.repo = $1 AND s.sha = $2 AND s.state = 'ready'
             GROUP BY s.id",
            &[&spec.repo, &spec.sha],
        )
        .await?
    {
        // Snapshots loaded before commit times were recorded get theirs now.
        if row.get::<_, bool>(6) {
            let time_spec = spec.clone();
            match tokio::task::spawn_blocking(move || spec_commit_time(&time_spec))
                .await
                .context("the commit reader panicked")?
            {
                Ok(committed_at) => {
                    client
                        .execute(
                            "UPDATE code_snapshots SET committed_at = $2 WHERE id = $1",
                            &[&row.get::<_, i32>(0), &committed_at],
                        )
                        .await?;
                }
                Err(error) => {
                    tracing::warn!(sha = %spec.sha, %error, "cannot read the commit's time")
                }
            }
        }
        return Ok(LoadStats {
            snapshot_id: row.get(0),
            files: row.get::<_, i32>(1) as u64,
            lines: row.get::<_, i64>(2) as u64,
            bytes: row.get::<_, i64>(3) as u64,
            text_files: row.get::<_, i64>(4) as u64,
            binary_files: row.get::<_, i64>(5) as u64,
            reused: true,
            ..LoadStats::default()
        });
    }
    let started = std::time::Instant::now();
    let read_spec = spec.clone();
    let tree = tokio::task::spawn_blocking(move || read_tree(&read_spec))
        .await
        .context("the tree reader panicked")??;
    tracing::info!(
        repo = %spec.repo, sha = %spec.sha, files = tree.files.len(),
        ms = started.elapsed().as_millis() as u64, "read the tree"
    );

    let tx = client.transaction().await?;
    tx.execute(
        "DELETE FROM code_snapshots WHERE repo = $1 AND sha = $2",
        &[&spec.repo, &spec.sha],
    )
    .await?;
    let snapshot_id: i32 = tx
        .query_one(
            "INSERT INTO code_snapshots (source_id, repo, ref, sha, version, paths, state, committed_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'loading', $7) RETURNING id",
            &[
                &spec.source_id,
                &spec.repo,
                &spec.git_ref,
                &spec.sha,
                &spec.version,
                &spec.paths,
                &tree.committed_at,
            ],
        )
        .await?
        .get(0);

    let mut stats = LoadStats {
        snapshot_id,
        skipped_links: tree.skipped_links,
        ..LoadStats::default()
    };
    let count = tree.files.len();
    let mut paths = Vec::with_capacity(count);
    let mut subsystems = Vec::with_capacity(count);
    let mut blob_shas = Vec::with_capacity(count);
    let mut sizes = Vec::with_capacity(count);
    let mut line_counts = Vec::with_capacity(count);
    let mut binaries = Vec::with_capacity(count);
    let mut final_newlines = Vec::with_capacity(count);
    let mut texts: Vec<Option<Vec<&str>>> = Vec::with_capacity(count);
    for file in &tree.files {
        let size =
            i32::try_from(file.data.len()).map_err(|_| anyhow!("{} is too large", file.path))?;
        paths.push(file.path.as_str());
        subsystems.push(subsystem_of(&file.path));
        blob_shas.push(file.blob_sha.as_str());
        sizes.push(size);
        stats.files += 1;
        match split_lines(&file.data) {
            Some((lines, final_newline)) => {
                stats.text_files += 1;
                stats.lines += lines.len() as u64;
                stats.bytes += file.data.len() as u64;
                line_counts.push(lines.len() as i32);
                binaries.push(false);
                final_newlines.push(final_newline);
                texts.push(Some(lines));
            }
            None => {
                stats.binary_files += 1;
                line_counts.push(0);
                binaries.push(true);
                final_newlines.push(false);
                texts.push(None);
            }
        }
    }
    tx.execute(
        "INSERT INTO code_files (snapshot_id, path, subsystem, blob_sha, size, line_count, is_binary, final_newline)
         SELECT $1, * FROM unnest($2::text[], $3::text[], $4::text[], $5::int[], $6::int[], $7::bool[], $8::bool[])",
        &[&snapshot_id, &paths, &subsystems, &blob_shas, &sizes, &line_counts, &binaries, &final_newlines],
    )
    .await?;
    let id_of: HashMap<String, i32> = tx
        .query(
            "SELECT path, id FROM code_files WHERE snapshot_id = $1",
            &[&snapshot_id],
        )
        .await?
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    if id_of.len() != paths.len() {
        bail!("inserted {} files of {}", id_of.len(), paths.len());
    }

    let sink = tx
        .copy_in("COPY code_lines (file_id, line_no, line) FROM STDIN BINARY")
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::INT4, Type::TEXT]);
    let mut writer = std::pin::pin!(writer);
    for (path, lines) in paths.iter().zip(&texts) {
        let Some(lines) = lines else { continue };
        let file_id = id_of[*path];
        for (index, line) in lines.iter().enumerate() {
            let line_no = (index + 1) as i32;
            writer.as_mut().write(&[&file_id, &line_no, line]).await?;
        }
    }
    let copied = writer.finish().await?;
    if copied != stats.lines {
        bail!("copied {copied} lines of {}", stats.lines);
    }
    tx.execute(
        "UPDATE code_snapshots SET state = 'ready', file_count = $2, line_count = $3, bytes = $4, loaded_at = now()
         WHERE id = $1",
        &[&snapshot_id, &(stats.files as i32), &(stats.lines as i64), &(stats.bytes as i64)],
    )
    .await?;
    tx.commit().await?;
    // Fresh statistics, or the planner won't use the trigram index on the new lines.
    client
        .batch_execute("VACUUM (ANALYZE) code_files, code_lines")
        .await
        .context("cannot analyze the code tables")?;
    tracing::info!(
        repo = %spec.repo, sha = %spec.sha, files = stats.files, lines = stats.lines,
        ms = started.elapsed().as_millis() as u64, "loaded the snapshot"
    );
    Ok(stats)
}

/// Writes the snapshot's text files, as they are in git, under `dir`: the tree
/// that ripgrep and grep search for ground truth. Binary files are left out,
/// as the loader leaves out their lines. Returns the number of files written.
/// A completed tree is reused; `<dir>.snapshot` records which commit it holds.
pub fn materialize(spec: &SnapshotSpec, dir: &Path) -> Result<u64> {
    let marker = dir.with_extension("snapshot");
    let tree = read_tree(spec)?;
    let text_files = tree
        .files
        .iter()
        .filter(|file| split_lines(&file.data).is_some());
    if dir.exists() && std::fs::read_to_string(&marker).is_ok_and(|sha| sha.trim() == spec.sha) {
        return Ok(text_files.count() as u64);
    }
    if dir.exists() {
        if !marker.exists() && std::fs::read_dir(dir)?.next().is_some() {
            bail!(
                "{} exists and isn't a materialized snapshot; refusing to replace it",
                dir.display()
            );
        }
        std::fs::remove_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&marker);
    let mut written = 0;
    for file in text_files {
        let target = dir.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &file.data)
            .with_context(|| format!("cannot write {}", target.display()))?;
        written += 1;
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(&marker, &spec.sha)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_like_the_study() {
        assert_eq!(split_lines(b""), Some((vec![], false)));
        assert_eq!(split_lines(b"a\nb\n"), Some((vec!["a", "b"], true)));
        assert_eq!(split_lines(b"a\nb"), Some((vec!["a", "b"], false)));
        assert_eq!(split_lines(b"\n"), Some((vec![""], true)));
        assert_eq!(split_lines(b"a\r\nb\r\n"), Some((vec!["a\r", "b\r"], true)));
        assert_eq!(
            split_lines("\u{feff}x\n".as_bytes()),
            Some((vec!["x"], true))
        );
        assert_eq!(split_lines(b"a\0b"), None);
        assert_eq!(split_lines(&[0xff, 0xfe, b'a']), None);
    }

    #[test]
    fn maps_paths_to_subsystems() {
        assert_eq!(
            subsystem_of("src/backend/parser/gram.y").as_deref(),
            Some("parser")
        );
        assert_eq!(
            subsystem_of("src/backend/utils/adt/jsonfuncs.c").as_deref(),
            Some("utils/adt")
        );
        assert_eq!(
            subsystem_of("src/include/nodes/parsenodes.h").as_deref(),
            Some("nodes")
        );
        assert_eq!(
            subsystem_of("src/include/utils/jsonb.h").as_deref(),
            Some("utils")
        );
        assert_eq!(
            subsystem_of("contrib/pg_trgm/trgm_op.c").as_deref(),
            Some("contrib/pg_trgm")
        );
        assert_eq!(
            subsystem_of("src/bin/psql/describe.c").as_deref(),
            Some("bin/psql")
        );
        assert_eq!(
            subsystem_of("doc/src/sgml/func.sgml").as_deref(),
            Some("doc")
        );
        assert_eq!(subsystem_of("README").as_deref(), None);
    }

    #[test]
    fn filters_by_path_prefix() {
        let prefixes = vec![
            "src/backend/parser".to_owned(),
            "contrib/pg_trgm/".to_owned(),
        ];
        assert!(in_paths("src/backend/parser/gram.y", &prefixes));
        assert!(in_paths("contrib/pg_trgm/trgm.h", &prefixes));
        assert!(!in_paths("src/backend/parser2/x.c", &prefixes));
        assert!(in_paths("anything", &[]));
    }
}
