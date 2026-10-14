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

/// Threads in every state, in both lanes and in neither. New mail, oldest
/// first: 3, 2, 1. The backfill, newest first and the undated last: 5, 6, 7,
/// 8, 9. Threads 4 and 11 need nothing, and thread 10 has no messages.
const LANES: &[Thread] = &[
    (1, Some(1), "pending", false),
    (2, Some(3), "extracted", true),
    (3, Some(5), "skipped", true),
    (4, Some(2), "extracted", false),
    (5, Some(10), "pending", false),
    (6, Some(40), "extracted", true),
    (7, Some(400), "skipped", true),
    (8, Some(4000), "pending", false),
    (9, None, "pending", false),
    (11, Some(20), "skipped", false),
];

async fn lanes_corpus() -> TestDb {
    let db = corpus(LANES).await;
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO threads (id, list, root_message_id, subject, started_at, last_at)
             VALUES (10, 'pgsql-hackers', 't10@example.org', 'Thread 10', now(), now())",
        )
        .await
        .unwrap();
    db
}

/// One lease of up to `threads` threads, with their lanes; None when no
/// thread is free.
async fn lease(indexer: &Indexer, threads: u32) -> Option<Vec<(i64, String)>> {
    let batch = extraction::next_batch(indexer, args(json!({ "threads": threads })))
        .await
        .unwrap();
    if batch["done"] == true {
        return None;
    }
    Some(
        batch["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let lane = t["lane"].as_str().unwrap().to_owned();
                (t["thread_id"].as_i64().unwrap(), lane)
            })
            .collect(),
    )
}

/// Leases `threads` at a time until none is free.
async fn lease_all(indexer: &Indexer, threads: u32) -> Vec<Vec<(i64, String)>> {
    let mut leases = Vec::new();
    while let Some(next) = lease(indexer, threads).await {
        leases.push(next);
        assert!(leases.len() <= 20, "the leases don't end: {leases:?}");
    }
    leases
}

fn ids(leases: &[Vec<(i64, String)>]) -> Vec<Vec<i64>> {
    leases
        .iter()
        .map(|lease| lease.iter().map(|(id, _)| *id).collect())
        .collect()
}

fn leased(pairs: &[(i64, &str)]) -> Vec<(i64, String)> {
    pairs
        .iter()
        .map(|(id, lane)| (*id, (*lane).to_owned()))
        .collect()
}

/// Releases every lease, as finished sessions would.
async fn release_all(db: &TestDb) {
    db.indexer()
        .await
        .unwrap()
        .execute("UPDATE extraction_leases SET released_at = now()", &[])
        .await
        .unwrap();
}

/// Thread `id`'s latest activity, as ingest_status shows dates.
async fn last_at(db: &TestDb, id: i64) -> Value {
    let at: chrono::DateTime<chrono::Utc> = db
        .indexer()
        .await
        .unwrap()
        .query_one("SELECT last_at FROM threads WHERE id = $1", &[&id])
        .await
        .unwrap()
        .get(0);
    json!(at)
}

#[tokio::test]
async fn new_mail_and_the_backfill_take_turns() {
    let db = lanes_corpus().await;
    let indexer = indexer(&db, &[]);
    let lanes = extraction_status(&indexer).await["lanes"].clone();
    assert_eq!(lanes["order"], "lanes", "{lanes:#}");
    assert_eq!(lanes["new_mail"]["days"], 7);
    assert_eq!(lanes["new_mail"]["share"], 0.5);
    assert_eq!(lanes["new_mail"]["threads"], 3, "{lanes:#}");
    assert_eq!(lanes["new_mail"]["messages"], 3);
    assert_eq!(
        lanes["new_mail"]["oldest_activity_at"],
        last_at(&db, 3).await
    );
    assert_eq!(lanes["backfill"]["threads"], 5);
    assert_eq!(
        lanes["backfill"]["newest_activity_at"],
        last_at(&db, 5).await
    );

    let leases = lease_all(&indexer, 1).await;
    assert_eq!(
        leases.concat(),
        leased(&[
            (5, "backfill"),
            (3, "new_mail"),
            (6, "backfill"),
            (2, "new_mail"),
            (7, "backfill"),
            (1, "new_mail"),
            (8, "backfill"),
            (9, "backfill"),
        ])
    );
    let lanes = extraction_status(&indexer).await["lanes"].clone();
    assert_eq!(lanes["new_mail"]["leased"], 3, "{lanes:#}");
    assert_eq!(lanes["backfill"]["leased"], 5);
    assert_eq!(lanes["new_mail"]["leased_since_start"], 3);
    assert_eq!(lanes["backfill"]["leased_since_start"], 5);
    assert_eq!(
        lanes["backfill"]["newest_activity_at"],
        last_at(&db, 5).await,
        "leased threads still wait: the frontier doesn't pass them"
    );
}

#[tokio::test]
async fn a_lease_of_three_threads_takes_turns_too() {
    let db = lanes_corpus().await;
    let indexer = indexer(&db, &[]);
    assert_eq!(
        ids(&lease_all(&indexer, 3).await),
        [vec![5, 3, 6], vec![2, 7, 1], vec![8, 9]]
    );
}

#[tokio::test]
async fn the_share_the_window_and_the_old_order_are_settings() {
    let db = lanes_corpus().await;
    let first = indexer(&db, &[("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "1")]);
    assert_eq!(
        ids(&lease_all(&first, 1).await).concat(),
        [3, 2, 1, 5, 6, 7, 8, 9]
    );

    release_all(&db).await;
    // Thread 3's latest message is 5 days old: the backfill's newest.
    let window = indexer(&db, &[("PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "4")]);
    assert_eq!(
        ids(&lease_all(&window, 1).await).concat(),
        [3, 2, 5, 1, 6, 7, 8, 9]
    );

    release_all(&db).await;
    let largest = indexer(&db, &[("PGHACKER_EXTRACTION_ORDER", "largest")]);
    let leases = lease_all(&largest, 1).await;
    assert_eq!(ids(&leases).concat(), [1, 5, 8, 9, 2, 3, 6, 7]);
    assert!(leases.concat().iter().all(|(_, lane)| lane == "largest"));
    assert_eq!(
        extraction_status(&largest).await["lanes"]["order"],
        "largest"
    );
}

#[tokio::test]
async fn a_leased_thread_that_new_mail_moves_is_not_leased_again() {
    let db = lanes_corpus().await;
    let indexer = indexer(&db, &[]);
    assert_eq!(
        lease(&indexer, 1).await.unwrap(),
        leased(&[(5, "backfill")])
    );
    db.indexer()
        .await
        .unwrap()
        .execute(
            "UPDATE threads SET last_at = now(), message_count = 2, content_version = content_version + 1
             WHERE id = 5",
            &[],
        )
        .await
        .unwrap();
    let lanes = extraction_status(&indexer).await["lanes"].clone();
    assert_eq!(lanes["new_mail"]["threads"], 4, "{lanes:#}");
    assert_eq!(lanes["new_mail"]["leased"], 1);
    assert_eq!(
        ids(&lease_all(&indexer, 1).await).concat(),
        [3, 6, 2, 7, 1, 8, 9]
    );

    let requested = extraction::next_batch(&indexer, args(json!({ "thread_ids": [4, 5] })))
        .await
        .unwrap();
    let threads = requested["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1, "thread 5 is still leased: {requested:#}");
    assert_eq!(threads[0]["thread_id"], 4);
    assert_eq!(threads[0]["lane"], "requested");
}
