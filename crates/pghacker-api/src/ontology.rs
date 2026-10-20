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
        "key, id, slug, name, status (what its most recent thread showed, though proposed or in_progress never replace \
         committed: proposed, \
         in_progress, committed, reverted, rejected, returned, withdrawn or unknown; returned is \
         returned with feedback in a CommitFest), release (the one major version it shipped in, or \
         targets until it's committed, by the name it shipped as: 9.0, not its development name 8.5; \
         never a branch it was back-ported to)",
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
    and tag. It comes from PostgreSQL's git repository (url): an operator builds it, the indexer \
    applies new commits hourly (synced_at), and it isn't listed in sources, which holds the \
    mailing lists. version names a major version (its stable branch), a release (18.3, its tag), a branch, \
    tag or commit SHA, a branch or tag as of a date (REL_17_STABLE@2024-06-01) or commits back \
    (master~3); without one, `default`. Paths below /branch/<name>/, /tag/<name>/ or /commit/<rev>/ \
    read those trees in the same call. blame, get_commit and search_commits answer as git blame, git \
    show and git log. In SQL, code_commits.addr places a commit: master's k-th commit is k * 2^32, \
    and a branch's commits follow the master commit it forks from. A file version (code_file_versions) \
    or line instance (code_line_instances) is in commit X's tree when lo <= X AND X < hi AND \
    (alive IS NULL OR alive @> X); a version's lines are its lineage's instances alive at its lo, in \
    weave order.";

/// Every readable table's description, from the schema's comments.
pub const TABLE_COMMENTS_SQL: &str = "SELECT c.relname AS table_name, obj_description(c.oid, 'pg_class') AS description FROM pg_class c WHERE c.relnamespace = 'pghacker'::regnamespace AND c.relkind = 'r' AND has_table_privilege(c.oid, 'SELECT') ORDER BY 1";

/// A table's columns, messages' here, with their types and descriptions.
pub const COLUMN_COMMENTS_SQL: &str = "SELECT a.attname AS column_name, format_type(a.atttypid, a.atttypmod) AS type, col_description(a.attrelid, a.attnum) AS description FROM pg_attribute a WHERE a.attrelid = 'pghacker.messages'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum";

/// Where the tables' and columns' descriptions are, which `tables` leaves
/// out, and how to read them.
fn tables_note() -> String {
    format!(
        "The schema's comments describe each table and column; `tables` leaves them out. \
         obj_description and col_description read them: every table's with `{TABLE_COMMENTS_SQL}`, \
         and a table's columns' (messages' here) with `{COLUMN_COMMENTS_SQL}`."
    )
}

/// The monthly archives that mail came from (YYYY-MM), in brief: the first
/// and the last, how many, and the months between them that none came from,
/// as runs such as "1997-07 to 1997-12".
fn month_coverage(months: &[String]) -> Value {
    let parse = |text: &str| -> Option<i32> {
        let (year, month) = text.split_once('-')?;
        let (year, month): (i32, i32) = (year.parse().ok()?, month.parse().ok()?);
        (1..=12).contains(&month).then_some(year * 12 + month - 1)
    };
    let name = |i: i32| format!("{:04}-{:02}", i.div_euclid(12), i.rem_euclid(12) + 1);
    let mut known: Vec<i32> = months.iter().filter_map(|m| parse(m)).collect();
    known.sort_unstable();
    known.dedup();
    let missing: Vec<String> = known
        .windows(2)
        .filter(|pair| pair[1] - pair[0] > 1)
        .map(|pair| match (pair[0] + 1, pair[1] - 1) {
            (first, last) if first == last => name(first),
            (first, last) => format!("{} to {}", name(first), name(last)),
        })
        .collect();
    json!({
        "first": known.first().map(|&i| name(i)),
        "last": known.last().map(|&i| name(i)),
        "count": months.len(),
        "missing": missing,
    })
}

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
    // format_type names arrays by their elements (text[]), as
    // information_schema doesn't.
    let tables: Vec<Value> = client
        .query(
            "SELECT c.relname::text,
                    string_agg(a.attname || ' ' || format_type(a.atttypid, a.atttypmod), ', ' ORDER BY a.attnum)
             FROM pg_class c
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
             WHERE c.relnamespace = 'pghacker'::regnamespace AND c.relkind IN ('r', 'p', 'v', 'f')
               AND a.attname NOT IN ('embedding', 'search_text')
               AND has_table_privilege(c.oid, 'SELECT')
               AND c.relname NOT IN ('schema_migrations', 'graph_label_counts')
             GROUP BY c.relname ORDER BY c.relname",
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
    // The mailing lists retrieved, as sources records them.
    let lists: Vec<Value> = client
        .query(
            "SELECT name, enabled, coalesce((config->>'follow')::boolean, false)
             FROM sources WHERE kind = 'mailing_list' ORDER BY name",
            &[],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "name": row.get::<_, String>(0), "enabled": row.get::<_, bool>(1),
                    "follow": row.get::<_, bool>(2) })
        })
        .collect();
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
        "tables_note": tables_note(),
        "examples": {
            "cypher": [
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author)-[:HOLDS]->(p:Position)-[:ABOUT]->(f:Feature {slug: 'sql-property-graph-queries'}) RETURN a.name, p.stance, p.summary $$) AS (author agtype, stance agtype, summary agtype)",
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
                "lists": lists,
                "messages": coverage.get::<_, i64>(0),
                "threads": coverage.get::<_, i64>(1),
                "authors": coverage.get::<_, i64>(2),
                "from": coverage.get::<_, Option<chrono::DateTime<chrono::Utc>>>(3),
                "to": coverage.get::<_, Option<chrono::DateTime<chrono::Utc>>>(4),
                "months": month_coverage(&coverage.get::<_, Option<Vec<String>>>(5).unwrap_or_default()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn months(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| (*m).to_owned()).collect()
    }

    #[test]
    fn months_are_a_range_with_the_runs_missing_from_it() {
        assert_eq!(
            month_coverage(&months(&["1997-06", "1998-01", "1998-02"])),
            json!({ "first": "1997-06", "last": "1998-02", "count": 3, "missing": ["1997-07 to 1997-12"] })
        );
        assert_eq!(
            month_coverage(&months(&["2025-11", "2026-01", "2026-02", "2026-06"])),
            json!({ "first": "2025-11", "last": "2026-06", "count": 4,
                    "missing": ["2025-12", "2026-03 to 2026-05"] })
        );
        assert_eq!(
            month_coverage(&months(&["2026-06", "2026-07"])),
            json!({ "first": "2026-06", "last": "2026-07", "count": 2, "missing": [] })
        );
        assert_eq!(
            month_coverage(&[]),
            json!({ "first": null, "last": null, "count": 0, "missing": [] })
        );
    }
}
