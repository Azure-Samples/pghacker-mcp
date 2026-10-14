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

/// Leases thread `id` by its id: the lease's token.
async fn token_for(indexer: &Indexer, id: i64) -> String {
    let batch = extraction::next_batch(indexer, args(json!({ "thread_ids": [id] })))
        .await
        .unwrap();
    batch["lease_token"].as_str().unwrap().to_owned()
}

/// Stores a reply to thread `id`, sent now, as ingest does.
async fn reply(db: &TestDb, id: i64) {
    db.indexer()
        .await
        .unwrap()
        .batch_execute(&format!(
            "INSERT INTO messages (message_id, list, thread_id, parent_message_id, author_id, from_name, subject,
                                   sent_at, body, body_clean)
             VALUES ('reply-{id}@example.org', 'pgsql-hackers', {id}, 't{id}@example.org', 1, 'Alice Hacker',
                     'Re: Thread {id}', now(), 'A reply that came during the lease.',
                     'A reply that came during the lease.');
             UPDATE threads SET message_count = message_count + 1, last_at = now(),
                                content_version = content_version + 1
             WHERE id = {id};"
        ))
        .await
        .unwrap();
}

/// Each thread's state, the version extraction read, and its version now.
async fn versions(db: &TestDb) -> Vec<(i64, String, Option<i32>, i32)> {
    db.indexer()
        .await
        .unwrap()
        .query(
            "SELECT id, extraction_state, extracted_version, content_version FROM threads ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect()
}

#[tokio::test]
async fn mail_that_comes_during_a_lease_leaves_the_thread_to_lease_again() {
    let db = corpus(&[
        (1, Some(10), "pending", false),
        (2, Some(20), "pending", false),
    ])
    .await;
    let indexer = indexer(&db, &[]);
    let token = token_for(&indexer, 1).await;
    reply(&db, 1).await;
    let skipped = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 1, "skip_reason": "Only an announcement so far." })),
    )
    .await
    .unwrap();
    assert_eq!(skipped["accepted"], true, "{skipped:#}");

    let token = token_for(&indexer, 2).await;
    reply(&db, 2).await;
    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 2, "kind": "feature", "name": "Planner change", "description": "A change to the planner." }
        ] })),
    )
    .await
    .unwrap();
    let draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let saved = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 2,
            "features": [{ "draft_id": draft, "action": "new", "slug": "planner-change", "name": "Planner change",
                           "description": "A change to the planner.", "status": "proposed",
                           "citations": [{ "message_id": "t2@example.org", "quote": "proposes a change to the planner" }] }] })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");

    assert_eq!(
        versions(&db).await,
        [
            (1, "skipped".to_owned(), Some(1), 2),
            (2, "extracted".to_owned(), Some(1), 2),
        ],
        "extraction records the version its lease read"
    );
    assert_eq!(
        extraction_status(&indexer).await["threads_changed_since_extraction"],
        2
    );
    assert_eq!(
        lease_all(&indexer, 1).await.concat(),
        leased(&[(1, "new_mail"), (2, "new_mail")]),
        "the replies are new mail"
    );
}

#[tokio::test]
async fn a_lease_from_before_versions_records_the_version_at_submission() {
    let db = corpus(&[(3, Some(10), "pending", false)]).await;
    let indexer = indexer(&db, &[]);
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO extraction_leases (token, thread_ids, expires_at)
             VALUES ('00000000-0000-4000-8000-000000000003', '{3}', now() + interval '20 minutes')",
        )
        .await
        .unwrap();
    reply(&db, 3).await;
    let skipped = extraction::submit(
        &indexer,
        args(
            json!({ "lease_token": "00000000-0000-4000-8000-000000000003", "thread_id": 3,
                     "skip_reason": "Only an announcement so far." }),
        ),
    )
    .await
    .unwrap();
    assert_eq!(skipped["accepted"], true, "{skipped:#}");
    assert_eq!(versions(&db).await, [(3, "skipped".to_owned(), Some(2), 2)]);
    assert!(lease(&indexer, 1).await.is_none());
}

#[tokio::test]
async fn a_thread_that_mail_merged_away_during_its_lease_is_not_saved() {
    let db = corpus(&[
        (4, Some(10), "pending", false),
        (5, Some(11), "pending", false),
    ])
    .await;
    let indexer = indexer(&db, &[]);
    let token = token_for(&indexer, 4).await;
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "UPDATE messages SET thread_id = 5 WHERE thread_id = 4;
             DELETE FROM threads WHERE id = 4;
             UPDATE threads SET message_count = 2, content_version = content_version + 1 WHERE id = 5;",
        )
        .await
        .unwrap();
    let error = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 4, "skip_reason": "Only an announcement so far." })),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("isn't stored any more"),
        "{error}"
    );
    let live: i64 = client
        .query_one(
            "SELECT count(*) FROM extraction_leases WHERE released_at IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        live, 0,
        "the lease, with nothing left to submit, is released"
    );
    assert_eq!(
        lease_all(&indexer, 1).await.concat(),
        leased(&[(5, "backfill")])
    );
}

/// Extracts thread `id` as discussing the feature `slug`, new the first time
/// and reused after, with the release it gives, if any.
async fn claim(db: &TestDb, indexer: &Indexer, id: i64, slug: &str, release: Option<&str>) {
    let token = token_for(indexer, id).await;
    let matched = extraction::match_extracts(
        indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": id, "kind": "feature", "name": slug, "description": "A change to the planner." }
        ] })),
    )
    .await
    .unwrap();
    let draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let existing: Option<i64> = db
        .indexer()
        .await
        .unwrap()
        .query_opt("SELECT id FROM features WHERE slug = $1", &[&slug])
        .await
        .unwrap()
        .map(|row| row.get(0));
    let mut feature = json!({ "draft_id": draft, "name": slug, "description": "A change to the planner.",
        "status": "proposed", "release": release,
        "citations": [{ "message_id": format!("t{id}@example.org"), "quote": "proposes a change to the planner" }] });
    match existing {
        Some(existing) => {
            feature["action"] = json!("reuse");
            feature["existing_id"] = json!(existing);
        }
        None => {
            feature["action"] = json!("new");
            feature["slug"] = json!(slug);
        }
    }
    let saved = extraction::submit(
        indexer,
        args(json!({ "lease_token": token, "thread_id": id, "features": [feature] })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");
}

/// A feature's release, and whether its evidence is the date of thread
/// `id`'s latest message (None: no date).
async fn release(db: &TestDb, slug: &str) -> (Option<String>, Option<i64>) {
    let row = db
        .indexer()
        .await
        .unwrap()
        .query_one(
            "SELECT f.release, (SELECT min(t.id) FROM threads t WHERE t.last_at = f.release_at)
             FROM features f WHERE f.slug = $1",
            &[&slug],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

#[tokio::test]
async fn a_release_comes_from_the_most_recent_evidence_whatever_the_order() {
    let db = corpus(&[
        (1, Some(2000), "pending", false),
        (2, Some(3000), "pending", false),
        (3, None, "pending", false),
        (4, Some(100), "pending", false),
        (5, Some(1500), "pending", false),
        (6, Some(1500), "pending", false),
    ])
    .await;
    let indexer = indexer(&db, &[]);
    let at = |release: &str, thread: i64| (Some(release.to_owned()), Some(thread));

    claim(&db, &indexer, 1, "plan-clause", Some("12")).await;
    assert_eq!(release(&db, "plan-clause").await, at("12", 1));
    claim(&db, &indexer, 2, "plan-clause", Some("11")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("12", 1),
        "an older thread extracted later doesn't undo a newer one's release"
    );
    claim(&db, &indexer, 4, "plan-clause", None).await;
    claim(&db, &indexer, 5, "plan-clause", Some("13")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("13", 5),
        "a newer claim wins, though a still newer thread linked without one"
    );
    claim(&db, &indexer, 3, "plan-clause", Some("14")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("13", 5),
        "an undated claim doesn't replace a release"
    );
    claim(&db, &indexer, 6, "plan-clause", Some("15")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("15", 5),
        "evidence as recent: the later submission wins"
    );

    claim(&db, &indexer, 1, "hash-join", None).await;
    claim(&db, &indexer, 3, "hash-join", Some("16")).await;
    assert_eq!(
        release(&db, "hash-join").await,
        (Some("16".to_owned()), None),
        "an undated claim fills a missing release"
    );
    claim(&db, &indexer, 2, "hash-join", Some("17")).await;
    assert_eq!(
        release(&db, "hash-join").await,
        at("17", 2),
        "undated evidence is the oldest"
    );
}

#[tokio::test]
async fn releases_set_before_take_their_features_most_recent_thread() {
    let db = corpus(&[
        (1, Some(300), "extracted", false),
        (2, Some(30), "extracted", false),
    ])
    .await;
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO features (id, slug, name, description, status, release) VALUES
                 (1, 'with-release', 'With a release', 'A change.', 'committed', '18'),
                 (2, 'without-release', 'Without a release', 'A change.', 'proposed', NULL),
                 (3, 'unlinked', 'Unlinked', 'A change.', 'proposed', '17');
             INSERT INTO feature_threads (feature_id, thread_id) VALUES (1, 1), (1, 2), (2, 2);
             ALTER TABLE features DROP COLUMN release_at;",
        )
        .await
        .unwrap();
    client
        .batch_execute(include_str!(
            "../../../migrations/0018_feature_release_at.sql"
        ))
        .await
        .unwrap();
    assert_eq!(
        release(&db, "with-release").await,
        (Some("18".to_owned()), Some(2))
    );
    assert_eq!(release(&db, "without-release").await, (None, None));
    assert_eq!(
        release(&db, "unlinked").await,
        (Some("17".to_owned()), None)
    );
}
