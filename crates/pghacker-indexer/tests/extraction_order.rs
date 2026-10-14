//! The order in which next_extraction_batch leases threads, and what
//! extraction records of a thread whose mail changes, on fresh databases,
//! without the network or an embeddings endpoint.

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, extraction, status};
use serde_json::{Value, json};

/// A stored thread: its id; how many days ago its latest message was sent,
/// or None for no date; its extraction state; and whether mail came after it
/// was extracted or skipped.
type Thread = (i64, Option<u32>, &'static str, bool);

/// A database with `threads`, each with one message by Alice.
async fn corpus(threads: &[Thread]) -> TestDb {
    let db = TestDb::create().await.unwrap();
    let mut sql = String::from(
        "INSERT INTO authors (id, display_name, message_count) VALUES (1, 'Alice Hacker', 0);\n",
    );
    for &(id, days, state, changed) in threads {
        let at = days.map_or("NULL".to_owned(), |d| {
            format!("now() - interval '{d} days'")
        });
        let (version, extracted) = match (state, changed) {
            ("pending", _) => (1, "NULL, NULL"),
            (_, false) => (1, "1, now() - interval '1 hour'"),
            (_, true) => (2, "1, now() - interval '1 hour'"),
        };
        sql.push_str(&format!(
            "INSERT INTO threads (id, list, root_message_id, subject, started_at, last_at, message_count,
                                  participant_count, content_version, extraction_state, extracted_version, extracted_at)
             VALUES ({id}, 'pgsql-hackers', 't{id}@example.org', 'Thread {id}', {at}, {at}, 1, 1, {version},
                     '{state}', {extracted});
             INSERT INTO messages (message_id, list, thread_id, author_id, from_name, subject, sent_at, body, body_clean)
             VALUES ('t{id}@example.org', 'pgsql-hackers', {id}, 1, 'Alice Hacker', 'Thread {id}', {at},
                     'Thread {id} proposes a change to the planner.', 'Thread {id} proposes a change to the planner.');\n"
        ));
    }
    db.indexer()
        .await
        .unwrap()
        .batch_execute(&sql)
        .await
        .unwrap();
    db
}

/// An indexer on `db` with `settings` besides the defaults.
fn indexer(db: &TestDb, settings: &[(&str, &str)]) -> Indexer {
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let mut pairs = vec![("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str())];
    pairs.extend_from_slice(settings);
    let env = Env::from_pairs(pairs);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    Indexer::new(settings, &env).unwrap()
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

async fn extraction_status(indexer: &Indexer) -> Value {
    status::ingest_status(indexer).await.unwrap()["extraction"].clone()
}

#[tokio::test]
async fn a_skipped_thread_that_gets_mail_is_leased_again() {
    let db = corpus(&[
        (1, Some(3), "skipped", true),
        (2, Some(3), "skipped", false),
        (3, Some(3), "extracted", false),
    ])
    .await;
    let indexer = indexer(&db, &[]);
    let before = extraction_status(&indexer).await;
    assert_eq!(before["threads_changed_since_extraction"], 1, "{before:#}");
    assert_eq!(before["threads_skipped"], 2);

    let batch = extraction::next_batch(&indexer, args(json!({ "threads": 3 })))
        .await
        .unwrap();
    let threads = batch["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1, "{batch:#}");
    assert_eq!(threads[0]["thread_id"], 1);
    assert_eq!(threads[0]["previously"], "skipped");
    let token = batch["lease_token"].as_str().unwrap();
    let skipped = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 1, "skip_reason": "Still administrivia only." })),
    )
    .await
    .unwrap();
    assert_eq!(skipped["accepted"], true, "{skipped:#}");

    let done = extraction::next_batch(&indexer, args(json!({})))
        .await
        .unwrap();
    assert_eq!(done["done"], true, "{done:#}");
    assert_eq!(
        extraction_status(&indexer).await["threads_changed_since_extraction"],
        0
    );
}
