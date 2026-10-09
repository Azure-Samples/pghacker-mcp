//! The indexer's jobs, sources, graph projection and extraction workflow on
//! fresh databases, without the network or an embeddings endpoint.

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, extraction, graph, jobs, sources};
use serde_json::{Value, json};

const FIXTURE: &str = r#"
INSERT INTO authors (id, display_name, message_count) VALUES (1, 'Alice Hacker', 2), (2, 'Bob Reviewer', 1), (3, 'Bob R.', 0);
INSERT INTO author_names (author_id, name, count) VALUES (1, 'Alice Hacker', 2), (2, 'Bob Reviewer', 1), (3, 'Bob R.', 1);
INSERT INTO author_emails (email, author_id) VALUES ('alice@example.org', 1), ('bob@example.org', 2), ('bob@work.example.org', 3);
INSERT INTO threads (id, list, root_message_id, subject, message_count, participant_count, content_version) VALUES
    (10, 'pgsql-hackers', 'm1@example.org', 'JSON_TABLE plan clause', 3, 2, 1),
    (11, 'pgsql-hackers', 'm9@example.org', 'Release announcement', 1, 1, 1);
INSERT INTO messages (id, message_id, list, thread_id, parent_message_id, author_id, from_name, subject, sent_at, body, body_clean, has_patch) VALUES
    (100, 'm1@example.org', 'pgsql-hackers', 10, NULL, 1, 'Alice Hacker', 'JSON_TABLE plan clause', '2026-06-01',
     'Here is a patch adding the PLAN clause to JSON_TABLE, as the standard describes.', 'Here is a patch adding the PLAN clause to JSON_TABLE, as the standard describes.', true),
    (101, 'm2@example.org', 'pgsql-hackers', 10, 'm1@example.org', 2, 'Bob Reviewer', 'Re: JSON_TABLE plan clause', '2026-06-02',
     'I am not convinced the PLAN clause is worth its complexity in the executor.', 'I am not convinced the PLAN clause is worth its complexity in the executor.', false),
    (102, 'm3@example.org', 'pgsql-hackers', 10, 'm2@example.org', 1, 'Alice Hacker', 'Re: JSON_TABLE plan clause', '2026-06-03',
     'Fair point; I will simplify the nested path handling first.', 'Fair point; I will simplify the nested path handling first.', false),
    (103, 'm9@example.org', 'pgsql-hackers', 11, NULL, 3, 'Bob R.', 'Release announcement', '2026-06-04',
     'The release is out.', 'The release is out.', false);
INSERT INTO message_chunks (message_id, thread_id, ord, content) SELECT id, thread_id, 0, subject || E'\n' || body_clean FROM messages;
INSERT INTO patches (message_id, filename, paths, added, removed, content) VALUES
    (100, 'v1-0001-plan.patch', '{src/backend/parser/parse_jsontable.c}', 10, 1, 'diff');
INSERT INTO anchors (message_id, kind, value, origin) VALUES
    (100, 'file', 'src/backend/parser/parse_jsontable.c', 'patch'), (100, 'release', '19', 'body'),
    (101, 'commitfest', '5123', 'body');
"#;

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

async fn corpus() -> (TestDb, Indexer) {
    let db = TestDb::create().await.unwrap();
    db.indexer()
        .await
        .unwrap()
        .batch_execute(FIXTURE)
        .await
        .unwrap();
    let indexer = indexer(&db).await;
    (db, indexer)
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn jobs_are_idempotent_claimed_once_and_retried() {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    let payload = json!({ "x": 1 });
    assert!(
        jobs::enqueue(&client, None, "test", "a", &payload, 0)
            .await
            .unwrap()
    );
    assert!(
        !jobs::enqueue(&client, None, "test", "a", &payload, 0)
            .await
            .unwrap()
    );
    assert!(
        jobs::enqueue(&client, None, "test", "b", &payload, 5)
            .await
            .unwrap()
    );
    let other = db.indexer().await.unwrap();
    let first = jobs::claim(&client, "n1", None).await.unwrap().unwrap();
    let second = jobs::claim(&other, "n2", None).await.unwrap().unwrap();
    assert_eq!(first.key, "b", "higher priority first");
    assert_eq!(second.key, "a");
    assert!(jobs::claim(&client, "n1", None).await.unwrap().is_none());
    assert!(
        jobs::claim(&client, "n1", Some(&["other"]))
            .await
            .unwrap()
            .is_none()
    );
    client
        .execute("UPDATE jobs SET state = 'failed' WHERE key = 'a'", &[])
        .await
        .unwrap();
    assert!(
        jobs::enqueue(&client, None, "test", "a", &payload, 0)
            .await
            .unwrap(),
        "a failed job is queued again"
    );
    assert_eq!(jobs::requeue_orphans(&client).await.unwrap(), 1);
    jobs::ensure_recurring(&client).await.unwrap();
    let recurring: i64 = client
        .query_one(
            "SELECT count(*) FROM jobs WHERE key = 'recurring' AND state = 'queued'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(recurring as usize, jobs::RECURRING.len());
}

#[tokio::test]
async fn sources_queue_the_test_slices() {
    let db = TestDb::create().await.unwrap();
    let indexer = indexer(&db).await;
    let list = json!({ "kind": "mailing_list", "name": "pgsql-hackers" });
    let added = sources::add_source(&indexer, args(list.clone()))
        .await
        .unwrap();
    let queued = added["jobs_queued"].as_array().unwrap().len();
    assert_eq!(
        queued,
        indexer.slices.mail.months.len() + indexer.slices.mail.seed_threads.len()
    );
    let again = sources::add_source(&indexer, args(list.clone()))
        .await
        .unwrap();
    assert!(again["jobs_queued"].as_array().unwrap().is_empty());
    let other = json!({ "kind": "mailing_list", "name": "pgsql-general" });
    assert!(sources::add_source(&indexer, args(other)).await.is_err());
    let code = sources::add_source(
        &indexer,
        args(json!({ "kind": "postgres_version", "name": "19" })),
    )
    .await
    .unwrap();
    assert_eq!(
        code["jobs_queued"][0],
        format!("postgres/{}", indexer.slices.code.sha)
    );
    let removed = sources::remove_source(&indexer, args(list)).await.unwrap();
    assert_eq!(removed["jobs_cancelled"].as_u64().unwrap() as usize, queued);
    let listed = sources::list_sources(&indexer).await.unwrap();
    assert_eq!(listed["sources"].as_array().unwrap().len(), 2);
}

async fn graph_counts(client: &tokio_postgres::Client) -> (i64, i64) {
    let row = client
        .query_one(
            "SELECT (SELECT count(*) FROM graph_nodes), (SELECT count(*) FROM graph_edges)",
            &[],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn cypher_count(client: &tokio_postgres::Client, pattern: &str) -> i64 {
    client
        .query_one(
            &format!(
                "SELECT count(*) FROM ag_catalog.cypher('pghacker_graph', $$ MATCH {pattern} RETURN 1 $$) \
                 AS (x ag_catalog.agtype)"
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn the_graph_follows_the_tables() {
    let (db, indexer) = corpus().await;
    let mut client = db.indexer().await.unwrap();
    let first = graph::sync(&mut client).await.unwrap();
    assert!(first.nodes_added > 0 && first.edges_added > 0);
    let (nodes, edges) = graph_counts(&client).await;
    let second = graph::sync(&mut client).await.unwrap();
    assert_eq!(
        (
            second.nodes_added,
            second.nodes_updated,
            second.nodes_removed
        ),
        (0, 0, 0)
    );
    assert_eq!((second.edges_added, second.edges_removed), (0, 0));
    assert_eq!(graph_counts(&client).await, (nodes, edges));

    let reader = db.reader().await.unwrap();
    assert_eq!(
        cypher_count(&reader, "(:Author)-[:AUTHORED]->(:Message)").await,
        4
    );
    assert_eq!(
        cypher_count(&reader, "(:Message)-[:REPLIES_TO]->(:Message)").await,
        2
    );
    assert_eq!(
        cypher_count(&reader, "(:Message)-[:IN_THREAD]->(:Thread)").await,
        4
    );
    assert_eq!(
        cypher_count(&reader, "(:Message)-[:MENTIONS]->(:File)").await,
        1
    );
    assert_eq!(
        cypher_count(&reader, "(:Message)-[:MENTIONS]->(:CommitFestEntry)").await,
        1
    );
    let addresses: i64 = reader
        .query_one(
            "SELECT count(*) FROM pghacker_graph._ag_label_vertex v
             WHERE regexp_replace(v.properties::text, '\"(key|message_id|root_message_id)\": \"[^\"]*\"', '', 'g')
                   ~ '@(work\\.)?example\\.org'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(addresses, 0, "no email in vertex properties");

    submit_extracts(&indexer).await;
    client
        .execute(
            "UPDATE threads SET subject = 'JSON_TABLE PLAN' WHERE id = 10",
            &[],
        )
        .await
        .unwrap();
    let third = graph::sync(&mut client).await.unwrap();
    assert!(
        third.nodes_added >= 3 && third.nodes_updated >= 1,
        "{third:?}"
    );
    assert_eq!(
        cypher_count(&reader, "(:Thread)-[:DISCUSSES]->(:Feature)").await,
        1
    );
    assert_eq!(
        cypher_count(
            &reader,
            "(:Author)-[:HOLDS]->(:Position)-[:ABOUT]->(:Feature)"
        )
        .await,
        2
    );
    assert_eq!(
        cypher_count(&reader, "(:File)-[:IMPLEMENTS]->(:Feature)").await,
        1
    );
    assert!(cypher_count(&reader, "(:Feature)-[:EVIDENCED_BY]->(:Message)").await >= 1);
    assert_eq!(
        cypher_count(&reader, "(:Feature)-[:TARGETS]->(:Release)").await,
        1
    );
    assert_eq!(
        cypher_count(&reader, "(:Thread {subject: 'JSON_TABLE PLAN'})").await,
        1
    );

    client
        .batch_execute(
            "DELETE FROM citations WHERE extract_kind = 'position'; DELETE FROM positions;",
        )
        .await
        .unwrap();
    let fourth = graph::sync(&mut client).await.unwrap();
    assert_eq!(fourth.nodes_removed, 2);
    assert_eq!(cypher_count(&reader, "(:Position)").await, 0);
}

async fn lease(indexer: &Indexer) -> (String, Value) {
    let batch = extraction::next_batch(
        indexer,
        args(json!({ "thread_ids": [10], "session": "s1", "model": "test" })),
    )
    .await
    .unwrap();
    (batch["lease_token"].as_str().unwrap().to_owned(), batch)
}

async fn submit_extracts(indexer: &Indexer) -> Value {
    let (token, _) = lease(indexer).await;
    let matched = extraction::match_extracts(
        indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 10, "kind": "feature", "name": "JSON_TABLE PLAN clause", "description": "The PLAN clause for JSON_TABLE." },
            { "thread_id": 10, "kind": "decision", "name": "Simplify nested paths first", "description": "Simplify before PLAN." }
        ] })),
    )
    .await
    .unwrap();
    let feature_draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let decision_draft = matched["drafts"][1]["draft_id"].as_i64().unwrap();
    extraction::submit(
        indexer,
        args(json!({
            "lease_token": token, "thread_id": 10,
            "features": [{ "draft_id": feature_draft, "action": "new", "slug": "json-table-plan", "name": "JSON_TABLE PLAN clause",
                           "description": "The PLAN clause for JSON_TABLE.", "status": "proposed", "release": "19",
                           "files": ["src/backend/parser/parse_jsontable.c"],
                           "citations": [{ "message_id": "m1@example.org", "quote": "adding the PLAN clause to JSON_TABLE" }] }],
            "decisions": [{ "draft_id": decision_draft, "action": "new", "feature_slug": "json-table-plan",
                            "title": "Simplify nested paths first", "outcome": "Simplify nested path handling before PLAN",
                            "rationale": "Reviewer doubts the complexity",
                            "citations": [{ "message_id": "m3@example.org", "quote": "I will simplify the nested path handling first" }] }],
            "positions": [
                { "author_id": 2, "feature_slug": "json-table-plan", "stance": "concerned", "summary": "Doubts the complexity.",
                  "citations": [{ "message_id": "<m2@example.org>", "quote": "not convinced the PLAN clause is worth its complexity" }] },
                { "author_id": 1, "feature_slug": "json-table-plan", "stance": "proposes", "summary": "Proposes it.",
                  "citations": [{ "message_id": "m1@example.org", "quote": "Here is a patch adding the PLAN clause" }] }
            ]
        })),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn extraction_validates_everything_and_applies_all_or_nothing() {
    let (db, indexer) = corpus().await;
    let (token, batch) = lease(&indexer).await;
    let thread = &batch["threads"][0];
    assert_eq!(thread["participants"].as_array().unwrap().len(), 2);
    assert_eq!(thread["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        thread["messages"][0]["patches"][0]["paths"][0],
        "src/backend/parser/parse_jsontable.c"
    );
    assert!(thread["next_offset"].is_null());
    let busy = extraction::next_batch(&indexer, args(json!({ "thread_ids": [10] })))
        .await
        .unwrap();
    assert!(
        busy["threads"].as_array().unwrap().is_empty(),
        "a leased thread isn't leased twice"
    );

    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 10, "kind": "feature", "name": "JSON_TABLE PLAN clause", "description": "The PLAN clause." }
        ] })),
    )
    .await
    .unwrap();
    let draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let bad = extraction::submit(
        &indexer,
        args(json!({
            "lease_token": token, "thread_id": 10,
            "features": [{ "draft_id": draft, "action": "new", "slug": "Bad Slug", "name": "X", "description": "Y", "status": "proposed",
                           "files": ["src/not/in/thread.c"],
                           "citations": [{ "message_id": "m1@example.org", "quote": "a paraphrase of the PLAN clause" },
                                         { "message_id": "m9@example.org", "quote": "The release is out." }] }],
            "positions": [{ "author_id": 2, "feature_slug": "json-table-plan", "stance": "supports", "summary": "Likes it",
                            "citations": [{ "message_id": "m1@example.org", "quote": "adding the PLAN clause to JSON_TABLE" }] }]
        })),
    )
    .await
    .unwrap();
    assert_eq!(bad["accepted"], false);
    let all: Vec<String> = bad["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().to_owned())
        .collect();
    let all = all.join("\n");
    for expected in [
        "slug",
        "isn't verbatim",
        "isn't in thread",
        "isn't in this thread's patches",
        "author's own messages",
        "neither in this submission",
    ] {
        assert!(all.contains(expected), "missing {expected:?} in:\n{all}");
    }
    let saved: i64 = db
        .indexer()
        .await
        .unwrap()
        .query_one("SELECT count(*) FROM features", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(saved, 0, "nothing is half-applied");
}

#[tokio::test]
async fn extracts_save_with_citations_and_near_duplicates_need_a_reason() {
    let (db, indexer) = corpus().await;
    let accepted = submit_extracts(&indexer).await;
    assert_eq!(accepted["accepted"], true, "{accepted:#}");
    assert_eq!(accepted["citations"], 4);
    let client = db.indexer().await.unwrap();
    let state: String = client
        .query_one("SELECT extraction_state FROM threads WHERE id = 10", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(state, "extracted");

    client
        .execute("UPDATE threads SET content_version = 2 WHERE id = 10", &[])
        .await
        .unwrap();
    let (token, batch) = lease(&indexer).await;
    assert_eq!(
        batch["threads"][0]["existing_extracts"]["features"][0]["slug"],
        "json-table-plan"
    );
    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 10, "kind": "feature", "name": "JSON_TABLE PLAN clause", "description": "PLAN clause again." }
        ] })),
    )
    .await
    .unwrap();
    assert_eq!(matched["drafts"][0]["near_duplicate"], true, "{matched:#}");
    let draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let new = json!({ "lease_token": token, "thread_id": 10,
        "features": [{ "draft_id": draft, "action": "new", "slug": "json-table-plan-v2", "name": "JSON_TABLE PLAN clause",
                       "description": "The PLAN clause.", "status": "in_progress",
                       "citations": [{ "message_id": "m1@example.org", "quote": "adding the PLAN clause to JSON_TABLE" }] }] });
    let refused = extraction::submit(&indexer, args(new)).await.unwrap();
    assert_eq!(refused["accepted"], false);
    assert!(
        refused["errors"][0]
            .as_str()
            .unwrap()
            .contains("resembles existing feature")
    );
    let reuse = json!({ "lease_token": token, "thread_id": 10,
        "features": [{ "draft_id": draft, "action": "reuse", "existing_id": accepted["created"]["features"][0]["id"],
                       "name": "JSON_TABLE PLAN clause", "description": "The PLAN clause.", "status": "in_progress",
                       "citations": [{ "message_id": "m1@example.org", "quote": "adding the PLAN clause to JSON_TABLE" }] }] });
    let reused = extraction::submit(&indexer, args(reuse)).await.unwrap();
    assert_eq!(reused["accepted"], true, "{reused:#}");
    let status: String = client
        .query_one("SELECT status FROM features", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "in_progress");
    let leases: i64 = client
        .query_one(
            "SELECT count(*) FROM extraction_leases WHERE released_at IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        leases, 0,
        "a lease is released when its threads are submitted"
    );

    let skipped = extraction::next_batch(&indexer, args(json!({ "thread_ids": [11] })))
        .await
        .unwrap();
    let token = skipped["lease_token"].as_str().unwrap();
    let out = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 11, "skip_reason": "A release announcement without discussion." })),
    )
    .await
    .unwrap();
    assert_eq!(out["accepted"], true);
    let done = extraction::next_batch(&indexer, args(json!({})))
        .await
        .unwrap();
    assert_eq!(done["done"], true);
}

#[tokio::test]
async fn merges_are_logged_and_undone() {
    let (db, indexer) = corpus().await;
    let client = db.indexer().await.unwrap();
    let merged = extraction::merge_authors(
        &indexer,
        args(json!({ "from_id": 3, "into_id": 2, "reason": "Same person, work address" })),
    )
    .await
    .unwrap();
    let row = client
        .query_one(
            "SELECT (SELECT author_id FROM messages WHERE id = 103), (SELECT merged_into FROM authors WHERE id = 3),
                    (SELECT count(*) FROM author_emails WHERE author_id = 2), (SELECT message_count FROM authors WHERE id = 2),
                    (SELECT moved::text FROM merge_log ORDER BY id DESC LIMIT 1)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 2);
    assert_eq!(row.get::<_, Option<i64>>(1), Some(2));
    assert_eq!(row.get::<_, i64>(2), 2);
    assert_eq!(row.get::<_, i32>(3), 2);
    assert!(
        !row.get::<_, String>(4).contains('@'),
        "the merge log keeps no addresses"
    );
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    let row = client
        .query_one(
            "SELECT (SELECT author_id FROM messages WHERE id = 103), (SELECT merged_into FROM authors WHERE id = 3),
                    (SELECT count(*) FROM author_emails WHERE author_id = 3)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 3);
    assert_eq!(row.get::<_, Option<i64>>(1), None);
    assert_eq!(row.get::<_, i64>(2), 1);

    submit_extracts(&indexer).await;
    client
        .execute(
            "INSERT INTO features (id, slug, name, description, status)
             VALUES (99, 'json-table-plan-dup', 'JSON_TABLE PLAN', 'Dup.', 'unknown')",
            &[],
        )
        .await
        .unwrap();
    client
        .execute("INSERT INTO feature_threads VALUES (99, 11)", &[])
        .await
        .unwrap();
    let duplicates = extraction::find_duplicates(&indexer, args(json!({ "kind": "feature" })))
        .await
        .unwrap();
    assert_eq!(
        duplicates["pairs"].as_array().unwrap().len(),
        1,
        "{duplicates:#}"
    );
    let survivor: i64 = client
        .query_one(
            "SELECT id FROM features WHERE slug = 'json-table-plan'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let merged = extraction::merge_extracts(
        &indexer,
        args(json!({ "kind": "feature", "from_id": 99, "into_id": survivor, "reason": "Same feature" })),
    )
    .await
    .unwrap();
    let threads: i64 = client
        .query_one(
            "SELECT count(*) FROM feature_threads WHERE feature_id = $1",
            &[&survivor],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(threads, 2);
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    let row = client
        .query_one(
            "SELECT (SELECT count(*) FROM feature_threads WHERE feature_id = $1),
                    (SELECT merged_into FROM features WHERE id = 99)",
            &[&survivor],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, Option<i64>>(1), None);
}

#[tokio::test]
async fn stale_counts_are_repaired() {
    let (db, _indexer) = corpus().await;
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "UPDATE threads SET participant_count = 99 WHERE id = 10;
             UPDATE authors SET message_count = 7 WHERE id = 1;",
        )
        .await
        .unwrap();
    let repaired = pghacker_indexer::repair::thread_and_author_stats(&client)
        .await
        .unwrap();
    assert!(repaired >= 2, "{repaired}");
    let row = client
        .query_one(
            "SELECT (SELECT participant_count FROM threads WHERE id = 10), (SELECT message_count FROM authors WHERE id = 1)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 2);
    assert_eq!(row.get::<_, i32>(1), 2);
    assert_eq!(
        pghacker_indexer::repair::thread_and_author_stats(&client)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn leases_show_commit_signals_and_reuse_never_downgrades_a_status() {
    let (db, indexer) = corpus().await;
    submit_extracts(&indexer).await;
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "UPDATE messages SET body_clean = body_clean || E'\\nPushed to master, thanks!' WHERE id = 102;
             UPDATE features SET status = 'committed';
             UPDATE threads SET content_version = 2 WHERE id = 10;",
        )
        .await
        .unwrap();
    let (token, batch) = lease(&indexer).await;
    let signals = batch["threads"][0]["status_signals"].as_array().unwrap();
    assert_eq!(signals.len(), 1, "{batch:#}");
    assert_eq!(signals[0]["message_id"], "m3@example.org");
    assert_eq!(signals[0]["line"], "Pushed to master, thanks!");
    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 10, "kind": "feature", "name": "JSON_TABLE PLAN clause", "description": "The PLAN clause." }
        ] })),
    )
    .await
    .unwrap();
    let draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let id: i64 = client
        .query_one("SELECT id FROM features", &[])
        .await
        .unwrap()
        .get(0);
    let reuse = json!({ "lease_token": token, "thread_id": 10,
        "features": [{ "draft_id": draft, "action": "reuse", "existing_id": id,
                       "name": "JSON_TABLE PLAN clause", "description": "The PLAN clause.", "status": "proposed",
                       "citations": [{ "message_id": "m1@example.org", "quote": "adding the PLAN clause to JSON_TABLE" }] }] });
    let reused = extraction::submit(&indexer, args(reuse)).await.unwrap();
    assert_eq!(reused["accepted"], true, "{reused:#}");
    let status: String = client
        .query_one("SELECT status FROM features", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "committed");
}
