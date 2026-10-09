//! Resolves file anchors against loaded code snapshots: a path mentioned in
//! mail becomes a file ID, `REL_19_STABLE@<sha>:<path>`, from the newest
//! version that has it.

use anyhow::Result;
use tokio_postgres::GenericClient;

/// Resolves the file anchors that a snapshot now covers; returns how many.
pub async fn resolve(client: &impl GenericClient) -> Result<u64> {
    Ok(client
        .execute(
            "UPDATE anchors a SET resolved = r.file_id
             FROM (
                 SELECT DISTINCT ON (f.path) f.path, s.ref || '@' || s.sha || ':' || f.path AS file_id
                 FROM code_files f JOIN code_snapshots s ON s.id = f.snapshot_id
                 WHERE s.state = 'ready' AND s.repo = 'postgres'
                 ORDER BY f.path, (s.version ~ '^[0-9]+$') DESC,
                          CASE WHEN s.version ~ '^[0-9]+$' THEN s.version::int END DESC NULLS LAST,
                          s.loaded_at DESC
             ) r
             WHERE a.kind = 'file' AND a.value = r.path AND a.resolved IS DISTINCT FROM r.file_id",
            &[],
        )
        .await?)
}
