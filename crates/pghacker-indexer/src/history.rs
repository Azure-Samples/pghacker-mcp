// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The hourly history sync: fetch every branch and tag of the history's
//! repository into the indexer's clone, then apply the new commits.
//!
//! The graph projects the installed history, whose tables have no triggers:
//! a sync of `pghacker` records in `graph_changes` what each batch of
//! commits changes, in the batch's transaction, as do the links' refresh and
//! the regroup that follow (`pghacker_code::history::sync`), and
//! `history build --install` asks for a full graph sync.

use anyhow::{Context, Result};
use pghacker_code::history::{fetch, sync};
use serde_json::{Value, json};

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
    // The graph follows what the sync recorded, even after a failure: each
    // batch of commits records its changes as it commits.
    jobs::wake(&**indexer.pool.get().await?, kind::GRAPH_SYNC).await?;
    let stats = synced?;
    if stats.commits > 0 {
        tracing::info!(commits = stats.commits, "synced the history");
    }
    Ok(serde_json::to_value(stats)?)
}
