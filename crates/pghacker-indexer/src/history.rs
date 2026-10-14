//! The hourly history sync: fetch every branch and tag of the history's
//! repository into the indexer's clone, then apply the new commits.
//!
//! The graph's File and Subsystem vertices and PART_OF edges come from the
//! installed history's file versions, whose tables have no triggers: a sync
//! of `pghacker` records the graph entries of the versions it added itself
//! ([`capture_files`]), and `history build --install` asks for a full graph
//! sync.

use anyhow::{Context, Result};
use pghacker_code::history::{fetch, sync};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use crate::Indexer;
use crate::jobs::{self, kind};

/// The clone's folder below the git directory.
pub const CLONE: &str = "postgres-history.git";

/// The installed history in brief, for status and list_sources: None until
/// one is installed. Cheap: one row and three counts.
pub async fn summary(client: &impl tokio_postgres::GenericClient) -> Result<Option<Value>> {
    let row = client
        .query_opt(
            "SELECT jsonb_build_object(
                 'master', h.master, 'url', h.url, 'paths', h.paths, 'head', h.head,
                 'built_at', h.built_at, 'synced_at', h.synced_at,
                 'commits', (SELECT count(*) FROM code_commits),
                 'branches', (SELECT count(*) FROM code_refs WHERE kind = 'branch'),
                 'tags', (SELECT count(*) FROM code_refs WHERE kind = 'tag'))
             FROM code_history h",
            &[],
        )
        .await
        .context("cannot read code_history")?;
    Ok(row.map(|r| r.get(0)))
}

/// The newest of the installed history's file versions, -1 for none: a sync
/// adds versions with greater IDs. Schema-qualified, for any connection.
pub async fn newest_version(client: &impl GenericClient) -> Result<i32> {
    Ok(client
        .query_one(
            "SELECT coalesce(max(id), -1) FROM pghacker.code_file_versions",
            &[],
        )
        .await
        .context("cannot read code_file_versions")?
        .get(0))
}

/// Records in `graph_changes` the graph entries of the installed history's
/// file versions after `after`: their File and Subsystem vertices and PART_OF
/// edges, which the incremental graph sync then recomputes. Returns the
/// entries recorded. Schema-qualified, for any connection.
pub async fn capture_files(client: &impl GenericClient, after: i32) -> Result<u64> {
    client
        .execute(
            "INSERT INTO pghacker.graph_changes (label, key, end_key)
             SELECT DISTINCT e.*
             FROM (SELECT DISTINCT path, subsystem FROM pghacker.code_file_versions WHERE id > $1) v
             CROSS JOIN LATERAL pghacker.graph_code_file_entries(v.path, v.subsystem) e",
            &[&after],
        )
        .await
        .context("cannot record the history's new files for the graph")
}

/// Syncs the installed history; does nothing until one is installed.
pub async fn sync(indexer: &Indexer) -> Result<Value> {
    let client = indexer.pool.get().await?;
    let installed = client
        .query_opt(
            "SELECT url FROM code_history WHERE EXISTS (SELECT FROM code_commits)",
            &[],
        )
        .await
        .context("cannot read code_history")?;
    let Some(row) = installed else {
        return Ok(json!({ "skipped": "no history is installed" }));
    };
    let Some(url) = row.get::<_, Option<String>>(0) else {
        return Ok(json!({ "skipped": "the history has no URL to fetch from" }));
    };
    let newest = newest_version(&**client).await?;
    drop(client);
    let dir = indexer.settings.git_dir.join(CLONE);
    let clone = dir.clone();
    tokio::task::spawn_blocking(move || fetch::update_clone(&url, &clone))
        .await
        .context("the fetch panicked")??;
    let client =
        pghacker_core::db::connect(&indexer.settings.database_url, "pghacker-indexer history")
            .await?;
    let synced = sync::sync(
        client,
        sync::SyncOptions {
            git_dir: dir,
            schema: "pghacker".into(),
            batch: 200,
        },
    )
    .await;
    // Even after a failure: each batch of commits is committed on its own.
    let client = indexer.pool.get().await?;
    if capture_files(&**client, newest).await? > 0 {
        jobs::wake(&**client, kind::GRAPH_SYNC).await?;
    }
    let stats = synced?;
    if stats.commits > 0 {
        tracing::info!(commits = stats.commits, "synced the history");
    }
    Ok(serde_json::to_value(stats)?)
}
