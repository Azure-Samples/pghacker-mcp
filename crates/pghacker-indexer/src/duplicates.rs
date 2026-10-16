// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Duplicate candidates for find_duplicates. Each feature and decision, once
//! it arrives, is compared with the live ones of its kind: by its embedding's
//! nearest neighbours, on the HNSW index, and by its name's trigrams, on the
//! trigram index. The pairs that come close are kept, with both similarities.
//! A pair is compared once, when the later of its two is, so the work grows
//! with the new extracts rather than with every pair; an extract compared
//! before its embedding came is compared again once it has one.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use tokio_postgres::{Client, GenericClient};

use crate::extraction::{DraftKind, NAME_DUPLICATE};

/// The lowest semantic similarity kept: find_duplicates' `min_similarity`
/// doesn't go below it.
pub const SEMANTIC_FLOOR: f64 = 0.8;

/// Nearest neighbours fetched per extract. Real near-duplicates are few: a
/// handful per extract at most.
const NEIGHBOURS: i64 = 50;

/// Extracts compared per statement.
const BATCH: i64 = 100;

const KINDS: [DraftKind; 2] = [DraftKind::Feature, DraftKind::Decision];

#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Extracts compared.
    pub checked: u64,
    /// Pairs kept, new or again.
    pub found: u64,
    /// Whether extracts still wait.
    pub more: bool,
}

/// The table of `kind`'s extracts, and their name column.
fn table(kind: DraftKind) -> (&'static str, &'static str) {
    match kind {
        DraftKind::Feature => ("features", "name"),
        DraftKind::Decision => ("decisions", "title"),
    }
}

/// Live extracts waiting to be compared: never compared, or compared before
/// their embedding came. `$1` is the kind.
fn waiting(table: &str) -> String {
    format!(
        "{table} e WHERE e.merged_into IS NULL AND NOT EXISTS (
             SELECT FROM duplicate_checks c
             WHERE c.kind = $1 AND c.extract_id = e.id AND (c.semantic OR e.embedding IS NULL))"
    )
}

/// How many extracts of `kind` wait to be compared.
pub async fn pending(client: &impl GenericClient, kind: DraftKind) -> Result<i64> {
    let (table, _) = table(kind);
    Ok(client
        .query_one(
            &format!("SELECT count(*) FROM {}", waiting(table)),
            &[&kind.as_str()],
        )
        .await?
        .get(0))
}

/// Compares waiting extracts, a batch at a time, until none wait or
/// `budget` is spent.
pub async fn refresh(client: &mut Client, budget: Duration) -> Result<Report> {
    let started = Instant::now();
    let mut report = Report::default();
    let mut spent = false;
    'kinds: for kind in KINDS {
        loop {
            if started.elapsed() >= budget {
                spent = true;
                break 'kinds;
            }
            let (checked, found) = compare(client, kind).await?;
            if checked == 0 {
                break;
            }
            report.checked += checked;
            report.found += found;
        }
    }
    if spent {
        for kind in KINDS {
            if pending(&*client, kind).await? > 0 {
                report.more = true;
                break;
            }
        }
    }
    if report.checked > 0 {
        tracing::info!(
            checked = report.checked,
            found = report.found,
            more = report.more,
            ms = started.elapsed().as_millis() as u64,
            "compared extracts for duplicates"
        );
    }
    Ok(report)
}

/// Compares the next batch of `kind`'s waiting extracts with the live ones,
/// in one statement, so that what it records as compared is what it read.
/// Returns how many it compared and how many pairs it kept.
async fn compare(client: &mut Client, kind: DraftKind) -> Result<(u64, u64)> {
    let (table, label) = table(kind);
    let ids: Vec<i64> = client
        .query(
            &format!("SELECT e.id FROM {} ORDER BY e.id LIMIT $2", waiting(table)),
            &[&kind.as_str(), &BATCH],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    if ids.is_empty() {
        return Ok((0, 0));
    }
    let tx = client.transaction().await?;
    // The neighbours are whatever the index finds nearest, merged ones left
    // out as the scan goes (relaxed_order); names match from NAME_DUPLICATE.
    // The planner would rather sort a sequential scan of the table, counting
    // nothing for reading each row's embedding out of TOAST: 20 times slower.
    tx.batch_execute(&format!(
        "SET LOCAL statement_timeout = '60s';
         SET LOCAL enable_seqscan = off;
         SET LOCAL hnsw.ef_search = {ef_search};
         SET LOCAL hnsw.iterative_scan = relaxed_order;
         SET LOCAL pg_trgm.similarity_threshold = {NAME_DUPLICATE};",
        ef_search = NEIGHBOURS * 2,
    ))
    .await?;
    let row = tx
        .query_one(
            &format!(
                "WITH batch AS (
                     SELECT id, {label} AS label, embedding FROM {table} WHERE id = ANY ($2)
                 ),
                 near AS (
                     SELECT a.id AS a_id, n.id AS b_id
                     FROM batch a CROSS JOIN LATERAL (
                         SELECT b.id FROM {table} b
                         WHERE b.merged_into IS NULL AND b.id <> a.id AND b.embedding IS NOT NULL
                         ORDER BY b.embedding <=> a.embedding
                         LIMIT $3
                     ) n
                     WHERE a.embedding IS NOT NULL
                 ),
                 named AS (
                     SELECT a.id AS a_id, b.id AS b_id
                     FROM batch a JOIN {table} b ON b.{label} % a.label
                     WHERE b.merged_into IS NULL AND b.id <> a.id
                 ),
                 pairs AS (
                     SELECT DISTINCT least(a_id, b_id) AS low_id, greatest(a_id, b_id) AS high_id
                     FROM (SELECT a_id, b_id FROM near UNION ALL SELECT a_id, b_id FROM named) x
                 ),
                 scored AS (
                     SELECT p.low_id, p.high_id,
                            (1 - (l.embedding <=> h.embedding))::float8 AS semantic,
                            similarity(l.{label}, h.{label})::float8 AS name_similarity
                     FROM pairs p
                     JOIN {table} l ON l.id = p.low_id
                     JOIN {table} h ON h.id = p.high_id
                 ),
                 found AS (
                     INSERT INTO duplicate_candidates (kind, low_id, high_id, semantic, name_similarity)
                     SELECT $1, low_id, high_id, semantic, name_similarity FROM scored
                     WHERE semantic >= $4 OR name_similarity >= $5
                     ON CONFLICT (kind, low_id, high_id) DO UPDATE
                         SET semantic = EXCLUDED.semantic, name_similarity = EXCLUDED.name_similarity
                     RETURNING 1
                 ),
                 checked AS (
                     INSERT INTO duplicate_checks (kind, extract_id, semantic)
                     SELECT $1, id, embedding IS NOT NULL FROM batch
                     ON CONFLICT (kind, extract_id) DO UPDATE
                         SET semantic = EXCLUDED.semantic, checked_at = now()
                     RETURNING 1
                 )
                 SELECT (SELECT count(*) FROM checked), (SELECT count(*) FROM found)"
            ),
            &[
                &kind.as_str(),
                &ids,
                &NEIGHBOURS,
                &SEMANTIC_FLOOR,
                &NAME_DUPLICATE,
            ],
        )
        .await?;
    tx.commit().await?;
    Ok((row.get::<_, i64>(0) as u64, row.get::<_, i64>(1) as u64))
}
