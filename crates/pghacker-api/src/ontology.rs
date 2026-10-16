// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! get_ontology: what's in the graph and the tables, how to query them, and
//! what's loaded.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::code::history_schema;
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
    (
        "Feature",
        "key, id, slug, name, status (the most advanced any of its threads showed: proposed, \
         in_progress, committed, reverted, rejected, returned, withdrawn or unknown; returned is \
         returned with feedback in a CommitFest), release (the major version it targets or shipped in)",
    ),
    ("Decision", "key, id, title, outcome, decided_at, thread_id"),
    ("Position", "key, id, stance, summary, author_id, thread_id"),
    ("Release", "key, version"),
    ("Subsystem", "key, name"),
    ("File", "key, path, subsystem, loaded"),
    (
        "Commit",
        "key, sha; and with the commit history: subject, committed_at, author (in PostgreSQL's \
         history, the committer's name), branch, backpatch (the SHA of its back-patch group's first \
         commit)",
    ),
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
    ("MODIFIES", "Commit -> File"),
    ("DISCUSSED_IN", "Commit -> Thread"),
    (
        "BACKPATCH_OF",
        "Commit -> Commit: each other commit of a back-patch group to the group's first",
    ),
    (
        "COMMITTED",
        "Author -> Commit: the one author whose name is the commit's author name",
    ),
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
    describe(client, history_schema()).await
}

/// How the code tools, and SQL, read the commit history.
const HISTORY_NOTE: &str = "The code tools read the commit history: every commit of every branch \
    and tag. version names a major version (its stable branch), a release (18.3, its tag), a branch, \
    tag or commit SHA, a branch or tag as of a date (REL_17_STABLE@2024-06-01) or commits back \
    (master~3); without one, `default`. Paths below /branch/<name>/, /tag/<name>/ or /commit/<rev>/ \
    read those trees in the same call. blame, get_commit and search_commits answer as git blame, git \
    show and git log. In SQL, code_commits.addr places a commit: master's k-th commit is k * 2^32, \
    and a branch's commits follow the master commit it forks from. A file version (code_file_versions) \
    or line instance (code_line_instances) is in commit X's tree when lo <= X AND X < hi AND \
    (alive IS NULL OR alive @> X); a version's lines are its lineage's instances alive at its lo, in \
    weave order.";

/// SQL over the commit history.
const HISTORY_SQL: &[&str] = &[
    "SELECT c.sha, c.committed_at, c.author_name, c.subject FROM code_commit_files f JOIN code_commits c ON c.addr = f.addr WHERE f.path = 'src/backend/parser/parse_jsontable.c' ORDER BY c.committed_at DESC LIMIT 10",
    "SELECT c.sha, c.subject, t.subject AS thread FROM code_commits c JOIN code_commit_threads ct ON ct.sha = c.sha JOIN threads t ON t.id = ct.thread_id WHERE c.subject ~* 'JSON_TABLE' ORDER BY c.committed_at LIMIT 10",
    "SELECT v.path, v.line_count FROM code_refs r JOIN code_file_versions v ON v.lo <= r.addr AND r.addr < v.hi AND (v.alive IS NULL OR v.alive @> r.addr) WHERE r.kind = 'branch' AND r.name = 'REL_17_STABLE' AND v.path LIKE 'src/backend/parser/%' ORDER BY v.path",
];

/// The commit history in `schema`, in brief: None until one is built.
async fn history_loaded(client: &impl GenericClient, schema: &str) -> Result<Value> {
    let s = format!("\"{}\"", schema.replace('"', "\"\""));
    let row = client
        .query_opt(
            &format!(
                "SELECT h.master, h.url, h.paths, h.head, h.built_at, h.synced_at,
                        (SELECT count(*) FROM {s}.code_commits),
                        (SELECT coalesce(array_agg(name ORDER BY name), '{{}}') FROM {s}.code_refs WHERE kind = 'branch'),
                        (SELECT count(*) FROM {s}.code_refs WHERE kind = 'tag')
                 FROM {s}.code_history h"
            ),
            &[],
        )
        .await
        .with_context(|| format!("cannot read the commit history in schema {schema}"))?;
    let Some(row) = row else {
        return Ok(Value::Null);
    };
    // The commit the code tools read without a version.
    let default = pghacker_codesearch::rev::resolve(client, schema, "postgres", None)
        .await
        .ok()
        .map(|d| json!({ "ref": d.git_ref, "sha": d.sha }));
    let paths: Vec<String> = row.get(2);
    Ok(json!({
        "repo": "postgres",
        "url": row.get::<_, Option<String>>(1),
        "paths": if paths.is_empty() { json!("the whole tree") } else { json!(paths) },
        "master": row.get::<_, String>(0),
        "head": row.get::<_, Option<String>>(3),
        "built_at": row.get::<_, chrono::DateTime<chrono::Utc>>(4),
        "synced_at": row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(5),
        "commits": row.get::<_, i64>(6),
        "branches": row.get::<_, Vec<String>>(7),
        "tags": row.get::<_, i64>(8),
        "default": default,
    }))
}

/// get_ontology, with `loaded.code` describing the commit history in
/// `history`, the schema the code tools read.
pub async fn describe(client: &impl GenericClient, history: &str) -> Result<Value> {
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
    let code = history_loaded(client, history).await?;
    let mut sql = vec![
        "SELECT a.display_name, count(*) FROM messages m JOIN authors a ON a.id = m.author_id GROUP BY 1 ORDER BY 2 DESC LIMIT 10",
        "SELECT f.slug, f.status, f.release, count(ft.thread_id) AS threads FROM features f LEFT JOIN feature_threads ft ON ft.feature_id = f.id WHERE f.merged_into IS NULL GROUP BY f.id ORDER BY threads DESC",
    ];
    sql.extend(HISTORY_SQL);
    Ok(json!({
        "graph": {
            "name": GRAPH,
            "vertices": vertices,
            "edges": edges,
            "keys": "Every vertex has a unique key property: author:<id>, thread:<id>, message:<Message-ID>, \
                     feature:<slug>, decision:<id>, position:<id>, release:<major>, subsystem:<name>, file:<path>, \
                     commit:<sha>, cf:<entry>. A commit's SHA is the full one when the commit history has the \
                     commit, else as a message wrote it.",
        },
        "tables": tables,
        "examples": {
            "cypher": [
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author)-[:HOLDS]->(p:Position)-[:ABOUT]->(f:Feature {slug: 'sql-pgq'}) RETURN a.name, p.stance, p.summary $$) AS (author agtype, stance agtype, summary agtype)",
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (t:Thread)-[:DISCUSSES]->(f:Feature) RETURN f.name, count(t) ORDER BY count(t) DESC LIMIT 10 $$) AS (feature agtype, threads agtype)",
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (m:Message)-[:MENTIONS]->(f:File)-[:PART_OF]->(s:Subsystem {name: 'parser'}) RETURN f.path, count(m) ORDER BY count(m) DESC LIMIT 10 $$) AS (path agtype, mentions agtype)",
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author)-[:COMMITTED]->(c:Commit)-[:MODIFIES]->(:File {path: 'src/backend/parser/parse_jsontable.c'}), (c)-[:DISCUSSED_IN]->(t:Thread) RETURN c.sha, c.committed_at, c.subject, a.name, t.subject ORDER BY c.committed_at DESC LIMIT 10 $$) AS (sha agtype, committed_at agtype, subject agtype, committer agtype, thread agtype)",
            ],
            "sql": sql,
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
            "code_note": HISTORY_NOTE,
        },
    }))
}
