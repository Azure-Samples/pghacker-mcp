//! The embeddings backfill: message chunks and extracts whose embedding is
//! still NULL. Search works without embeddings, in full text mode.

use anyhow::Result;
use serde::Serialize;

use crate::Indexer;

/// Chunks per embeddings request batch.
const BATCH: i64 = 128;

#[derive(Debug, Default, Serialize)]
pub struct BackfillReport {
    pub chunks: usize,
    pub extracts: usize,
    /// Whether rows are still waiting.
    pub more: bool,
    pub enabled: bool,
}

/// Embeds up to `max_batches` batches.
pub async fn backfill(indexer: &Indexer, max_batches: usize) -> Result<BackfillReport> {
    let Some(embedder) = &indexer.embedder else {
        return Ok(BackfillReport::default());
    };
    let mut report = BackfillReport {
        enabled: true,
        ..BackfillReport::default()
    };
    let mut client = indexer.pool.get().await?;
    for _ in 0..max_batches {
        let rows = client
            .query(
                "SELECT id, content FROM message_chunks WHERE embedding IS NULL ORDER BY id LIMIT $1",
                &[&BATCH],
            )
            .await?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
        let texts: Vec<String> = rows.iter().map(|row| row.get(1)).collect();
        let vectors = embedder.embed(&texts).await?;
        let tx = client.transaction().await?;
        let update = tx
            .prepare("UPDATE message_chunks SET embedding = $2 WHERE id = $1")
            .await?;
        for (id, vector) in ids.iter().zip(&vectors) {
            tx.execute(&update, &[id, vector]).await?;
        }
        tx.commit().await?;
        report.chunks += ids.len();
    }
    for (table, text) in [
        ("features", "search_text"),
        ("decisions", "search_text"),
        ("positions", "search_text"),
    ] {
        let rows = client
            .query(
                &format!(
                    "SELECT id, {text} FROM {table} WHERE embedding IS NULL ORDER BY id LIMIT $1"
                ),
                &[&BATCH],
            )
            .await?;
        if rows.is_empty() {
            continue;
        }
        let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
        let texts: Vec<String> = rows.iter().map(|row| row.get(1)).collect();
        let vectors = embedder.embed(&texts).await?;
        let tx = client.transaction().await?;
        let update = tx
            .prepare(&format!("UPDATE {table} SET embedding = $2 WHERE id = $1"))
            .await?;
        for (id, vector) in ids.iter().zip(&vectors) {
            tx.execute(&update, &[id, vector]).await?;
        }
        tx.commit().await?;
        report.extracts += ids.len();
    }
    report.more = client
        .query_one(
            "SELECT EXISTS (SELECT FROM message_chunks WHERE embedding IS NULL)
                 OR EXISTS (SELECT FROM features WHERE embedding IS NULL)
                 OR EXISTS (SELECT FROM decisions WHERE embedding IS NULL)
                 OR EXISTS (SELECT FROM positions WHERE embedding IS NULL)",
            &[],
        )
        .await?
        .get(0);
    if report.chunks + report.extracts > 0 {
        tracing::info!(
            chunks = report.chunks,
            extracts = report.extracts,
            more = report.more,
            "embedded"
        );
    }
    Ok(report)
}
