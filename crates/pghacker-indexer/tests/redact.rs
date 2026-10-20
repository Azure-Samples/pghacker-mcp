// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The redact_addresses job's extracts and patch_texts phases: extracts and
//! quotes stored with addresses are redacted, embedded and extracted again,
//! and patch lines with addresses that no patch uses go.

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::jobs::{self, Job, Outcome};
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, redact};
use serde_json::{Value, json};

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

/// The job runs its phases to the end: messages, extracts, then, once the
/// changed patches are indexed again, the patch lines no patch uses.
#[tokio::test]
async fn the_job_redacts_stored_mail_in_phases() {
    let db = TestDb::create().await.unwrap();
    let indexer = indexer(&db).await;
    let mut client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO authors (id, display_name) VALUES (1, 'Tom <tom@example.org>');
             INSERT INTO author_names (author_id, name, count) VALUES (1, 'Tom <tom@example.org>', 1);
             INSERT INTO threads (id, list, root_message_id, subject, message_count)
             VALUES (1, 'pgsql-hackers', 'r1@example.org', 'Hi bob@example.org', 1);
             INSERT INTO messages (id, message_id, list, thread_id, author_id, from_name, subject, body, body_clean, has_patch)
             VALUES (1, 'r1@example.org', 'pgsql-hackers', 1, 1, 'Tom <tom@example.org>', 'Hi bob@example.org',
                     'See https://postgr.es/m/x@example.org or ask bob@example.org', 'ask bob@example.org', true);
             INSERT INTO message_chunks (message_id, thread_id, ord, content)
             VALUES (1, 1, 0, 'Hi bob@example.org\n\nask bob@example.org');
             INSERT INTO patches (id, message_id, filename, content)
             VALUES (1, 1, 'fix.patch', 'From: Tom <tom@example.org>\n--- a/x.c\n+++ b/x.c\n@@ -1 +1 @@\n-a\n+b\n');",
        )
        .await
        .unwrap();
    pghacker_mail::patch_lines::index(&mut client, 10)
        .await
        .unwrap();
    let with_address = "SELECT count(*) FROM patch_texts WHERE line LIKE '%tom@example.org%'";
    let lines: i64 = client.query_one(with_address, &[]).await.unwrap().get(0);
    assert_eq!(lines, 1);

    jobs::enqueue(
        &client,
        None,
        jobs::kind::REDACT_ADDRESSES,
        "redact",
        &json!({}),
        1,
    )
    .await
    .unwrap();
    let id: i64 = client
        .query_one("SELECT id FROM jobs WHERE kind = 'redact_addresses'", &[])
        .await
        .unwrap()
        .get(0);
    let mut result = Value::Null;
    for _ in 0..20 {
        let payload: Value = client
            .query_one("SELECT payload FROM jobs WHERE id = $1", &[&id])
            .await
            .unwrap()
            .get(0);
        let job = Job {
            id,
            source_id: None,
            kind: jobs::kind::REDACT_ADDRESSES.to_owned(),
            key: "redact".to_owned(),
            payload,
            attempts: 0,
            max_attempts: 5,
        };
        match redact::run(&indexer, &job).await.unwrap() {
            Outcome::Done(done) => {
                result = done;
                break;
            }
            // The recurring patch_lines job, meanwhile.
            Outcome::Again { .. } => {
                pghacker_mail::patch_lines::index(&mut client, 10)
                    .await
                    .unwrap();
            }
        }
    }
    assert_eq!(result["phase"], "done", "{result}");
    assert_eq!(result["messages"], 1, "{result}");
    assert_eq!(result["patches"], 1, "{result}");
    assert_eq!(result["patch_texts"], 1, "{result}");

    let row = client
        .query_one(
            "SELECT m.from_name, m.subject, m.body, t.subject, a.display_name, c.content, c.embedding IS NULL
             FROM messages m JOIN threads t ON t.id = m.thread_id JOIN authors a ON a.id = m.author_id
             JOIN message_chunks c ON c.message_id = m.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Tom");
    assert_eq!(row.get::<_, String>(1), "Hi [email]");
    assert_eq!(
        row.get::<_, String>(2),
        "See https://postgr.es/m/x@example.org or ask [email]"
    );
    assert_eq!(row.get::<_, String>(3), "Hi [email]");
    assert_eq!(row.get::<_, String>(4), "Tom");
    assert_eq!(row.get::<_, String>(5), "Hi [email]\n\nask [email]");
    assert!(row.get::<_, bool>(6));
    let lines: i64 = client.query_one(with_address, &[]).await.unwrap().get(0);
    assert_eq!(lines, 0);
    let indexed: bool = client
        .query_one("SELECT text_ids IS NOT NULL FROM patches", &[])
        .await
        .unwrap()
        .get(0);
    assert!(indexed);
}

#[tokio::test]
async fn extracts_with_addresses_are_redacted_and_extracted_again() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO authors (id, display_name) VALUES (1, 'tom@example.org'), (2, 'Ann');
             INSERT INTO author_names (author_id, name, count) VALUES (1, 'tom@example.org', 2), (1, 'tom', 1);
             INSERT INTO threads (id, list, root_message_id, subject, content_version, extraction_state, extracted_version)
             VALUES (1, 'pgsql-hackers', 'r1@example.org', 'One', 3, 'extracted', 3),
                    (2, 'pgsql-hackers', 'r2@example.org', 'Two', 1, 'extracted', 1),
                    (3, 'pgsql-hackers', 'r3@example.org', 'Three', 0, 'pending', NULL);
             INSERT INTO messages (id, message_id, list, thread_id, author_id, body)
             VALUES (1, 'r1@example.org', 'pgsql-hackers', 1, 1, 'Ask [email] about it, please.'),
                    (2, 'r2@example.org', 'pgsql-hackers', 2, 2, 'Nothing to hide here at all.'),
                    (3, 'r3@example.org', 'pgsql-hackers', 3, 2, 'Write to [email] with questions.');
             INSERT INTO features (id, slug, name, description, status, embedding)
             VALUES (1, 'f', 'F', 'Proposed by ann@example.org', 'proposed', array_fill(0.1, ARRAY[1536])::halfvec),
                    (2, 'g', 'G', 'Clean', 'proposed', array_fill(0.1, ARRAY[1536])::halfvec);
             INSERT INTO feature_threads VALUES (1, 1), (2, 2);
             INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary, embedding)
             VALUES (1, 2, 3, 2, 'supports', 'Wants mail at ann@example.org', array_fill(0.1, ARRAY[1536])::halfvec);
             INSERT INTO citations (extract_kind, extract_id, message_id, quote)
             VALUES ('feature', 1, 1, 'Ask tom@example.org about it'),
                    ('feature', 1, 1, 'Ask [email] about it'),
                    ('feature', 2, 2, 'Nothing to hide here');",
        )
        .await
        .unwrap();

    let tx = client.transaction().await.unwrap();
    let done = redact::redact_extracts(&tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (done.authors, done.extracts, done.citations, done.threads),
        (1, 2, 1, 1)
    );

    let author: String = client
        .query_one("SELECT display_name FROM authors WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(author, "tom");
    let names: Vec<(String, i32)> = client
        .query("SELECT name, count FROM author_names ORDER BY name", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(names, [("tom".to_owned(), 3)]);

    let features: Vec<(String, bool)> = client
        .query(
            "SELECT description, embedding IS NULL FROM features ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        features,
        [
            ("Proposed by [email]".to_owned(), true),
            ("Clean".to_owned(), false)
        ]
    );
    let position: (String, bool) = {
        let r = client
            .query_one("SELECT summary, embedding IS NULL FROM positions", &[])
            .await
            .unwrap();
        (r.get(0), r.get(1))
    };
    assert_eq!(position, ("Wants mail at [email]".to_owned(), true));

    // The quote redacted is the one already there: it goes.
    let quotes: Vec<String> = client
        .query("SELECT quote FROM citations ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(quotes, ["Ask [email] about it", "Nothing to hide here"]);

    // Thread 1 is extracted again; thread 2 had nothing to redact, thread 3
    // isn't extracted yet.
    let versions: Vec<(i32, Option<i32>)> = client
        .query(
            "SELECT content_version, extracted_version FROM threads ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(versions, [(4, Some(3)), (1, Some(1)), (0, None)]);

    // The graph hears of the author, feature and position.
    let keys: Vec<String> = client
        .query(
            "SELECT DISTINCT key FROM graph_changes WHERE label IS NULL ORDER BY key",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(keys.contains(&"author:1".to_owned()), "{keys:?}");
    assert!(keys.contains(&"position:1".to_owned()), "{keys:?}");

    // Again, nothing changes.
    let tx = client.transaction().await.unwrap();
    let again = redact::redact_extracts(&tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (
            again.authors,
            again.extracts,
            again.citations,
            again.threads
        ),
        (0, 0, 0, 0)
    );
}

/// A submission's addresses are redacted: its quotes then match the stored,
/// redacted messages, and no extract stores an address.
#[tokio::test]
async fn submissions_are_redacted() {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO authors (id, display_name, message_count) VALUES (1, 'Alice', 1);
             INSERT INTO threads (id, list, root_message_id, subject, started_at, last_at, message_count, participant_count)
             VALUES (1, 'pgsql-hackers', 't1@example.org', 'Thread', now(), now(), 1, 1);
             INSERT INTO messages (message_id, list, thread_id, author_id, from_name, subject, sent_at, body, body_clean)
             VALUES ('t1@example.org', 'pgsql-hackers', 1, 1, 'Alice', 'Thread', now(),
                     'Send reviews of SQL/JSON to [email] please.', 'Send reviews of SQL/JSON to [email] please.');",
        )
        .await
        .unwrap();
    let indexer = indexer(&db).await;
    let batch =
        pghacker_indexer::extraction::next_batch(&indexer, args(json!({ "thread_ids": [1] })))
            .await
            .unwrap();
    let token = batch["lease_token"].as_str().unwrap().to_owned();
    let matched = pghacker_indexer::extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 1, "kind": "feature", "name": "SQL/JSON", "description": "SQL/JSON." }
        ] })),
    )
    .await
    .unwrap();
    let saved = pghacker_indexer::extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 1, "features": [{
            "draft_id": matched["drafts"][0]["draft_id"], "action": "new", "slug": "sql-json",
            "name": "SQL/JSON", "description": "Reviews go to alice@example.org.", "status": "proposed",
            "citations": [{ "message_id": "t1@example.org",
                            "quote": "reviews of SQL/JSON to alice@example.org please" }] }] })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");
    let row = client
        .query_one(
            "SELECT f.description, c.quote FROM features f JOIN citations c ON c.extract_id = f.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Reviews go to [email].");
    assert_eq!(
        row.get::<_, String>(1),
        "reviews of SQL/JSON to [email] please"
    );
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}
