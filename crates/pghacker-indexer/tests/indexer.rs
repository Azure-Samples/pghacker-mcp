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
async fn code_snapshots_keep_to_one_lane() {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    let payload = json!({});
    for (kind, key, priority) in [
        (jobs::kind::CODE_SNAPSHOT, "postgres/a", 3),
        (jobs::kind::CODE_SNAPSHOT, "postgres/b", 3),
        (jobs::kind::MBOX_MONTH, "pgsql-hackers/2026-10", 1),
    ] {
        assert!(
            jobs::enqueue(&client, None, kind, key, &payload, priority)
                .await
                .unwrap()
        );
    }
    // Snapshots outrank mail, but only worker 1 takes them.
    let (kinds, except) = jobs::lane(2, 5);
    let mail = jobs::claim(&client, "w2", kinds, except)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mail.kind, jobs::kind::MBOX_MONTH);
    assert!(
        jobs::claim(&client, "w3", kinds, except)
            .await
            .unwrap()
            .is_none()
    );
    let (kinds, except) = jobs::lane(1, 5);
    let code = jobs::claim(&client, "w1", kinds, except)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code.kind, jobs::kind::CODE_SNAPSHOT);

    assert!(
        jobs::lane(0, 5)
            .0
            .unwrap()
            .contains(&jobs::kind::MAIL_REFRESH)
    );
    assert!(jobs::lane(4, 5).1.contains(&jobs::kind::CODE_SNAPSHOT));
    // Fewer workers than lanes: whatever is left runs anything.
    assert_eq!(jobs::lane(0, 1), (None, &[] as &[&str]));
    assert_eq!(jobs::lane(1, 2), (None, &[] as &[&str]));
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
    let first = jobs::claim(&client, "n1", None, &[])
        .await
        .unwrap()
        .unwrap();
    let second = jobs::claim(&other, "n2", None, &[]).await.unwrap().unwrap();
    assert_eq!(first.key, "b", "higher priority first");
    assert_eq!(second.key, "a");
    assert!(
        jobs::claim(&client, "n1", None, &[])
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        jobs::claim(&client, "n1", Some(&["other"]), &[])
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
    let queued = added["jobs_queued"].as_u64().unwrap() as usize;
    assert_eq!(
        queued,
        indexer.slices.mail.months.len() + indexer.slices.mail.seed_threads.len()
    );
    let again = sources::add_source(&indexer, args(list.clone()))
        .await
        .unwrap();
    assert_eq!(again["jobs_queued"], 0);
    let other = json!({ "kind": "mailing_list", "name": "pgsql-general" });
    assert!(sources::add_source(&indexer, args(other)).await.is_err());
    let months = json!({ "kind": "mailing_list", "name": "pgsql-hackers", "months": ["2026-06"] });
    let refused = sources::add_source(&indexer, args(months))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("PGHACKER_SOURCES=open"),
        "{refused}"
    );
    let code = sources::add_source(
        &indexer,
        args(json!({ "kind": "postgres_version", "name": "19" })),
    )
    .await
    .unwrap();
    assert_eq!(
        code["queued"][0],
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
async fn leases_are_renewed_by_use_and_lapse_when_abandoned() {
    let (db, indexer) = corpus().await;
    let client = db.indexer().await.unwrap();
    let minutes_left = async |token: &str| -> f64 {
        client
            .query_one(
                "SELECT (extract(epoch FROM expires_at - now()) / 60)::float8
                 FROM extraction_leases WHERE token::text = $1",
                &[&token],
            )
            .await
            .unwrap()
            .get(0)
    };
    let set_expiry = async |token: &str, interval: &str| {
        client
            .execute(
                "UPDATE extraction_leases SET expires_at = now() + $2::text::interval WHERE token::text = $1",
                &[&token, &interval],
            )
            .await
            .unwrap();
    };
    let (token, _) = lease(&indexer).await;
    let left = minutes_left(&token).await;
    assert!(
        (19.0..=20.0).contains(&left),
        "a default lease lasts 20 minutes: {left}"
    );

    set_expiry(&token, "1 minute").await;
    let read = json!({ "lease_token": token, "thread_id": 10, "offset": 0 });
    extraction::read_messages(&indexer, args(read.clone()))
        .await
        .unwrap();
    assert!(
        minutes_left(&token).await >= 19.0,
        "using a lease renews it"
    );

    set_expiry(&token, "-1 second").await;
    let error = extraction::read_messages(&indexer, args(read))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expired"), "{error}");
    let again = extraction::next_batch(&indexer, args(json!({ "thread_ids": [10] })))
        .await
        .unwrap();
    assert_eq!(
        again["threads"][0]["thread_id"], 10,
        "an abandoned lease's thread can be leased again: {again:#}"
    );
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
async fn judged_distinct_pairs_are_not_offered_as_duplicates() {
    let (db, indexer) = corpus().await;
    let client = db.indexer().await.unwrap();
    let first = submit_extracts(&indexer).await;
    let feature = first["created"]["features"][0]["id"].as_i64().unwrap();
    let decision = first["created"]["decisions"][0]["id"].as_i64().unwrap();

    // A new version of the thread brings look-alikes that its extractor says are different.
    client
        .execute("UPDATE threads SET content_version = 2 WHERE id = 10", &[])
        .await
        .unwrap();
    let (token, _) = lease(&indexer).await;
    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 10, "kind": "feature", "name": "JSON_TABLE PLAN clause defaults", "description": "A default plan for nested paths." },
            { "thread_id": 10, "kind": "decision", "name": "Simplify nested path handling first", "description": "Before the default plan." }
        ] })),
    )
    .await
    .unwrap();
    for draft in matched["drafts"].as_array().unwrap() {
        assert_eq!(draft["near_duplicate"], true, "{matched:#}");
    }
    let saved = extraction::submit(
        &indexer,
        args(json!({
            "lease_token": token, "thread_id": 10,
            "features": [{ "draft_id": matched["drafts"][0]["draft_id"], "action": "new", "slug": "json-table-plan-defaults",
                           "name": "JSON_TABLE PLAN clause defaults", "description": "A default plan for nested paths.",
                           "status": "proposed",
                           "citations": [{ "message_id": "m1@example.org", "quote": "adding the PLAN clause to JSON_TABLE" }],
                           "distinct_because": "It changes the default plan, not the PLAN syntax." }],
            "decisions": [{ "draft_id": matched["drafts"][1]["draft_id"], "action": "new",
                            "feature_slug": "json-table-plan-defaults", "title": "Simplify nested path handling first",
                            "outcome": "Simplify nested path handling before the default plan",
                            "rationale": "The default plan builds on it",
                            "citations": [{ "message_id": "m3@example.org", "quote": "I will simplify the nested path handling first" }],
                            "distinct_because": "It orders work on the default plan, not on the PLAN clause." }]
        })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");
    assert_eq!(saved["judged_distinct"], 2);
    let added = saved["created"]["features"][0]["id"].as_i64().unwrap();
    let rows = client
        .query(
            "SELECT kind, low_id, high_id, judged_by, session FROM distinct_pairs ORDER BY kind",
            &[],
        )
        .await
        .unwrap();
    let recorded: Vec<(String, i64, String, Option<String>)> = rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(3), row.get(4)))
        .collect();
    assert_eq!(
        recorded,
        [
            (
                "decision".into(),
                decision,
                "extractor".into(),
                Some("s1".into())
            ),
            (
                "feature".into(),
                feature,
                "extractor".into(),
                Some("s1".into())
            ),
        ]
    );
    for kind in ["feature", "decision"] {
        let found = extraction::find_duplicates(&indexer, args(json!({ "kind": kind })))
            .await
            .unwrap();
        assert!(found["pairs"].as_array().unwrap().is_empty(), "{found:#}");
        assert_eq!(found["judged_distinct"], 1);
    }

    // Without its record the pair is offered again; the deduplicator's verdict hides it.
    client
        .execute("DELETE FROM distinct_pairs WHERE kind = 'feature'", &[])
        .await
        .unwrap();
    let found = extraction::find_duplicates(&indexer, args(json!({ "kind": "feature" })))
        .await
        .unwrap();
    let pair = &found["pairs"][0];
    assert_eq!(
        (pair["a"]["id"].as_i64(), pair["b"]["id"].as_i64()),
        (Some(feature), Some(added))
    );
    let verdict = json!({ "kind": "feature", "a_id": added, "b_id": feature,
                          "reason": "Different scope: the default plan, not the clause.", "session": "d1" });
    let marked = extraction::mark_distinct(&indexer, args(verdict.clone()))
        .await
        .unwrap();
    assert_eq!(marked["recorded"], true, "{marked:#}");
    assert_eq!(marked["pair"], json!([feature, added]));
    let found = extraction::find_duplicates(&indexer, args(json!({ "kind": "feature" })))
        .await
        .unwrap();
    assert!(found["pairs"].as_array().unwrap().is_empty(), "{found:#}");
    let again = extraction::mark_distinct(&indexer, args(verdict))
        .await
        .unwrap();
    assert_eq!(again["recorded"], false);
    assert_eq!(again["already_judged"]["judged_by"], "dedup");

    for (bad, expected) in [
        (
            json!({ "kind": "feature", "a_id": feature, "b_id": feature, "reason": "The same one twice." }),
            "the same",
        ),
        (
            json!({ "kind": "feature", "a_id": feature, "b_id": added, "reason": "Differs." }),
            "what differs",
        ),
        (
            json!({ "kind": "feature", "a_id": feature, "b_id": 999, "reason": "No such feature here." }),
            "must exist",
        ),
    ] {
        let error = extraction::mark_distinct(&indexer, args(bad))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
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

/// An HTTP server on a free port that answers `GET path` from `pages`, and
/// 404 otherwise.
async fn archive(pages: Vec<(&'static str, &'static str)>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let pages = pages.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("").to_owned();
                let (status, body) = pages
                    .iter()
                    .find(|(page, _)| *page == path)
                    .map_or((404, "Not found"), |(_, body)| (200, *body));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn open_sources_take_months_ranges_follows_and_commits() {
    let db = TestDb::create().await.unwrap();
    let base = archive(vec![(
        "/list/pgsql-hackers/",
        r#"<a href="/list/pgsql-hackers/2026-05/">May</a> <a href="/list/pgsql-hackers/2026-06/">June</a>
           <a href="/list/pgsql-hackers/2026-07/">July</a>"#,
    )])
    .await;
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let env = Env::from_pairs([
        ("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str()),
        ("PGHACKER_SOURCES", "open"),
        ("PGHACKER_ARCHIVE_BASE_URL", base.as_str()),
        ("PGHACKER_HTTP_RATE", "100"),
    ]);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    let indexer = Indexer::new(settings, &env).unwrap();

    let range = json!({ "kind": "mailing_list", "name": "pgsql-hackers", "from": "2026-06" });
    let added = sources::add_source(&indexer, args(range)).await.unwrap();
    assert_eq!(added["jobs_queued"], 2);
    assert_eq!(
        added["queued"],
        json!(["pgsql-hackers/2026-07", "pgsql-hackers/2026-06"])
    );
    assert_eq!(added["months"]["archive"]["first"], "2026-05");
    assert_eq!(added["source"]["config"], json!({}));

    let follow = json!({
        "kind": "mailing_list", "name": "pgsql-hackers", "months": ["2026-05"], "follow": true,
        "seed_threads": [{ "message_id": "<seed@example.org>", "topic": "a seed" }],
    });
    let added = sources::add_source(&indexer, args(follow)).await.unwrap();
    assert_eq!(
        added["queued"],
        json!([
            "pgsql-hackers/thread/seed@example.org",
            "pgsql-hackers/2026-05"
        ])
    );
    assert_eq!(added["source"]["config"]["follow"], true);
    // follow stays until it's set again.
    let again = json!({ "kind": "mailing_list", "name": "pgsql-hackers", "months": ["2026-07"] });
    let added = sources::add_source(&indexer, args(again)).await.unwrap();
    assert_eq!(added["jobs_queued"], 0);
    assert_eq!(added["source"]["config"]["follow"], true);

    let missing = json!({ "kind": "mailing_list", "name": "pgsql-nothing" });
    let error = sources::add_source(&indexer, args(missing))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no mailing list"), "{error}");
    let outside = json!({ "kind": "mailing_list", "name": "pgsql-hackers", "months": ["2025-01"] });
    assert!(sources::add_source(&indexer, args(outside)).await.is_err());

    // The hourly refresh queues the followed list's current month again once it's done.
    let client = db.indexer().await.unwrap();
    let now = "2026-07-01T10:00:00Z".parse().unwrap();
    assert!(
        sources::refresh_followed(&client, now)
            .await
            .unwrap()
            .is_empty()
    );
    client
        .execute(
            "UPDATE jobs SET state = 'done' WHERE kind = 'mbox_month'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        sources::refresh_followed(&client, now).await.unwrap(),
        ["pgsql-hackers/2026-06", "pgsql-hackers/2026-07"]
    );
    let refresh: Value = client
        .query_one(
            "SELECT payload FROM jobs WHERE key = 'pgsql-hackers/2026-07' AND state = 'queued'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(refresh["refresh"], true);

    let sha = "0123456789abcdef0123456789abcdef01234567";
    let code = json!({
        "kind": "postgres_version", "name": "18", "branch": "REL_18_STABLE", "sha": sha,
        "paths": ["src/backend/parser"],
    });
    let added = sources::add_source(&indexer, args(code)).await.unwrap();
    assert_eq!(added["queued"], json!([format!("postgres/{sha}")]));
    assert_eq!(
        added["source"]["config"]["paths"],
        json!(["src/backend/parser"])
    );
    let bad = json!({ "kind": "postgres_version", "name": "18", "sha": sha, "paths": ["../etc"] });
    assert!(sources::add_source(&indexer, args(bad)).await.is_err());
    let foreign = json!({
        "kind": "postgres_version", "name": "18", "sha": sha,
        "repo_url": "https://example.org/postgres.git",
    });
    assert!(sources::add_source(&indexer, args(foreign)).await.is_err());
    let listed = sources::list_sources(&indexer).await.unwrap();
    assert_eq!(listed["available"]["mode"], "open");

    let status = pghacker_indexer::status::ingest_status(&indexer)
        .await
        .unwrap();
    assert_eq!(status["mail"]["lists"][0]["follow"], true);
    assert_eq!(status["mail"]["lists"][0]["months"]["queued"], 2);
    assert_eq!(status["extraction"]["scheduler"], "manual");
}

#[tokio::test]
async fn one_indexer_runs_the_jobs_at_a_time() {
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    let db = TestDb::create().await.unwrap();
    let shutdown = CancellationToken::new();
    let first = jobs::lead(&db.indexer_url, &shutdown)
        .await
        .unwrap()
        .unwrap();
    let second = tokio::spawn({
        let url = db.indexer_url.clone();
        let shutdown = shutdown.clone();
        async move { jobs::lead(&url, &shutdown).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!second.is_finished(), "the second waits for the first");
    // The first stops: its connection closes and releases the lock.
    drop(first);
    let led = tokio::time::timeout(Duration::from_secs(15), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(led.is_some());
    // Stopping ends a wait.
    let stopping = CancellationToken::new();
    let third = tokio::spawn({
        let url = db.indexer_url.clone();
        let stopping = stopping.clone();
        async move { jobs::lead(&url, &stopping).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    stopping.cancel();
    let none = tokio::time::timeout(Duration::from_secs(10), third)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(none.is_none());
}
