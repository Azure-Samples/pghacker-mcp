//! Projects the tables into the AGE graph `pghacker_graph`. Each sync
//! computes the wanted vertices and edges with SQL, diffs them against
//! `graph_nodes` and `graph_edges`, and writes only the difference straight
//! into AGE's label tables, which is much faster than Cypher MERGE. Vertex
//! properties never include emails.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::{Client, Transaction};

use pghacker_core::{EDGE_LABELS, GRAPH, VERTEX_LABELS};

#[derive(Debug, Default, Serialize)]
pub struct SyncStats {
    pub nodes_added: u64,
    pub nodes_updated: u64,
    pub nodes_removed: u64,
    pub edges_added: u64,
    pub edges_removed: u64,
    pub ms: u64,
}

const WANT_NODES: &str = r"
INSERT INTO want_nodes (key, label, props)
SELECT 'author:' || a.id, 'Author', jsonb_strip_nulls(jsonb_build_object(
    'key', 'author:' || a.id, 'id', a.id, 'name', a.display_name, 'messages', a.message_count,
    'first_seen', a.first_seen, 'last_seen', a.last_seen))
FROM authors a WHERE a.merged_into IS NULL
UNION ALL
SELECT 'thread:' || t.id, 'Thread', jsonb_strip_nulls(jsonb_build_object(
    'key', 'thread:' || t.id, 'id', t.id, 'subject', t.subject, 'root_message_id', t.root_message_id,
    'messages', t.message_count, 'started_at', t.started_at, 'last_at', t.last_at))
FROM threads t
UNION ALL
SELECT 'message:' || m.message_id, 'Message', jsonb_strip_nulls(jsonb_build_object(
    'key', 'message:' || m.message_id, 'id', m.id, 'message_id', m.message_id, 'subject', m.subject,
    'sent_at', m.sent_at, 'has_patch', m.has_patch, 'thread_id', m.thread_id, 'author_id', m.author_id))
FROM messages m
UNION ALL
SELECT 'feature:' || f.slug, 'Feature', jsonb_strip_nulls(jsonb_build_object(
    'key', 'feature:' || f.slug, 'id', f.id, 'slug', f.slug, 'name', f.name, 'status', f.status,
    'release', f.release))
FROM features f WHERE f.merged_into IS NULL
UNION ALL
SELECT 'decision:' || d.id, 'Decision', jsonb_strip_nulls(jsonb_build_object(
    'key', 'decision:' || d.id, 'id', d.id, 'title', d.title, 'outcome', d.outcome,
    'decided_at', d.decided_at, 'thread_id', d.thread_id))
FROM decisions d WHERE d.merged_into IS NULL
UNION ALL
SELECT 'position:' || p.id, 'Position', jsonb_strip_nulls(jsonb_build_object(
    'key', 'position:' || p.id, 'id', p.id, 'stance', p.stance, 'summary', p.summary,
    'author_id', p.author_id, 'thread_id', p.thread_id))
FROM positions p
UNION ALL
SELECT 'release:' || r.v, 'Release', jsonb_build_object('key', 'release:' || r.v, 'version', r.v)
FROM (SELECT release AS v FROM features WHERE release IS NOT NULL AND merged_into IS NULL
      UNION SELECT value FROM anchors WHERE kind = 'release') r
UNION ALL
SELECT 'subsystem:' || x.subsystem, 'Subsystem', jsonb_build_object('key', 'subsystem:' || x.subsystem, 'name', x.subsystem)
FROM (SELECT DISTINCT f.subsystem FROM code_files f JOIN code_snapshots s ON s.id = f.snapshot_id
      WHERE s.state = 'ready' AND s.repo = 'postgres' AND f.subsystem IS NOT NULL) x
UNION ALL
SELECT 'file:' || p.path, 'File', jsonb_strip_nulls(jsonb_build_object(
    'key', 'file:' || p.path, 'path', p.path, 'subsystem', p.subsystem, 'loaded', p.loaded))
FROM (
    SELECT path, max(subsystem) AS subsystem, bool_or(loaded) AS loaded FROM (
        SELECT f.path, f.subsystem, true AS loaded
        FROM code_files f JOIN code_snapshots s ON s.id = f.snapshot_id
        WHERE s.state = 'ready' AND s.repo = 'postgres'
        UNION ALL SELECT a.value, NULL, false FROM anchors a WHERE a.kind = 'file'
        UNION ALL SELECT ff.path, NULL, false FROM feature_files ff
    ) x GROUP BY path
) p
UNION ALL
SELECT 'commit:' || x.value, 'Commit', jsonb_build_object('key', 'commit:' || x.value, 'sha', x.value)
FROM (SELECT DISTINCT value FROM anchors WHERE kind = 'commit') x
UNION ALL
SELECT 'cf:' || x.value, 'CommitFestEntry', jsonb_build_object(
    'key', 'cf:' || x.value, 'entry', x.value, 'url', 'https://commitfest.postgresql.org/patch/' || x.value || '/')
FROM (SELECT DISTINCT value FROM anchors WHERE kind = 'commitfest') x
ON CONFLICT (key) DO NOTHING";

const WANT_EDGES: &str = r"
INSERT INTO want_edges (label, start_key, end_key)
SELECT 'AUTHORED', 'author:' || m.author_id, 'message:' || m.message_id FROM messages m WHERE m.author_id IS NOT NULL
UNION ALL
SELECT 'REPLIES_TO', 'message:' || m.message_id, 'message:' || m.parent_message_id
FROM messages m WHERE m.parent_message_id IS NOT NULL
UNION ALL
SELECT 'IN_THREAD', 'message:' || m.message_id, 'thread:' || m.thread_id FROM messages m
UNION ALL
SELECT 'MENTIONS', 'message:' || m.message_id,
       CASE a.kind WHEN 'file' THEN 'file:' WHEN 'commit' THEN 'commit:' WHEN 'commitfest' THEN 'cf:' ELSE 'release:' END || a.value
FROM anchors a JOIN messages m ON m.id = a.message_id
UNION ALL
SELECT 'PART_OF', 'file:' || f.path, 'subsystem:' || f.subsystem
FROM code_files f JOIN code_snapshots s ON s.id = f.snapshot_id
WHERE s.state = 'ready' AND s.repo = 'postgres' AND f.subsystem IS NOT NULL
UNION ALL
SELECT 'DISCUSSES', 'thread:' || ft.thread_id, 'feature:' || f.slug
FROM feature_threads ft JOIN features f ON f.id = ft.feature_id
UNION ALL
SELECT 'DECIDES', 'decision:' || d.id, 'feature:' || f.slug
FROM decisions d JOIN features f ON f.id = d.feature_id
UNION ALL
SELECT 'TARGETS', 'feature:' || f.slug, 'release:' || f.release FROM features f WHERE f.release IS NOT NULL
UNION ALL
SELECT 'IMPLEMENTS', 'file:' || ff.path, 'feature:' || f.slug
FROM feature_files ff JOIN features f ON f.id = ff.feature_id
UNION ALL
SELECT 'HOLDS', 'author:' || p.author_id, 'position:' || p.id FROM positions p
UNION ALL
SELECT 'ABOUT', 'position:' || p.id, 'feature:' || f.slug FROM positions p JOIN features f ON f.id = p.feature_id
UNION ALL
SELECT 'ABOUT', 'position:' || p.id, 'decision:' || p.decision_id FROM positions p WHERE p.decision_id IS NOT NULL
UNION ALL
SELECT 'EVIDENCED_BY',
       CASE c.extract_kind WHEN 'feature' THEN 'feature:' || f.slug ELSE c.extract_kind || ':' || c.extract_id END,
       'message:' || m.message_id
FROM citations c JOIN messages m ON m.id = c.message_id
LEFT JOIN features f ON c.extract_kind = 'feature' AND f.id = c.extract_id
ON CONFLICT DO NOTHING";

/// AGE's id and id sequence of each label.
async fn labels(tx: &Transaction<'_>) -> Result<HashMap<String, (i32, String)>> {
    let rows = tx
        .query(
            "SELECT l.name::text, l.id::int, l.seq_name::text FROM ag_catalog.ag_label l
             JOIN ag_catalog.ag_graph g ON g.graphid = l.graph WHERE g.name = $1",
            &[&GRAPH],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let seq: String = row.get(2);
            (row.get(0), (row.get(1), format!("{GRAPH}.\"{seq}\"")))
        })
        .collect())
}

/// Brings the graph up to date with the tables, in one transaction.
pub async fn sync(client: &mut Client) -> Result<SyncStats> {
    let started = Instant::now();
    let tx = client.transaction().await?;
    tx.batch_execute(
        "SELECT pg_advisory_xact_lock(hashtext('pghacker.graph_sync'));
         CREATE TEMP TABLE want_nodes (key text PRIMARY KEY, label text NOT NULL, props jsonb NOT NULL) ON COMMIT DROP;
         CREATE TEMP TABLE want_edges (label text, start_key text, end_key text,
                                       PRIMARY KEY (label, start_key, end_key)) ON COMMIT DROP;",
    )
    .await?;
    tx.batch_execute(WANT_NODES)
        .await
        .context("cannot compute the graph's vertices")?;
    tx.batch_execute(WANT_EDGES)
        .await
        .context("cannot compute the graph's edges")?;
    tx.batch_execute(
        "DELETE FROM want_edges w
         WHERE NOT EXISTS (SELECT FROM want_nodes n WHERE n.key = w.start_key)
            OR NOT EXISTS (SELECT FROM want_nodes n WHERE n.key = w.end_key);
         ANALYZE want_nodes; ANALYZE want_edges;",
    )
    .await?;
    let labels = labels(&tx).await?;
    let mut stats = SyncStats::default();

    for label in EDGE_LABELS {
        stats.edges_removed += tx
            .execute(
                &format!(
                    "DELETE FROM {GRAPH}.\"{label}\" e USING graph_edges g
                     WHERE g.label = $1 AND e.id = g.graph_id
                       AND NOT EXISTS (SELECT FROM want_edges w
                                       WHERE w.label = g.label AND w.start_key = g.start_key AND w.end_key = g.end_key)"
                ),
                &[label],
            )
            .await?;
        tx.execute(
            "DELETE FROM graph_edges g WHERE g.label = $1
               AND NOT EXISTS (SELECT FROM want_edges w
                               WHERE w.label = g.label AND w.start_key = g.start_key AND w.end_key = g.end_key)",
            &[label],
        )
        .await?;
    }
    for label in VERTEX_LABELS {
        // Edges of a removed vertex were removed above: they're only wanted between wanted vertices.
        stats.nodes_removed += tx
            .execute(
                &format!(
                    "DELETE FROM {GRAPH}.\"{label}\" v USING graph_nodes g
                     WHERE g.label = $1 AND v.id = g.graph_id
                       AND NOT EXISTS (SELECT FROM want_nodes w WHERE w.key = g.key)"
                ),
                &[label],
            )
            .await?;
        tx.execute(
            "DELETE FROM graph_nodes g WHERE g.label = $1 AND NOT EXISTS (SELECT FROM want_nodes w WHERE w.key = g.key)",
            &[label],
        )
        .await?;
        stats.nodes_updated += tx
            .execute(
                &format!(
                    "UPDATE {GRAPH}.\"{label}\" v SET properties = w.props::text::ag_catalog.agtype
                     FROM graph_nodes g JOIN want_nodes w ON w.key = g.key
                     WHERE g.label = $1 AND v.id = g.graph_id AND g.props IS DISTINCT FROM w.props"
                ),
                &[label],
            )
            .await?;
        tx.execute(
            "UPDATE graph_nodes g SET props = w.props, updated_at = now() FROM want_nodes w
             WHERE w.key = g.key AND g.label = $1 AND g.props IS DISTINCT FROM w.props",
            &[label],
        )
        .await?;
        let (label_id, seq) = labels
            .get(*label)
            .with_context(|| format!("the graph has no label {label}; run bootstrap"))?;
        stats.nodes_added += tx
            .execute(
                &format!(
                    "WITH new AS (
                         SELECT w.key, w.props, ag_catalog._graphid($2, nextval($3::text::regclass)) AS gid
                         FROM want_nodes w
                         WHERE w.label = $1 AND NOT EXISTS (SELECT FROM graph_nodes g WHERE g.key = w.key)
                     ), ins AS (
                         INSERT INTO {GRAPH}.\"{label}\" (id, properties)
                         SELECT gid, props::text::ag_catalog.agtype FROM new
                     )
                     INSERT INTO graph_nodes (key, label, graph_id, props) SELECT key, $1, gid, props FROM new"
                ),
                &[label, label_id, seq],
            )
            .await?;
    }
    for label in EDGE_LABELS {
        let (label_id, seq) = labels
            .get(*label)
            .with_context(|| format!("the graph has no label {label}; run bootstrap"))?;
        stats.edges_added += tx
            .execute(
                &format!(
                    "WITH new AS (
                         SELECT w.start_key, w.end_key, s.graph_id AS sid, e.graph_id AS eid,
                                ag_catalog._graphid($2, nextval($3::text::regclass)) AS gid
                         FROM want_edges w
                         JOIN graph_nodes s ON s.key = w.start_key
                         JOIN graph_nodes e ON e.key = w.end_key
                         WHERE w.label = $1 AND NOT EXISTS (
                             SELECT FROM graph_edges g
                             WHERE g.label = w.label AND g.start_key = w.start_key AND g.end_key = w.end_key)
                     ), ins AS (
                         INSERT INTO {GRAPH}.\"{label}\" (id, start_id, end_id, properties)
                         SELECT gid, sid, eid, '{{}}'::ag_catalog.agtype FROM new
                     )
                     INSERT INTO graph_edges (label, start_key, end_key, graph_id)
                     SELECT $1, start_key, end_key, gid FROM new"
                ),
                &[label, label_id, seq],
            )
            .await?;
    }
    stats.ms = started.elapsed().as_millis() as u64;
    let summary = serde_json::to_value(&stats)?;
    tx.execute(
        "INSERT INTO graph_sync_runs (finished_at, stats) VALUES (now(), $1)",
        &[&summary],
    )
    .await?;
    tx.execute(
        "DELETE FROM graph_sync_runs WHERE id < (SELECT max(id) - 100 FROM graph_sync_runs)",
        &[],
    )
    .await?;
    tx.commit().await?;
    let changed = stats.nodes_added
        + stats.nodes_updated
        + stats.nodes_removed
        + stats.edges_added
        + stats.edges_removed;
    if changed > 0 {
        tracing::info!(?stats, "synced the graph");
    }
    Ok(stats)
}
