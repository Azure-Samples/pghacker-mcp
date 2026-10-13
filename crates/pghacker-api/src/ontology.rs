//! get_ontology: what's in the graph and the tables, how to query them, and
//! what's loaded.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::code::{LATEST_FIRST, newest_first};
use pghacker_core::{EDGE_LABELS, GRAPH, VERTEX_LABELS};

/// Each vertex label's properties.
const VERTEX_PROPERTIES: &[(&str, &str)] = &[
    ("Author", "key, id, name, messages, first_seen, last_seen"),
    (
        "Thread",
        "key, id, subject, root_message_id, messages, started_at, last_at",
    ),
    (
        "Message",
        "key, id, message_id, subject, sent_at, has_patch, thread_id, author_id",
    ),
    ("Feature", "key, id, slug, name, status, release"),
    ("Decision", "key, id, title, outcome, decided_at, thread_id"),
    ("Position", "key, id, stance, summary, author_id, thread_id"),
    ("Release", "key, version"),
    ("Subsystem", "key, name"),
    ("File", "key, path, subsystem, loaded"),
    ("Commit", "key, sha"),
    ("CommitFestEntry", "key, entry, url"),
];

/// Each edge type's endpoints.
const EDGE_ENDPOINTS: &[(&str, &str)] = &[
    ("AUTHORED", "Author -> Message"),
    ("REPLIES_TO", "Message -> Message"),
    ("IN_THREAD", "Message -> Thread"),
    (
        "MENTIONS",
        "Message -> File, Commit, CommitFestEntry or Release",
    ),
    ("PART_OF", "File -> Subsystem"),
    ("DISCUSSES", "Thread -> Feature"),
    ("DECIDES", "Decision -> Feature"),
    ("TARGETS", "Feature -> Release"),
    ("IMPLEMENTS", "File -> Feature"),
    ("HOLDS", "Author -> Position"),
    ("ABOUT", "Position -> Feature or Decision"),
    ("EVIDENCED_BY", "Feature, Decision or Position -> Message"),
    ("MODIFIES", "Commit -> File (wave 2)"),
    ("DISCUSSED_IN", "Commit -> Thread (wave 2)"),
];

/// A label's vertices or edges: as the graph sync counted them, or else
/// counted now.
async fn label_count(
    client: &impl GenericClient,
    kept: &HashMap<String, i64>,
    label: &str,
) -> Result<i64> {
    if let Some(count) = kept.get(label) {
        return Ok(*count);
    }
    Ok(client
        .query_one(&format!("SELECT count(*) FROM {GRAPH}.\"{label}\""), &[])
        .await?
        .get(0))
}

pub async fn get_ontology(client: &impl GenericClient) -> Result<Value> {
    let kept: HashMap<String, i64> = client
        .query("SELECT label, count FROM graph_label_counts", &[])
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let mut vertices = Vec::new();
    for label in VERTEX_LABELS {
        let count = label_count(client, &kept, label).await?;
        let properties = VERTEX_PROPERTIES
            .iter()
            .find(|(l, _)| l == label)
            .map_or("", |(_, p)| *p);
        vertices.push(json!({ "label": label, "count": count, "properties": properties }));
    }
    let mut edges = Vec::new();
    for label in EDGE_LABELS {
        let count = label_count(client, &kept, label).await?;
        let endpoints = EDGE_ENDPOINTS
            .iter()
            .find(|(l, _)| l == label)
            .map_or("", |(_, e)| *e);
        edges.push(json!({ "type": label, "count": count, "connects": endpoints }));
    }
    let tables: Vec<Value> = client
        .query(
            "SELECT c.table_name, string_agg(c.column_name || ' ' || c.data_type, ', ' ORDER BY c.ordinal_position)
             FROM information_schema.columns c
             WHERE c.table_schema = 'pghacker' AND c.column_name NOT IN ('embedding', 'search_text')
               AND has_table_privilege(current_user, format('%I.%I', c.table_schema, c.table_name), 'SELECT')
               AND c.table_name NOT IN ('schema_migrations', 'graph_label_counts')
             GROUP BY c.table_name ORDER BY c.table_name",
            &[],
        )
        .await?
        .iter()
        .map(|row| json!({ "table": row.get::<_, String>(0), "columns": row.get::<_, String>(1) }))
        .collect();
    let coverage = client
        .query_one(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM threads),
                    (SELECT count(*) FROM authors WHERE merged_into IS NULL),
                    (SELECT min(sent_at) FROM messages), (SELECT max(sent_at) FROM messages),
                    (SELECT array_agg(DISTINCT archive_month ORDER BY archive_month) FROM messages WHERE archive_month IS NOT NULL),
                    (SELECT count(*) FROM threads WHERE extraction_state <> 'pending'),
                    (SELECT count(*) FROM features WHERE merged_into IS NULL),
                    (SELECT count(*) FROM decisions WHERE merged_into IS NULL),
                    (SELECT count(*) FROM positions),
                    (SELECT count(*) FROM message_chunks WHERE embedding IS NOT NULL),
                    (SELECT count(*) FROM message_chunks)",
            &[],
        )
        .await?;
    // One entry per version, newest first: its default snapshot, the latest
    // (the newest commit), and the refs of the others, which stay readable.
    let mut code: Vec<(String, Option<String>, Value, Vec<String>)> = Vec::new();
    for row in client
        .query(
            &format!(
                "SELECT repo, version, ref, sha, paths, file_count, line_count, committed_at
                 FROM code_snapshots WHERE state = 'ready' ORDER BY {LATEST_FIRST}"
            ),
            &[],
        )
        .await?
    {
        let (repo, version, git_ref): (String, Option<String>, String) =
            (row.get(0), row.get(1), row.get(2));
        match code
            .iter_mut()
            .find(|(r, v, ..)| *r == repo && *v == version)
        {
            Some((.., others)) => others.push(git_ref),
            None => {
                let latest = json!({ "ref": git_ref, "sha": row.get::<_, String>(3),
                                     "committed_at": row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(7),
                                     "paths": row.get::<_, Vec<String>>(4), "files": row.get::<_, i32>(5),
                                     "lines": row.get::<_, i64>(6) });
                code.push((repo, version, latest, Vec::new()));
            }
        }
    }
    code.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| newest_first(a.1.as_deref().unwrap_or(""), b.1.as_deref().unwrap_or("")))
    });
    let code: Vec<Value> = code
        .into_iter()
        .map(|(repo, version, latest, others)| {
            json!({ "repo": repo, "version": version, "default": latest, "other_refs": others })
        })
        .collect();
    Ok(json!({
        "graph": {
            "name": GRAPH,
            "vertices": vertices,
            "edges": edges,
            "keys": "Every vertex has a unique key property: author:<id>, thread:<id>, message:<Message-ID>, \
                     feature:<slug>, decision:<id>, position:<id>, release:<major>, subsystem:<name>, file:<path>, \
                     commit:<sha>, cf:<entry>.",
        },
        "tables": tables,
        "examples": {
            "cypher": [
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author)-[:HOLDS]->(p:Position)-[:ABOUT]->(f:Feature {slug: 'sql-pgq'}) RETURN a.name, p.stance, p.summary $$) AS (author agtype, stance agtype, summary agtype)",
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (t:Thread)-[:DISCUSSES]->(f:Feature) RETURN f.name, count(t) ORDER BY count(t) DESC LIMIT 10 $$) AS (feature agtype, threads agtype)",
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (m:Message)-[:MENTIONS]->(f:File)-[:PART_OF]->(s:Subsystem {name: 'parser'}) RETURN f.path, count(m) ORDER BY count(m) DESC LIMIT 10 $$) AS (path agtype, mentions agtype)",
            ],
            "sql": [
                "SELECT a.display_name, count(*) FROM messages m JOIN authors a ON a.id = m.author_id GROUP BY 1 ORDER BY 2 DESC LIMIT 10",
                "SELECT f.slug, f.status, f.release, count(ft.thread_id) AS threads FROM features f LEFT JOIN feature_threads ft ON ft.feature_id = f.id WHERE f.merged_into IS NULL GROUP BY f.id ORDER BY threads DESC",
            ],
        },
        "search_modes": {
            "fulltext": "BM25 ranking of the words (pg_textsearch)",
            "semantic": "cosine similarity of embeddings (pgvector, text-embedding-3-small)",
            "hybrid": "both rankings fused with reciprocal rank fusion (default)",
        },
        "loaded": {
            "mail": {
                "list": "pgsql-hackers",
                "messages": coverage.get::<_, i64>(0),
                "threads": coverage.get::<_, i64>(1),
                "authors": coverage.get::<_, i64>(2),
                "from": coverage.get::<_, Option<chrono::DateTime<chrono::Utc>>>(3),
                "to": coverage.get::<_, Option<chrono::DateTime<chrono::Utc>>>(4),
                "months": coverage.get::<_, Option<Vec<String>>>(5).unwrap_or_default(),
                "note": "Whole months plus whole seed threads on SQL/PGQ and SQL/JSON, including older ones.",
            },
            "extraction": {
                "threads_extracted": coverage.get::<_, i64>(6),
                "features": coverage.get::<_, i64>(7),
                "decisions": coverage.get::<_, i64>(8),
                "positions": coverage.get::<_, i64>(9),
            },
            "embeddings": { "chunks_embedded": coverage.get::<_, i64>(10), "chunks": coverage.get::<_, i64>(11) },
            "code": code,
            "code_note": "A snapshot is the tree as of committed_at: for a release branch, what that release contains; \
                          work committed to master after the branch point goes to a later release. The code tools read \
                          a version's default, its latest snapshot; name a release (18.3), a ref from other_refs or a \
                          commit SHA as the version to read another, and no version for the newest release line.",
        },
    }))
}
