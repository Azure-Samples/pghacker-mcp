//! The hourly history sync: fetch every branch and tag of the history's
//! repository into the indexer's clone, then apply the new commits.

use anyhow::{Context, Result};
use pghacker_code::history::{fetch, sync};
use serde_json::{Value, json};

use crate::Indexer;

/// The clone's folder below the git directory.
pub const CLONE: &str = "postgres-history.git";

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
    drop(client);
    let Some(row) = installed else {
        return Ok(json!({ "skipped": "no history is installed" }));
    };
    let Some(url) = row.get::<_, Option<String>>(0) else {
        return Ok(json!({ "skipped": "the history has no URL to fetch from" }));
    };
    let dir = indexer.settings.git_dir.join(CLONE);
    let clone = dir.clone();
    tokio::task::spawn_blocking(move || fetch::update_clone(&url, &clone))
        .await
        .context("the fetch panicked")??;
    let client =
        pghacker_core::db::connect(&indexer.settings.database_url, "pghacker-indexer history")
            .await?;
    let stats = sync::sync(
        client,
        sync::SyncOptions {
            git_dir: dir,
            schema: "pghacker".into(),
            batch: 200,
        },
    )
    .await?;
    if stats.commits > 0 {
        tracing::info!(commits = stats.commits, "synced the history");
    }
    Ok(serde_json::to_value(stats)?)
}
