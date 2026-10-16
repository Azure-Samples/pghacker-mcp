// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The incremental graph sync against the full one: whatever the tables go
//! through, applying the changes they captured must leave exactly the graph
//! that a full sync computes, with AGE's rows matching the bookkeeping and the
//! kept label counts matching both.

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, extraction, graph};
use serde_json::{Value, json};
use tokio_postgres::Client;

const FIXTURE: &str = r#"
INSERT INTO authors (id, display_name, message_count, first_seen, last_seen) VALUES
    (1, 'Alice Hacker', 2, '2026-06-01', '2026-06-03'), (2, 'Bob Reviewer', 1, '2026-06-02', '2026-06-02'),
    (3, 'Bob R.', 1, '2026-06-04', '2026-06-04');
INSERT INTO author_names (author_id, name, count) VALUES (1, 'Alice Hacker', 2), (2, 'Bob Reviewer', 1), (3, 'Bob R.', 1);
SELECT setval('authors_id_seq', 100);
INSERT INTO threads (id, list, root_message_id, subject, message_count, participant_count, started_at, last_at) VALUES
    (10, 'pgsql-hackers', 'm1@example.org', 'JSON_TABLE plan clause', 3, 2, '2026-06-01', '2026-06-03'),
    (11, 'pgsql-hackers', 'm9@example.org', 'Release announcement', 1, 1, '2026-06-04', '2026-06-04');
SELECT setval('threads_id_seq', 100);
INSERT INTO messages (id, message_id, list, thread_id, parent_message_id, author_id, from_name, subject, sent_at, has_patch) VALUES
    (100, 'm1@example.org', 'pgsql-hackers', 10, NULL, 1, 'Alice Hacker', 'JSON_TABLE plan clause', '2026-06-01', true),
    (101, 'm2@example.org', 'pgsql-hackers', 10, 'm1@example.org', 2, 'Bob Reviewer', 'Re: JSON_TABLE plan clause', '2026-06-02', false),
    (102, 'm3@example.org', 'pgsql-hackers', 10, 'm2@example.org', 1, 'Alice Hacker', 'Re: JSON_TABLE plan clause', '2026-06-03', false),
    (103, 'm9@example.org', 'pgsql-hackers', 11, NULL, 3, 'Bob R.', 'Release announcement', '2026-06-04', false);
SELECT setval('messages_id_seq', 1000);
INSERT INTO anchors (message_id, kind, value) VALUES
    (100, 'file', 'src/backend/parser/parse_jsontable.c'), (100, 'release', '19'), (101, 'commitfest', '5123'),
    (102, 'commit', 'abc1234'), (103, 'release', '19'), (103, 'file', 'doc/src/sgml/release-19.sgml');
INSERT INTO features (id, slug, name, description, status, release) VALUES
    (1, 'json-table-plan', 'JSON_TABLE PLAN clause', 'The PLAN clause.', 'proposed', '19'),
    (2, 'sql-pgq', 'SQL/PGQ', 'Property graph queries.', 'in_progress', NULL);
SELECT setval('features_id_seq', 100);
INSERT INTO feature_threads (feature_id, thread_id) VALUES (1, 10), (2, 10);
INSERT INTO feature_files (feature_id, path, thread_id) VALUES
    (1, 'src/backend/parser/parse_jsontable.c', 10), (2, 'src/backend/catalog/pg_propgraph.c', 10);
INSERT INTO decisions (id, feature_id, thread_id, title, outcome, rationale) VALUES
    (1, 1, 10, 'Simplify nested paths first', 'Agreed', 'Complexity.');
SELECT setval('decisions_id_seq', 100);
INSERT INTO positions (id, author_id, thread_id, feature_id, decision_id, stance, summary) VALUES
    (1, 1, 10, 1, NULL, 'proposes', 'Alice proposes PLAN.'), (2, 2, 10, 1, 1, 'concerned', 'Bob worries about complexity.');
SELECT setval('positions_id_seq', 100);
INSERT INTO citations (extract_kind, extract_id, message_id, quote) VALUES
    ('feature', 1, 100, 'adding the PLAN clause'), ('feature', 1, 100, 'PLAN clause to JSON_TABLE'),
    ('decision', 1, 102, 'simplify the nested path'), ('position', 1, 100, 'Here is a patch'),
    ('position', 2, 101, 'not convinced');
INSERT INTO code_file_versions (id, lineage, path, blob_sha, lo, hi, size, line_count, is_binary, final_newline, bom, subsystem) VALUES
    (1, 1, 'src/backend/parser/parse_jsontable.c', 'b1', 0, 9223372036854775807, 10, 1, false, true, false, 'parser'),
    (2, 2, 'src/backend/parser/gram.y', 'b2', 0, 9223372036854775807, 10, 1, false, true, false, 'parser'),
    (3, 3, 'src/backend/executor/execMain.c', 'b3', 0, 9223372036854775807, 10, 1, false, true, false, 'executor'),
    (4, 4, 'README', 'b4', 0, 9223372036854775807, 10, 1, false, true, false, NULL);
"#;

/// An insert of the history's file versions, whose rows [`version`] gives.
const VERSION_COLUMNS: &str = "INSERT INTO code_file_versions
    (id, lineage, path, blob_sha, lo, hi, size, line_count, is_binary, final_newline, bom, subsystem) VALUES";

/// A file version of the history at `path`, alive from a commit on.
fn version(id: u64, path: &str, subsystem: &str) -> String {
    format!(
        "({id}, {id}, '{path}', 'v{id}', 4294967296, 9223372036854775807, 1, 1, false, true, false, {subsystem})"
    )
}

/// The newest of the history's file versions, -1 for none: versions written
/// after it have greater IDs.
async fn newest_version(client: &Client) -> i32 {
    client
        .query_one("SELECT coalesce(max(id), -1) FROM code_file_versions", &[])
        .await
        .unwrap()
        .get(0)
}

/// Records the graph entries of the file versions after `after`, as the
/// history's sync records those of the files its commits change
/// (`graph_commit_entries`): the history's tables have no triggers.
async fn record_versions(client: &Client, after: i32) -> u64 {
    client
        .execute(
            "INSERT INTO graph_changes (label, key, end_key)
             SELECT DISTINCT e.*
             FROM (SELECT DISTINCT path, subsystem FROM code_file_versions WHERE id > $1) v
             CROSS JOIN LATERAL graph_code_file_entries(v.path, v.subsystem) e",
            &[&after],
        )
        .await
        .unwrap()
}

/// Loads the fixture, recording its versions' graph entries as a sync would.
async fn load(client: &Client) {
    client.batch_execute(FIXTURE).await.unwrap();
    assert!(record_versions(client, -1).await > 0);
}

/// A fresh database with the fixture, and the graph built by a full sync.
async fn synced() -> (TestDb, Client) {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    load(&client).await;
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_some(), "the migration asks for a full sync");
    (db, client)
}

/// Applies the waiting changes, `limit` at a time; returns the vertices and
/// edges they changed.
async fn drain(client: &mut Client, limit: i64) -> u64 {
    let mut changed = 0;
    loop {
        let stats = graph::sync_changes(client, limit).await.unwrap();
        if stats.changes == 0 {
            return changed;
        }
        changed += stats.changed();
    }
}

/// Applies the waiting changes and checks that the graph is what a full sync
/// computes.
async fn converged(client: &mut Client) {
    drain(client, graph::BATCH).await;
    assert!(!graph::pending(&*client).await.unwrap());
    let drift = graph::check(client).await.unwrap();
    assert!(drift.is_empty(), "{drift:#?}");
}

async fn changes(client: &Client) -> i64 {
    client
        .query_one("SELECT count(*) FROM graph_changes", &[])
        .await
        .unwrap()
        .get(0)
}

async fn has_edge(client: &Client, label: &str, start: &str, end: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT FROM graph_edges WHERE label = $1 AND start_key = $2 AND end_key = $3)",
            &[&label, &start, &end],
        )
        .await
        .unwrap()
        .get(0)
}

async fn has_vertex(client: &Client, key: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT FROM graph_nodes WHERE key = $1)",
            &[&key],
        )
        .await
        .unwrap()
        .get(0)
}

async fn props(client: &Client, key: &str) -> Value {
    client
        .query_one("SELECT props FROM graph_nodes WHERE key = $1", &[&key])
        .await
        .unwrap()
        .get(0)
}

async fn indexer(db: &TestDb) -> Indexer {
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let env = Env::from_pairs([("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str())]);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    Indexer::new(settings, &env).unwrap()
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn changes_alone_build_the_graph() {
    // An empty graph and every row captured: applying the changes builds it.
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    load(&client).await;
    assert!(changes(&client).await > 0);
    assert!(drain(&mut client, 7).await > 0);
    let drift = graph::check(&mut client).await.unwrap();
    assert!(drift.is_empty(), "{drift:#?}");
    assert!(
        has_edge(
            &client,
            "REPLIES_TO",
            "message:m3@example.org",
            "message:m2@example.org"
        )
        .await
    );
    assert!(
        has_edge(
            &client,
            "EVIDENCED_BY",
            "feature:json-table-plan",
            "message:m1@example.org"
        )
        .await
    );
    assert!(
        has_edge(
            &client,
            "PART_OF",
            "file:src/backend/parser/gram.y",
            "subsystem:parser"
        )
        .await
    );
}

#[tokio::test]
async fn the_job_syncs_fully_once_then_in_batches() {
    let (db, mut client) = synced().await;
    assert_eq!(
        changes(&client).await,
        0,
        "the full sync covered the fixture's changes"
    );
    let counts: i64 = client
        .query_one("SELECT count(*) FROM graph_label_counts", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        counts as usize,
        pghacker_core::VERTEX_LABELS.len() + pghacker_core::EDGE_LABELS.len()
    );
    client
        .batch_execute(
            "INSERT INTO messages (message_id, list, thread_id, parent_message_id, author_id, subject, sent_at)
             VALUES ('m4@example.org', 'pgsql-hackers', 10, 'm3@example.org', 2, 'Re: JSON_TABLE plan clause', '2026-06-05');
             UPDATE threads SET message_count = 4, last_at = '2026-06-05' WHERE id = 10;",
        )
        .await
        .unwrap();
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_none());
    assert_eq!(
        (report.batches, report.nodes_added, report.nodes_updated),
        (1, 1, 1)
    );
    assert_eq!(report.edges_added, 3, "AUTHORED, REPLIES_TO and IN_THREAD");
    assert!(!report.more);
    assert!(
        has_edge(
            &client,
            "REPLIES_TO",
            "message:m4@example.org",
            "message:m3@example.org"
        )
        .await
    );
    converged(&mut client).await;

    // A reconcile runs only once it's due and no change waits.
    let report = graph::run(
        &mut client,
        std::time::Duration::from_secs(10),
        Some(std::time::Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    assert!(report.full.is_none());
    client
        .execute(
            "UPDATE graph_sync_state SET last_full_at = now() - interval '2 hours'",
            &[],
        )
        .await
        .unwrap();
    let report = graph::run(
        &mut client,
        std::time::Duration::from_secs(10),
        Some(std::time::Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    assert_eq!(report.full.map(|stats| stats.changed()), Some(0));

    // ingest_status reads the kept counts and shows what's waiting.
    let db_indexer = indexer(&db).await;
    client
        .batch_execute("UPDATE threads SET subject = 'Waiting' WHERE id = 11")
        .await
        .unwrap();
    let status = pghacker_indexer::status::ingest_status(&db_indexer)
        .await
        .unwrap();
    let graph = &status["graph"];
    assert_eq!(graph["nodes"]["Message"], 5);
    assert_eq!(graph["edges"]["MENTIONS"], 6);
    assert_eq!(graph["pending_changes"], 1);
    assert_eq!(graph["full_sync"]["requested"], false);
    assert_eq!(graph["last_sync"]["stats"]["mode"], "full");
}

#[tokio::test]
async fn replies_wait_for_their_parent() {
    let (_db, mut client) = synced().await;
    client
        .batch_execute(
            "INSERT INTO messages (message_id, list, thread_id, parent_message_id, author_id, subject, sent_at)
             VALUES ('child@example.org', 'pgsql-hackers', 11, 'parent@example.org', 1, 'Re: Release', '2026-06-06')",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(
        !has_edge(
            &client,
            "REPLIES_TO",
            "message:child@example.org",
            "message:parent@example.org"
        )
        .await
    );
    client
        .batch_execute(
            "INSERT INTO messages (message_id, list, thread_id, author_id, subject, sent_at)
             VALUES ('parent@example.org', 'pgsql-hackers', 11, 3, 'Release', '2026-06-05')",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(
        has_edge(
            &client,
            "REPLIES_TO",
            "message:child@example.org",
            "message:parent@example.org"
        )
        .await
    );
    // And go with it.
    client
        .batch_execute("DELETE FROM messages WHERE message_id = 'parent@example.org'")
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(!has_vertex(&client, "message:parent@example.org").await);
    assert!(
        !has_edge(
            &client,
            "REPLIES_TO",
            "message:child@example.org",
            "message:parent@example.org"
        )
        .await
    );
}

#[tokio::test]
async fn merges_and_their_undoing_follow() {
    let (db, mut client) = synced().await;
    let indexer = indexer(&db).await;
    let merged = extraction::merge_authors(
        &indexer,
        args(json!({ "from_id": 3, "into_id": 2, "reason": "Same person" })),
    )
    .await
    .unwrap();
    converged(&mut client).await;
    assert!(!has_vertex(&client, "author:3").await);
    assert!(has_edge(&client, "AUTHORED", "author:2", "message:m9@example.org").await);
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(has_edge(&client, "AUTHORED", "author:3", "message:m9@example.org").await);

    let merged = extraction::merge_extracts(
        &indexer,
        args(json!({ "kind": "feature", "from_id": 2, "into_id": 1, "reason": "Same feature" })),
    )
    .await
    .unwrap();
    converged(&mut client).await;
    assert!(!has_vertex(&client, "feature:sql-pgq").await);
    assert!(
        has_edge(
            &client,
            "IMPLEMENTS",
            "file:src/backend/catalog/pg_propgraph.c",
            "feature:json-table-plan"
        )
        .await
    );
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    converged(&mut client).await;
    // The feature's own threads and files come back with it.
    assert!(has_edge(&client, "DISCUSSES", "thread:10", "feature:sql-pgq").await);
    assert!(
        has_edge(
            &client,
            "IMPLEMENTS",
            "file:src/backend/catalog/pg_propgraph.c",
            "feature:sql-pgq"
        )
        .await
    );
}

#[tokio::test]
async fn history_files_follow_syncs_and_installs() {
    let (_db, mut client) = synced().await;
    assert_eq!(
        props(&client, "file:doc/src/sgml/release-19.sgml").await["loaded"],
        false
    );
    // A sync adds versions, a new file's and an existing file's, which no
    // trigger captures: it records their entries itself.
    let newest = newest_version(&client).await;
    assert_eq!(newest, 4);
    client
        .batch_execute(&format!(
            "{VERSION_COLUMNS} {}, {};
             UPDATE code_file_versions SET hi = 4294967296 WHERE id = 2;",
            version(5, "doc/src/sgml/release-19.sgml", "'doc'"),
            version(6, "src/backend/parser/gram.y", "'parser'"),
        ))
        .await
        .unwrap();
    assert_eq!(changes(&client).await, 0, "the history has no triggers");
    assert!(record_versions(&client, newest).await > 0);
    converged(&mut client).await;
    assert_eq!(
        props(&client, "file:doc/src/sgml/release-19.sgml").await,
        json!({ "key": "file:doc/src/sgml/release-19.sgml", "path": "doc/src/sgml/release-19.sgml",
                "subsystem": "doc", "loaded": true })
    );
    assert!(
        has_edge(
            &client,
            "PART_OF",
            "file:doc/src/sgml/release-19.sgml",
            "subsystem:doc"
        )
        .await
    );
    // An install swaps in a history without some files, and asks for a full
    // sync: the files that another source names stay.
    client
        .batch_execute(
            "DELETE FROM code_file_versions
             WHERE path IN ('README', 'src/backend/executor/execMain.c', 'src/backend/parser/parse_jsontable.c')",
        )
        .await
        .unwrap();
    graph::request_full(&client, "the commit history was installed")
        .await
        .unwrap();
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_some());
    converged(&mut client).await;
    assert!(!has_vertex(&client, "file:README").await);
    assert!(!has_vertex(&client, "subsystem:executor").await);
    assert!(
        has_edge(
            &client,
            "PART_OF",
            "file:src/backend/parser/gram.y",
            "subsystem:parser"
        )
        .await
    );
    assert_eq!(
        props(&client, "file:src/backend/parser/parse_jsontable.c").await["loaded"],
        false,
        "an anchor and a feature still name it"
    );
    // Entries recorded for files that leave apply incrementally too.
    client
        .batch_execute(
            "INSERT INTO graph_changes (label, key, end_key)
             SELECT DISTINCT e.* FROM code_file_versions v
             CROSS JOIN LATERAL graph_code_file_entries(v.path, v.subsystem) e WHERE v.subsystem = 'parser';
             DELETE FROM code_file_versions WHERE subsystem = 'parser';",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(!has_vertex(&client, "subsystem:parser").await);
    assert!(!has_vertex(&client, "file:src/backend/parser/gram.y").await);
    assert!(has_vertex(&client, "subsystem:doc").await);
}

#[tokio::test]
async fn updates_that_the_graph_ignores_capture_nothing() {
    let (_db, client) = synced().await;
    client
        .batch_execute(
            "UPDATE anchors SET resolved = '/branch/REL_19_STABLE/' || value WHERE kind = 'file';
             UPDATE threads SET extraction_state = 'extracted', extracted_version = 1, updated_at = now();
             UPDATE features SET description = 'Rewritten.', updated_at = now();
             UPDATE decisions SET rationale = 'Rewritten.';
             UPDATE messages SET body = 'Rewritten.', body_clean = 'Rewritten.';
             UPDATE authors SET updated_at = now();
             UPDATE citations SET quote = quote || '.';
             UPDATE features SET status = status;",
        )
        .await
        .unwrap();
    assert_eq!(changes(&client).await, 0);
}

#[tokio::test]
async fn a_truncate_asks_for_a_full_sync() {
    let (_db, mut client) = synced().await;
    client.batch_execute("TRUNCATE citations").await.unwrap();
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    let full = report.full.expect("a full sync");
    assert_eq!(full.edges_removed, 4, "the EVIDENCED_BY edges");
    converged(&mut client).await;
}

#[tokio::test]
async fn an_older_projection_needs_a_full_sync() {
    let (_db, mut client) = synced().await;
    let projection = "SELECT projection FROM graph_sync_state";
    let recorded: i32 = client.query_one(projection, &[]).await.unwrap().get(0);
    assert_eq!(recorded, graph::PROJECTION);
    // As if an indexer with the previous projection had answered the
    // requests, as while an old revision runs the jobs beside a new one.
    client
        .batch_execute("UPDATE graph_sync_state SET projection = projection - 1")
        .await
        .unwrap();
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert_eq!(report.full.map(|stats| stats.changed()), Some(0));
    let recorded: i32 = client.query_one(projection, &[]).await.unwrap().get(0);
    assert_eq!(recorded, graph::PROJECTION);
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_none());
}

#[tokio::test]
async fn a_label_without_a_count_asks_for_a_full_sync() {
    // As an older image's full sync leaves a label it doesn't know.
    let (_db, mut client) = synced().await;
    client
        .batch_execute(
            "DELETE FROM graph_label_counts WHERE label = 'AUTHORED';
             INSERT INTO messages (message_id, list, thread_id, author_id, subject, sent_at)
             VALUES ('m5@example.org', 'pgsql-hackers', 10, 1, 'Re: JSON_TABLE plan clause', '2026-06-06');",
        )
        .await
        .unwrap();
    // A batch that changes the label can't count it: it asks for a full sync.
    graph::sync_changes(&mut client, graph::BATCH)
        .await
        .unwrap();
    let requested: bool = client
        .query_one(
            "SELECT full_requested > full_done FROM graph_sync_state",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(requested);
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_some());
    converged(&mut client).await;
    // And the sync job runs one itself while a label has no count.
    client
        .batch_execute("DELETE FROM graph_label_counts WHERE label = 'HOLDS'")
        .await
        .unwrap();
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert_eq!(report.full.map(|stats| stats.changed()), Some(0));
    converged(&mut client).await;
}

#[tokio::test]
async fn the_counts_follow_the_graph() {
    let (_db, mut client) = synced().await;
    client
        .batch_execute(
            "DELETE FROM citations WHERE extract_kind = 'position';
             DELETE FROM positions;
             UPDATE features SET merged_into = 1 WHERE id = 2;",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    let count = |label: &'static str| {
        let client = &client;
        async move {
            client
                .query_one(
                    "SELECT count FROM graph_label_counts WHERE label = $1",
                    &[&label],
                )
                .await
                .unwrap()
                .get::<_, i64>(0)
        }
    };
    assert_eq!(count("Position").await, 0);
    assert_eq!(count("HOLDS").await, 0);
    assert_eq!(count("Feature").await, 1);
}

#[tokio::test]
async fn the_check_finds_what_differs() {
    let (_db, mut client) = synced().await;
    assert!(graph::check(&mut client).await.unwrap().is_empty());
    client
        .batch_execute(
            "DELETE FROM pghacker_graph.\"HOLDS\" WHERE id = (SELECT graph_id FROM graph_edges WHERE label = 'HOLDS' LIMIT 1);
             UPDATE pghacker_graph.\"Thread\" SET properties = '{\"key\": \"thread:10\"}'
             WHERE id = (SELECT graph_id FROM graph_nodes WHERE key = 'thread:10');
             DELETE FROM graph_edges WHERE label = 'IN_THREAD' AND start_key = 'message:m9@example.org';
             UPDATE graph_label_counts SET count = count + 1 WHERE label = 'Author';
             UPDATE authors SET display_name = 'Renamed without capture' WHERE id = 1;
             DELETE FROM graph_changes;",
        )
        .await
        .unwrap();
    let drift = graph::check(&mut client).await.unwrap();
    assert_eq!(drift.changed_nodes, ["author:1"]);
    assert_eq!(
        drift.missing_edges,
        ["IN_THREAD message:m9@example.org -> thread:11"]
    );
    assert_eq!(drift.counts, ["Author: 4, 3", "IN_THREAD: 4, 3"]);
    assert!(
        drift
            .age
            .iter()
            .any(|row| row.starts_with("vertex thread:10")),
        "{drift:#?}"
    );
    assert!(
        drift.age.iter().any(|row| row.starts_with("edge HOLDS ")),
        "{drift:#?}"
    );
    assert!(
        drift
            .age
            .iter()
            .any(|row| row.starts_with("IN_THREAD edge ")),
        "{drift:#?}"
    );
    // A full sync repairs what it can compute; its bookkeeping errors need a rebuild.
    let stats = graph::sync(&mut client).await.unwrap();
    assert_eq!((stats.nodes_updated, stats.edges_added), (1, 1));
}

#[tokio::test]
async fn a_restore_rebuilds_the_graph() {
    // As scripts/corpus-restore.mjs does: empty the corpus, the bookkeeping
    // and the graph; load the rows; then drop the changes the load captured.
    let (_db, mut client) = synced().await;
    client
        .batch_execute(
            "TRUNCATE graph_nodes, graph_edges, graph_sync_runs, graph_changes, graph_sync_state, graph_label_counts,
                      graph_committers,
                      authors, threads, messages, anchors, features, decisions, positions, citations, feature_threads,
                      feature_files, code_file_versions, author_names CASCADE;
             DELETE FROM pghacker_graph._ag_label_edge;
             DELETE FROM pghacker_graph._ag_label_vertex;",
        )
        .await
        .unwrap();
    let requested: bool = client
        .query_one(
            "SELECT full_requested > full_done FROM graph_sync_state",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(requested, "a TRUNCATE asks for a full sync");
    load(&client).await;
    client
        .batch_execute("TRUNCATE graph_changes; SELECT graph_request_full('corpus restore');")
        .await
        .unwrap();
    let report = graph::run(&mut client, std::time::Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.unwrap().nodes_added > 0);
    converged(&mut client).await;
}

/// xorshift64*: the same changes for the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn of<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const FILES: &[&str] = &[
    "src/backend/parser/parse_jsontable.c",
    "src/backend/parser/gram.y",
    "src/backend/executor/execMain.c",
    "src/include/nodes/parsenodes.h",
    "README",
    "src/backend/catalog/pg_propgraph.c",
];
const SUBSYSTEMS: &[&str] = &["'parser'", "'executor'", "'include'", "'catalog'", "NULL"];
const RELEASES: &[&str] = &["'17'", "'18'", "'19'", "NULL"];
const STATUSES: &[&str] = &["proposed", "in_progress", "committed", "rejected"];
const STANCES: &[&str] = &["proposes", "supports", "opposes", "concerned", "reviews"];

fn anchor(rng: &mut Rng) -> (&'static str, &'static str) {
    match rng.below(4) {
        0 => ("file", rng.of(FILES)),
        1 => ("commit", rng.of(&["abc1234", "def5678", "0123abcd"])),
        2 => ("commitfest", rng.of(&["5123", "4999"])),
        _ => ("release", rng.of(&["17", "18", "19"])),
    }
}

/// One of the IDs that `sql` lists.
async fn pick(client: &Client, rng: &mut Rng, sql: &str) -> Option<i64> {
    let rows = client.query(sql, &[]).await.unwrap();
    (!rows.is_empty()).then(|| rows[rng.below(rows.len() as u64) as usize].get(0))
}

fn or_null(id: Option<i64>) -> String {
    id.map_or_else(|| "NULL".to_owned(), |id| id.to_string())
}

/// One random change to the tables, in one transaction, as the indexer's
/// writers or a person with SQL might make it; or a sync, now and then.
async fn change(client: &mut Client, rng: &mut Rng, n: u64) {
    let thread = pick(client, rng, "SELECT id FROM threads ORDER BY id").await;
    let message = pick(client, rng, "SELECT id FROM messages ORDER BY id").await;
    let author = pick(
        client,
        rng,
        "SELECT id FROM authors WHERE merged_into IS NULL ORDER BY id",
    )
    .await;
    let feature = pick(client, rng, "SELECT id FROM features ORDER BY id").await;
    let decision = pick(client, rng, "SELECT id FROM decisions ORDER BY id").await;
    let position = pick(client, rng, "SELECT id FROM positions ORDER BY id").await;
    let other_feature = pick(
        client,
        rng,
        "SELECT id FROM features WHERE merged_into IS NULL ORDER BY id",
    )
    .await;
    let row = rng.below(6);
    let sql = match rng.below(24) {
        0..=4 => {
            let Some(thread) = thread else { return };
            let author = if rng.chance(90) { author } else { None };
            let parent = match rng.below(4) {
                0 => "NULL".to_owned(),
                1 => format!("'r{}@x'", n + 1 + rng.below(4)),
                _ => match message {
                    Some(id) => format!("(SELECT message_id FROM messages WHERE id = {id})"),
                    None => "NULL".to_owned(),
                },
            };
            let mut sql = format!(
                "INSERT INTO messages (message_id, list, thread_id, parent_message_id, author_id, subject, sent_at, has_patch)
                 VALUES ('r{n}@x', 'pgsql-hackers', {thread}, {parent}, {}, 'Subject {n}',
                         '2026-07-01'::timestamptz + interval '{n} minutes', {});
                 UPDATE threads SET message_count = message_count + 1,
                     last_at = greatest(last_at, '2026-07-01'::timestamptz + interval '{n} minutes') WHERE id = {thread};
                 UPDATE authors SET message_count = message_count + 1 WHERE id = {};",
                or_null(author),
                rng.chance(30),
                or_null(author)
            );
            for _ in 0..rng.below(4) {
                let (kind, value) = anchor(rng);
                sql.push_str(&format!(
                    "INSERT INTO anchors (message_id, kind, value) SELECT id, '{kind}', '{value}'
                     FROM messages WHERE message_id = 'r{n}@x' ON CONFLICT DO NOTHING;"
                ));
            }
            sql
        }
        5 => format!(
            "DELETE FROM messages m WHERE m.id = {} AND NOT EXISTS (SELECT FROM citations c WHERE c.message_id = m.id)",
            or_null(message)
        ),
        6 => match rng.below(3) {
            0 => format!("UPDATE messages SET thread_id = {} WHERE id = {}", or_null(thread), or_null(message)),
            1 => format!("UPDATE messages SET author_id = {} WHERE id = {}", or_null(author), or_null(message)),
            _ => format!(
                "UPDATE messages SET subject = 'Changed {n}', has_patch = NOT has_patch WHERE id = {}",
                or_null(message)
            ),
        },
        7 => match rng.below(5) {
            0 => format!("INSERT INTO authors (display_name) VALUES ('Author {n}')"),
            1 => format!(
                "UPDATE authors SET display_name = 'Name {n}', last_seen = '2026-08-01' WHERE id = {}",
                or_null(author)
            ),
            2 => {
                let other = pick(client, rng, "SELECT id FROM authors WHERE merged_into IS NULL ORDER BY id").await;
                format!(
                    "UPDATE authors SET merged_into = {} WHERE id = {} AND id <> {}",
                    or_null(other),
                    or_null(author),
                    or_null(other)
                )
            }
            3 => format!(
                "UPDATE authors SET merged_into = NULL WHERE id = (SELECT id FROM authors WHERE merged_into IS NOT NULL ORDER BY id OFFSET {row} LIMIT 1)"
            ),
            _ => format!("DELETE FROM authors WHERE id = {}", or_null(author)),
        },
        8 => match rng.below(4) {
            0 => format!(
                "INSERT INTO threads (list, root_message_id, subject) VALUES ('pgsql-hackers', 'root{n}@x', 'Thread {n}')"
            ),
            1 => format!(
                "UPDATE threads SET subject = 'Thread {n}', root_message_id = 'root{n}@x', message_count = message_count + 1
                 WHERE id = {}",
                or_null(thread)
            ),
            2 => format!("DELETE FROM threads WHERE id = {}", or_null(thread)),
            // As merge_thread does.
            _ => {
                let into = pick(client, rng, "SELECT id FROM threads ORDER BY id").await;
                let (from, into) = (or_null(thread), or_null(into));
                format!(
                    "UPDATE messages SET thread_id = {into} WHERE thread_id = {from} AND {from} <> {into};
                     UPDATE message_chunks SET thread_id = {into} WHERE thread_id = {from} AND {from} <> {into};
                     DELETE FROM threads WHERE id = {from} AND {from} <> {into};"
                )
            }
        },
        9 => format!(
            "INSERT INTO features (slug, name, description, status, release) VALUES ('f{n}', 'Feature {n}', 'D.', '{}', {})",
            rng.of(STATUSES),
            rng.of(RELEASES)
        ),
        10 => match rng.below(5) {
            0 => format!("UPDATE features SET status = '{}' WHERE id = {}", rng.of(STATUSES), or_null(feature)),
            1 => format!("UPDATE features SET release = {} WHERE id = {}", rng.of(RELEASES), or_null(feature)),
            2 => format!("UPDATE features SET slug = 'f{n}s', name = 'Renamed {n}' WHERE id = {}", or_null(feature)),
            3 => format!(
                "UPDATE features SET merged_into = {} WHERE id = {} AND id <> {}",
                or_null(other_feature),
                or_null(feature),
                or_null(other_feature)
            ),
            _ => format!("UPDATE features SET merged_into = NULL WHERE id = {}", or_null(feature)),
        },
        11 => {
            if rng.chance(60) {
                format!(
                    "INSERT INTO feature_threads VALUES ({}, {}) ON CONFLICT DO NOTHING",
                    or_null(feature),
                    or_null(thread)
                )
            } else {
                format!(
                    "DELETE FROM feature_threads WHERE ctid IN
                     (SELECT ctid FROM feature_threads ORDER BY feature_id, thread_id OFFSET {row} LIMIT 1)"
                )
            }
        }
        12 => {
            if rng.chance(60) {
                format!(
                    "INSERT INTO feature_files (feature_id, path) VALUES ({}, '{}') ON CONFLICT DO NOTHING",
                    or_null(feature),
                    rng.of(FILES)
                )
            } else {
                format!(
                    "DELETE FROM feature_files WHERE ctid IN
                     (SELECT ctid FROM feature_files ORDER BY feature_id, path OFFSET {row} LIMIT 1)"
                )
            }
        }
        13 => match rng.below(4) {
            0 => format!(
                "INSERT INTO decisions (feature_id, thread_id, title, outcome, rationale) VALUES ({}, {}, 'D{n}', 'O.', 'R.')",
                or_null(if rng.chance(70) { feature } else { None }),
                or_null(thread)
            ),
            1 => format!(
                "UPDATE decisions SET feature_id = {}, title = 'D{n}' WHERE id = {}",
                or_null(feature),
                or_null(decision)
            ),
            2 => {
                let other = pick(client, rng, "SELECT id FROM decisions WHERE merged_into IS NULL ORDER BY id").await;
                format!(
                    "UPDATE decisions SET merged_into = {} WHERE id = {} AND id <> {}",
                    or_null(other),
                    or_null(decision),
                    or_null(other)
                )
            }
            _ => format!("UPDATE decisions SET merged_into = NULL WHERE id = {}", or_null(decision)),
        },
        14 => match rng.below(3) {
            0 => format!(
                "INSERT INTO positions (author_id, thread_id, feature_id, decision_id, stance, summary)
                 VALUES ({}, {}, {}, {}, '{}', 'P{n}.')",
                or_null(author),
                or_null(thread),
                or_null(if rng.chance(70) { feature } else { None }),
                or_null(if rng.chance(50) { decision } else { None }),
                rng.of(STANCES)
            ),
            1 => format!(
                "UPDATE positions SET stance = '{}', feature_id = {}, decision_id = {}, author_id = coalesce({}, author_id)
                 WHERE id = {}",
                rng.of(STANCES),
                or_null(if rng.chance(60) { feature } else { None }),
                or_null(if rng.chance(60) { decision } else { None }),
                or_null(author),
                or_null(position)
            ),
            _ => format!(
                "DELETE FROM citations WHERE extract_kind = 'position' AND extract_id = {0};
                 DELETE FROM positions WHERE id = {0};",
                or_null(position)
            ),
        },
        15 => {
            let (kind, extract) = match rng.below(3) {
                0 => ("feature", feature),
                1 => ("decision", decision),
                _ => ("position", position),
            };
            match rng.below(3) {
                0 => format!(
                    "INSERT INTO citations (extract_kind, extract_id, message_id, quote) VALUES ('{kind}', {}, {}, 'Q{n}')",
                    or_null(extract),
                    or_null(message)
                ),
                1 => format!(
                    "DELETE FROM citations WHERE ctid IN (SELECT ctid FROM citations ORDER BY id OFFSET {row} LIMIT 1)"
                ),
                _ => format!(
                    "UPDATE citations SET extract_id = {}, message_id = coalesce({}, message_id)
                     WHERE ctid IN (SELECT ctid FROM citations WHERE extract_kind = '{kind}' ORDER BY id OFFSET {row} LIMIT 1)",
                    or_null(extract),
                    or_null(message)
                ),
            }
        }
        16 => {
            let (kind, value) = anchor(rng);
            match rng.below(4) {
                0 => format!(
                    "INSERT INTO anchors (message_id, kind, value) VALUES ({}, '{kind}', '{value}') ON CONFLICT DO NOTHING",
                    or_null(message)
                ),
                1 => format!("DELETE FROM anchors WHERE ctid IN (SELECT ctid FROM anchors ORDER BY id OFFSET {row} LIMIT 1)"),
                // A commit citation resolved to another's SHA, as
                // resolve_anchors does with a history installed, or unresolved.
                2 => format!(
                    "UPDATE anchors SET resolved = CASE WHEN resolved IS NULL THEN '{}' END
                     WHERE ctid IN (SELECT ctid FROM anchors WHERE kind = 'commit' ORDER BY id OFFSET {row} LIMIT 1)",
                    rng.of(&["abc1234", "def5678", "0123abcd"])
                ),
                _ => format!(
                    "UPDATE anchors SET kind = '{kind}', value = '{value}'
                     WHERE ctid IN (SELECT ctid FROM anchors ORDER BY id OFFSET {row} LIMIT 1)"
                ),
            }
        }
        17 => {
            // As a sync does: new versions, whose graph entries it records.
            let newest = newest_version(client).await;
            let paths: Vec<&str> = FILES.iter().copied().filter(|_| rng.chance(50)).collect();
            if paths.is_empty() {
                return;
            }
            let mut versions = Vec::new();
            for (k, path) in paths.into_iter().enumerate() {
                versions.push(version(1000 + n * 10 + k as u64, path, rng.of(SUBSYSTEMS)));
            }
            let sql = format!("{VERSION_COLUMNS} {}", versions.join(", "));
            client.batch_execute(&sql).await.unwrap();
            record_versions(client, newest).await;
            return;
        }
        18 => {
            // Versions that leave the history or change, as an install might
            // make them, with the graph entries of their old and new rows.
            let changed = if rng.chance(50) {
                format!(
                    "DELETE FROM code_file_versions v
                     WHERE v.id = (SELECT id FROM code_file_versions ORDER BY id OFFSET {row} LIMIT 1)
                     RETURNING v.path, v.subsystem"
                )
            } else {
                format!(
                    "UPDATE code_file_versions v SET subsystem = {}
                     WHERE v.id = (SELECT id FROM code_file_versions ORDER BY id OFFSET {row} LIMIT 1)
                     RETURNING v.path, v.subsystem",
                    rng.of(SUBSYSTEMS)
                )
            };
            format!(
                "WITH old AS (SELECT path, subsystem FROM code_file_versions ORDER BY id OFFSET {row} LIMIT 1),
                      new AS ({changed})
                 INSERT INTO graph_changes (label, key, end_key)
                 SELECT DISTINCT e.* FROM (SELECT * FROM old UNION ALL SELECT * FROM new) r
                 CROSS JOIN LATERAL graph_code_file_entries(r.path, r.subsystem) e"
            )
        }
        19..=21 => {
            // Part of what's waiting, in small batches.
            for _ in 0..=rng.below(3) {
                graph::sync_changes(client, 1 + rng.below(5) as i64).await.unwrap();
            }
            return;
        }
        22 => {
            if rng.chance(25) {
                graph::sync(client).await.unwrap();
            }
            return;
        }
        _ => {
            // Changes the graph ignores.
            format!(
                "UPDATE threads SET extraction_state = 'extracted' WHERE id = {};
                 UPDATE features SET description = 'Changed {n}.' WHERE id = {};
                 UPDATE anchors SET resolved = 'x{n}'
                 WHERE ctid IN (SELECT ctid FROM anchors WHERE kind <> 'commit' ORDER BY id OFFSET {row} LIMIT 1);",
                or_null(thread),
                or_null(feature)
            )
        }
    };
    if let Err(error) = client.batch_execute(&sql).await {
        // Changes that break a constraint fail, as they would for a writer.
        let code = error
            .code()
            .map(|code| code.code().to_owned())
            .unwrap_or_default();
        assert!(code.starts_with("23"), "{sql}\n{error:?}");
    }
}

#[tokio::test]
async fn random_changes_end_as_the_full_sync_would() {
    for seed in [1, 7, 42, 1009, 31337, 271828] {
        eprintln!("seed {seed}");
        let (_db, mut client) = synced().await;
        let mut rng = Rng(seed);
        for n in 0..240 {
            change(&mut client, &mut rng, n).await;
            if n % 80 == 79 {
                converged(&mut client).await;
            }
        }
        converged(&mut client).await;
    }
}
