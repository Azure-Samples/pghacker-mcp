//! Rebuilds files from the history and checks them against git: at each
//! commit, the files alive must be exactly git's, and each text file's
//! lines, in weave order, must hash to its blob's SHA.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use tokio_postgres::Client;

use super::store::readable_schema;

/// What a check found.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Verified {
    pub commits: u64,
    pub files: u64,
    pub text_files: u64,
    pub lines: u64,
    pub mismatch_count: u64,
    /// The first mismatches, described.
    pub mismatches: Vec<String>,
    /// Commits where index and sequential scans returned different lines.
    pub scan_disagreements: u64,
}

impl Verified {
    fn mismatch(&mut self, what: String) {
        self.mismatch_count += 1;
        if self.mismatches.len() < 50 {
            self.mismatches.push(what);
        }
    }
}

/// The blobs of `commit`'s tree under `paths`: what the history must hold.
pub fn git_files(
    repo: &gix::Repository,
    commit: ObjectId,
    paths: &[String],
) -> Result<BTreeMap<String, ObjectId>> {
    let tree = repo.find_commit(commit)?.tree()?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .context("cannot walk the commit's tree")?;
    let mut out = BTreeMap::new();
    for entry in recorder.records {
        if !entry.mode.is_blob() {
            continue;
        }
        let path = entry
            .filepath
            .to_str()
            .map_err(|_| anyhow!("a path isn't UTF-8: {:?}", entry.filepath))?
            .to_owned();
        if crate::in_paths(&path, paths) {
            out.insert(path, entry.oid);
        }
    }
    Ok(out)
}

/// The SQL that a row is alive at address `$1`.
pub fn alive_at(alias: &str) -> String {
    format!(
        "{alias}.lo <= $1 AND {alias}.hi > $1 AND ({alias}.alive IS NULL OR {alias}.alive @> $1)"
    )
}

/// Checks each commit (full SHAs).
pub async fn verify(
    client: &Client,
    schema: &str,
    repo: &gix::Repository,
    paths: &[String],
    commits: &[ObjectId],
    check_scans: bool,
) -> Result<Verified> {
    let s = readable_schema(schema)?;
    let mut out = Verified::default();
    for &commit in commits {
        let sha = commit.to_string();
        let addr: i64 = client
            .query_opt(
                &format!("SELECT addr FROM {s}.code_commits WHERE sha = $1"),
                &[&sha],
            )
            .await?
            .with_context(|| format!("the history has no commit {sha}"))?
            .get(0);
        let expected = git_files(repo, commit, paths)?;
        let rows = client
            .query(
                &format!(
                    "SELECT v.lineage, v.path, v.blob_sha, v.is_binary, v.final_newline, v.bom \
                     FROM {s}.code_file_versions v WHERE {}",
                    alive_at("v")
                ),
                &[&addr],
            )
            .await?;
        out.commits += 1;
        let mut text: HashMap<i32, (String, ObjectId, bool, bool)> = HashMap::new();
        let mut seen = 0usize;
        for row in &rows {
            let lineage: i32 = row.get(0);
            let path: String = row.get(1);
            let blob: String = row.get(2);
            out.files += 1;
            let blob_id = ObjectId::from_hex(blob.as_bytes())
                .with_context(|| format!("{path} has an invalid blob SHA {blob}"))?;
            match expected.get(&path) {
                None => out.mismatch(format!("{sha} {path}: in the history, not in git")),
                Some(id) if *id != blob_id => out.mismatch(format!(
                    "{sha} {path}: blob {blob} in the history, {id} in git"
                )),
                Some(_) => seen += 1,
            }
            if !row.get::<_, bool>(3) {
                out.text_files += 1;
                let previous = text.insert(lineage, (path, blob_id, row.get(4), row.get(5)));
                if let Some((other, ..)) = previous {
                    out.mismatch(format!("{sha}: lineage {lineage} is alive twice ({other})"));
                }
            }
        }
        if seen < expected.len() {
            let have: std::collections::HashSet<String> =
                rows.iter().map(|r| r.get::<_, String>(1)).collect();
            for path in expected.keys().filter(|p| !have.contains(*p)) {
                out.mismatch(format!("{sha} {path}: in git, not in the history"));
            }
        }

        let lineages: Vec<i32> = text.keys().copied().collect();
        let line_sql = format!(
            "SELECT l.lineage, l.line FROM {s}.code_line_instances l \
             WHERE l.lineage = ANY($2) AND {} ORDER BY l.lineage, l.weave",
            alive_at("l")
        );
        let lines = client.query(&line_sql, &[&addr, &lineages]).await?;
        let mut content: HashMap<i32, Vec<u8>> = HashMap::new();
        let mut count: HashMap<i32, usize> = HashMap::new();
        for row in &lines {
            let lineage: i32 = row.get(0);
            let line: &str = row.get(1);
            let buf = content.entry(lineage).or_default();
            let n = count.entry(lineage).or_default();
            if *n > 0 {
                buf.push(b'\n');
            }
            buf.extend_from_slice(line.as_bytes());
            *n += 1;
            out.lines += 1;
        }
        for (lineage, (path, blob, final_newline, bom)) in &text {
            let mut data = Vec::new();
            if *bom {
                data.extend_from_slice("\u{feff}".as_bytes());
            }
            data.extend_from_slice(content.get(lineage).map(Vec::as_slice).unwrap_or_default());
            if *final_newline {
                data.push(b'\n');
            }
            let id = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, &data)
                .map_err(|e| anyhow!("cannot hash {path}: {e}"))?;
            if id != *blob {
                out.mismatch(format!(
                    "{sha} {path}: the lines rebuild blob {id}, not {blob}"
                ));
            }
        }

        if check_scans {
            let digest = |rows: &[tokio_postgres::Row]| -> (usize, u64) {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for row in rows {
                    row.get::<_, i32>(0).hash(&mut h);
                    row.get::<_, &str>(1).hash(&mut h);
                }
                (rows.len(), h.finish())
            };
            let indexed = digest(&lines);
            client
                .batch_execute(
                    "BEGIN; SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off; \
                     SET LOCAL enable_indexonlyscan = off",
                )
                .await?;
            let scanned = client.query(&line_sql, &[&addr, &lineages]).await;
            client.batch_execute("ROLLBACK").await?;
            if digest(&scanned?) != indexed {
                out.scan_disagreements += 1;
                out.mismatch(format!("{sha}: index and sequential scans disagree"));
            }
        }
    }
    Ok(out)
}
