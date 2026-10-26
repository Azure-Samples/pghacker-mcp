// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Prewarms PostgreSQL's `shared_buffers` with every table, TOAST table and
//! index of schemas `pghacker` and `pghacker_graph` (`pg_prewarm`), so that
//! searches after a database start don't wait on cold heap, TOAST or HNSW/BM25
//! index reads.
//!
//! Runs when the leading indexer starts, and again whenever
//! `pg_postmaster_start_time()` changes, as after a database restart. Mail and
//! extract relations are prewarmed first, then the commit history, then the
//! AGE graph. When `pg_prewarm` isn't installed on the server, it logs a
//! warning and skips.

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio_postgres::GenericClient;
use tokio_util::sync::CancellationToken;

/// How often the watcher checks whether the database restarted.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// Every table, materialized view, TOAST table and index (including TOAST
/// indexes) of `pghacker` and `pghacker_graph`, ordered with mail and extract
/// relations first, then the commit history, then the graph.
const RELATIONS_SQL: &str = "
WITH base AS (
    SELECT c.oid, n.nspname AS schema_name, c.relname, c.relkind::text AS kind, c.reltoastrelid,
           CASE
               WHEN n.nspname = 'pghacker' AND c.relname NOT LIKE 'code_%' THEN 0
               WHEN n.nspname = 'pghacker' THEN 1
               ELSE 2
           END AS priority
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname IN ('pghacker', 'pghacker_graph')
      AND c.relkind IN ('r', 'm', 'i')
),
toast AS (
    SELECT t.oid, 'pg_toast'::text AS schema_name, t.relname, t.relkind::text AS kind,
           b.priority
    FROM base b
    JOIN pg_class t ON t.oid = b.reltoastrelid
    WHERE b.reltoastrelid <> 0
),
toast_idx AS (
    SELECT ic.oid, 'pg_toast'::text AS schema_name, ic.relname, ic.relkind::text AS kind,
           t.priority
    FROM toast t
    JOIN pg_index i ON i.indrelid = t.oid
    JOIN pg_class ic ON ic.oid = i.indexrelid
),
relations AS (
    SELECT oid, schema_name, relname, kind, priority FROM base
    UNION
    SELECT oid, schema_name, relname, kind, priority FROM toast
    UNION
    SELECT oid, schema_name, relname, kind, priority FROM toast_idx
)
SELECT oid, kind
FROM relations
ORDER BY priority, kind, schema_name, relname
";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Tables (`r`, `m`) prewarmed.
    pub tables: u64,
    /// TOAST tables (`t`) prewarmed.
    pub toast: u64,
    /// Indexes (`i`, including TOAST indexes) prewarmed.
    pub indexes: u64,
    /// 8 KB blocks loaded into `shared_buffers`.
    pub blocks: u64,
}

/// Prewarms every table, TOAST table and index of `pghacker` and
/// `pghacker_graph` into `shared_buffers`, stopping early if `stop` is
/// cancelled. Returns `None` when `pg_prewarm` isn't installed, or when
/// `stop` cancelled the run.
pub async fn all(client: &impl GenericClient, stop: &CancellationToken) -> Result<Option<Report>> {
    let started = Instant::now();
    let rows = client.query(RELATIONS_SQL, &[]).await?;
    let mut report = Report::default();
    for row in rows {
        if stop.is_cancelled() {
            return Ok(None);
        }
        let oid: u32 = row.get(0);
        let kind: String = row.get(1);
        let blocks: Option<i64> = client
            .query_one("SELECT pghacker.prewarm_relation($1)", &[&oid])
            .await?
            .get(0);
        let Some(blocks) = blocks else {
            tracing::warn!("pg_prewarm is not installed; skipping shared_buffers prewarm");
            return Ok(None);
        };
        match kind.as_str() {
            "r" | "m" => report.tables += 1,
            "t" => report.toast += 1,
            "i" => report.indexes += 1,
            _ => {}
        }
        report.blocks += blocks.max(0) as u64;
    }
    tracing::info!(
        tables = report.tables,
        toast = report.toast,
        indexes = report.indexes,
        blocks = report.blocks,
        ms = started.elapsed().as_millis() as u64,
        "prewarmed shared_buffers"
    );
    Ok(Some(report))
}

/// Prewarms `shared_buffers` on start and whenever `pg_postmaster_start_time()`
/// changes, until `stop` is cancelled.
pub async fn watch(database_url: String, stop: CancellationToken) {
    let mut last_start: Option<DateTime<Utc>> = None;
    while !stop.is_cancelled() {
        let step = async {
            let client =
                pghacker_core::db::connect(&database_url, "pghacker-indexer prewarm").await?;
            let started_at: DateTime<Utc> = client
                .query_one("SELECT pg_postmaster_start_time()", &[])
                .await?
                .get(0);
            if last_start != Some(started_at) {
                all(&client, &stop).await?;
                if !stop.is_cancelled() {
                    last_start = Some(started_at);
                }
            }
            anyhow::Ok(())
        };
        if let Err(error) = step.await {
            tracing::warn!(error = %format!("{error:#}"), "cannot prewarm shared_buffers");
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(CHECK_EVERY) => {}
        }
    }
}
