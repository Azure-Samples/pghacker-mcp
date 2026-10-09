//! `ingest_status`: what each source has retrieved, parsed, embedded and
//! extracted, and the graph's size. Months are summarized per list, with the
//! unfinished and latest ones in detail: a whole archive has hundreds.

use anyhow::Result;
use serde_json::{Value, json};

use crate::Indexer;
use crate::jobs::kind;

/// The months detailed: unfinished first, then the latest.
const MONTHS_LISTED: i64 = 24;

pub async fn ingest_status(indexer: &Indexer) -> Result<Value> {
    let client = indexer.pool.get().await?;
    let jobs: Vec<Value> = client
        .query(
            "SELECT kind,
                    count(*) FILTER (WHERE state = 'queued'), count(*) FILTER (WHERE state = 'running'),
                    count(*) FILTER (WHERE state = 'done'), count(*) FILTER (WHERE state = 'failed'),
                    count(*) FILTER (WHERE state = 'cancelled'),
                    (array_agg(key || ': ' || last_error ORDER BY finished_at DESC)
                         FILTER (WHERE last_error IS NOT NULL))[1:3]
             FROM jobs WHERE key <> 'recurring' GROUP BY kind ORDER BY kind",
            &[],
        )
        .await?
        .iter()
        .map(|row| {
            json!({
                "kind": row.get::<_, String>(0), "queued": row.get::<_, i64>(1), "running": row.get::<_, i64>(2),
                "done": row.get::<_, i64>(3), "failed": row.get::<_, i64>(4), "cancelled": row.get::<_, i64>(5),
                "recent_errors": row.get::<_, Option<Vec<String>>>(6).unwrap_or_default(),
            })
        })
        .collect();
    let months: Vec<Value> = client
        .query(
            "WITH counts AS (
                 SELECT list, archive_month, count(*) AS n FROM messages
                 WHERE archive_month IS NOT NULL GROUP BY list, archive_month)
             SELECT j.payload->>'list', j.payload->>'month', j.state, j.result, j.last_error, coalesce(c.n, 0)
             FROM jobs j
             LEFT JOIN counts c ON c.list = j.payload->>'list' AND c.archive_month = j.payload->>'month'
             WHERE j.kind = $1
             ORDER BY j.state = 'done', j.payload->>'month' DESC, j.payload->>'list'
             LIMIT $2",
            &[&kind::MBOX_MONTH, &MONTHS_LISTED],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "list": row.get::<_, Option<String>>(0), "month": row.get::<_, Option<String>>(1),
                    "state": row.get::<_, String>(2), "stats": row.get::<_, Option<Value>>(3),
                    "error": row.get::<_, Option<String>>(4), "messages": row.get::<_, i64>(5) })
        })
        .collect();
    let lists: Vec<Value> = client
        .query(
            "SELECT s.name, coalesce((s.config->>'follow')::boolean, false), s.enabled,
                    count(j.*) FILTER (WHERE j.state = 'done'), count(j.*) FILTER (WHERE j.state = 'queued'),
                    count(j.*) FILTER (WHERE j.state = 'running'), count(j.*) FILTER (WHERE j.state = 'failed'),
                    min(j.payload->>'month'), max(j.payload->>'month')
             FROM sources s LEFT JOIN jobs j ON j.source_id = s.id AND j.kind = $1
             WHERE s.kind = 'mailing_list' GROUP BY s.id ORDER BY s.name",
            &[&kind::MBOX_MONTH],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "list": row.get::<_, String>(0), "follow": row.get::<_, bool>(1),
                    "enabled": row.get::<_, bool>(2),
                    "months": { "done": row.get::<_, i64>(3), "queued": row.get::<_, i64>(4),
                                "running": row.get::<_, i64>(5), "failed": row.get::<_, i64>(6),
                                "first": row.get::<_, Option<String>>(7), "last": row.get::<_, Option<String>>(8) } })
        })
        .collect();
    let seeds: Vec<Value> = client
        .query(
            "SELECT j.payload->>'topic', j.payload->>'message_id', j.state, j.last_error,
                    (SELECT t.message_count FROM messages m JOIN threads t ON t.id = m.thread_id
                     WHERE m.message_id = j.payload->>'message_id')
             FROM jobs j WHERE j.kind = $1 ORDER BY j.id",
            &[&kind::SEED_THREAD],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "topic": row.get::<_, Option<String>>(0), "message_id": row.get::<_, Option<String>>(1),
                    "state": row.get::<_, String>(2), "error": row.get::<_, Option<String>>(3),
                    "thread_messages": row.get::<_, Option<i32>>(4) })
        })
        .collect();
    let counts = client
        .query_one(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM threads),
                    (SELECT count(*) FROM authors WHERE merged_into IS NULL), (SELECT count(*) FROM patches),
                    (SELECT count(*) FROM message_chunks), (SELECT count(*) FROM message_chunks WHERE embedding IS NOT NULL),
                    (SELECT count(*) FROM threads WHERE extraction_state = 'pending'),
                    (SELECT count(*) FROM threads WHERE extraction_state = 'extracted'),
                    (SELECT count(*) FROM threads WHERE extraction_state = 'skipped'),
                    (SELECT count(*) FROM threads WHERE extraction_state = 'extracted' AND extracted_version < content_version),
                    (SELECT count(*) FROM extraction_leases WHERE released_at IS NULL AND expires_at > now()),
                    (SELECT count(*) FROM features WHERE merged_into IS NULL),
                    (SELECT count(*) FROM decisions WHERE merged_into IS NULL),
                    (SELECT count(*) FROM positions), (SELECT count(*) FROM citations)",
            &[],
        )
        .await?;
    let anchors: Value = client
        .query_one(
            "SELECT coalesce(jsonb_object_agg(kind, n), '{}') FROM (
                 SELECT kind, count(*) AS n FROM anchors GROUP BY kind
                 UNION ALL SELECT 'file_resolved', count(*) FROM anchors WHERE kind = 'file' AND resolved IS NOT NULL
             ) x",
            &[],
        )
        .await?
        .get(0);
    let snapshots: Vec<Value> = client
        .query(
            "SELECT repo, version, ref, sha, state, file_count, line_count, paths, loaded_at FROM code_snapshots ORDER BY id",
            &[],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "repo": row.get::<_, String>(0), "version": row.get::<_, Option<String>>(1),
                    "ref": row.get::<_, String>(2), "sha": row.get::<_, String>(3), "state": row.get::<_, String>(4),
                    "files": row.get::<_, i32>(5), "lines": row.get::<_, i64>(6), "paths": row.get::<_, Vec<String>>(7),
                    "loaded_at": row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(8) })
        })
        .collect();
    let graph: Value = client
        .query_one(
            "SELECT jsonb_build_object(
                 'nodes', (SELECT coalesce(jsonb_object_agg(label, n), '{}') FROM
                           (SELECT label, count(*) AS n FROM graph_nodes GROUP BY label) x),
                 'edges', (SELECT coalesce(jsonb_object_agg(label, n), '{}') FROM
                           (SELECT label, count(*) AS n FROM graph_edges GROUP BY label) y),
                 'last_sync', (SELECT jsonb_build_object('at', finished_at, 'stats', stats)
                               FROM graph_sync_runs ORDER BY id DESC LIMIT 1))",
            &[],
        )
        .await?
        .get(0);
    let chunks: i64 = counts.get(4);
    let embedded: i64 = counts.get(5);
    Ok(json!({
        "jobs": jobs,
        "mail": {
            "lists": lists,
            "months": months,
            "seed_threads": seeds,
            "messages": counts.get::<_, i64>(0),
            "threads": counts.get::<_, i64>(1),
            "authors": counts.get::<_, i64>(2),
            "patches": counts.get::<_, i64>(3),
            "anchors": anchors,
        },
        "embeddings": {
            "enabled": indexer.embedder.is_some(),
            "chunks": chunks,
            "embedded": embedded,
        },
        "extraction": {
            "scheduler": if indexer.settings.extraction_automatic { "automatic" } else { "manual" },
            "threads_pending": counts.get::<_, i64>(6),
            "threads_extracted": counts.get::<_, i64>(7),
            "threads_skipped": counts.get::<_, i64>(8),
            "threads_changed_since_extraction": counts.get::<_, i64>(9),
            "active_leases": counts.get::<_, i64>(10),
            "features": counts.get::<_, i64>(11),
            "decisions": counts.get::<_, i64>(12),
            "positions": counts.get::<_, i64>(13),
            "citations": counts.get::<_, i64>(14),
        },
        "code": { "snapshots": snapshots },
        "graph": graph,
    }))
}
