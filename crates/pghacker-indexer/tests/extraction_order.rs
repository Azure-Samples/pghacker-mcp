// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The order in which next_extraction_batch leases threads, and what
//! extraction records of a thread whose mail changes, on fresh databases,
//! without the network or an embeddings endpoint.

use chrono::Datelike;
use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, extraction, graph, status};
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
async fn each_release_given_replaces_the_features_whatever_its_evidence() {
    let db = corpus(&[
        (1, Some(2000), "pending", false),
        (2, Some(3000), "pending", false),
        (3, None, "pending", false),
        (4, Some(100), "pending", false),
    ])
    .await;
    let indexer = indexer(&db, &[]);
    let at = |release: &str, thread: i64| (Some(release.to_owned()), Some(thread));

    claim(&db, &indexer, 1, "plan-clause", Some("12")).await;
    assert_eq!(release(&db, "plan-clause").await, at("12", 1));
    claim(&db, &indexer, 2, "plan-clause", Some("11")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("11", 2),
        "an older thread extracted later corrects a newer one's release"
    );
    claim(&db, &indexer, 4, "plan-clause", None).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("11", 2),
        "a thread that gives none leaves it"
    );
    claim(&db, &indexer, 3, "plan-clause", Some("14")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        (Some("14".to_owned()), None),
        "an undated thread's replaces it too, without a date"
    );
    claim(&db, &indexer, 4, "plan-clause", Some("16")).await;
    assert_eq!(release(&db, "plan-clause").await, at("16", 4));
    claim(&db, &indexer, 1, "plan-clause", Some("16")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("16", 4),
        "an older thread that states it again leaves its latest evidence"
    );

    // The extractor sees when the evidence for each release was written.
    let evidence = last_at(&db, 4).await;
    let batch = extraction::next_batch(&indexer, args(json!({ "thread_ids": [1] })))
        .await
        .unwrap();
    let existing = &batch["threads"][0]["existing_extracts"]["features"][0];
    assert_eq!(
        (&existing["release"], &existing["release_evidence_at"]),
        (&json!("16"), &evidence)
    );
    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": batch["lease_token"], "drafts": [
            { "thread_id": 1, "kind": "feature", "name": "plan-clause", "description": "A change to the planner." }
        ] })),
    )
    .await
    .unwrap();
    let candidate = &matched["drafts"][0]["candidates"][0];
    assert_eq!(
        (&candidate["release"], &candidate["release_evidence_at"]),
        (&json!("16"), &evidence)
    );
}

/// Submits new features (slug, release) from thread `id`: the reply.
async fn submit_releases(indexer: &Indexer, id: i64, features: &[(&str, &str)]) -> Value {
    let token = token_for(indexer, id).await;
    let drafts: Vec<Value> = features
        .iter()
        .map(|(slug, _)| json!({ "thread_id": id, "kind": "feature", "name": slug, "description": slug }))
        .collect();
    let matched = extraction::match_extracts(
        indexer,
        args(json!({ "lease_token": token, "drafts": drafts })),
    )
    .await
    .unwrap();
    let new: Vec<Value> = features
        .iter()
        .enumerate()
        .map(|(i, (slug, release))| {
            json!({ "draft_id": matched["drafts"][i]["draft_id"], "action": "new", "slug": slug,
                    "name": slug, "description": slug, "status": "committed", "release": release,
                    "citations": cites(&format!("t{id}@example.org"), "proposes a change to the planner") })
        })
        .collect();
    let saved = extraction::submit(
        indexer,
        args(json!({ "lease_token": token, "thread_id": id, "features": new })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");
    saved
}

#[tokio::test]
async fn a_release_that_isnt_one_major_version_isnt_saved() {
    let db = corpus(&[
        (1, Some(100), "pending", false),
        (2, Some(7000), "pending", false),
    ])
    .await;
    let mut client = db.indexer().await.unwrap();
    let indexer = indexer(&db, &[]);
    // Thread 1 ended 100 days ago: it can name two past the version then in
    // development.
    let year = (chrono::Utc::now() - chrono::Duration::days(100)).year();
    let latest = (pghacker_mail::anchors::development_version(year) + 2).to_string();
    let beyond = (pghacker_mail::anchors::development_version(year) + 3).to_string();
    let saved = submit_releases(
        &indexer,
        1,
        &[
            ("aggregate-pushdown", "17"),
            ("brin-bloom", "17.0"),
            ("copy-header", "PG 9.6"),
            ("toast-zstd", &latest),
            ("libxml-deprecation", "12-16"),
            ("relkind-errors", "11, 12, 13, 14"),
            ("pgbench-catchup", "14 and 15"),
            ("perl-warning", "6.4.1"),
            ("vacuum-options", "17.2"),
            ("auto-explain", "8"),
            ("upsert", "9"),
            ("partition-pruning", &beyond),
        ],
    )
    .await;
    let releases: Vec<(String, Option<String>)> = client
        .query("SELECT slug, release FROM features ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let release = |slug: &str, release: Option<&str>| (slug.to_owned(), release.map(str::to_owned));
    assert_eq!(
        releases,
        [
            release("aggregate-pushdown", Some("17")),
            release("brin-bloom", Some("17")),
            release("copy-header", Some("9.6")),
            release("toast-zstd", Some(&latest)),
            release("libxml-deprecation", None),
            release("relkind-errors", None),
            release("pgbench-catchup", None),
            release("perl-warning", None),
            release("vacuum-options", None),
            release("auto-explain", None),
            release("upsert", None),
            release("partition-pruning", None),
        ],
        "each feature is saved, with its release only if it's one major version"
    );
    let not_saved: Vec<(&str, &str)> = saved["releases_not_saved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["slug"].as_str().unwrap(), r["release"].as_str().unwrap()))
        .collect();
    assert_eq!(
        not_saved,
        [
            ("libxml-deprecation", "12-16"),
            ("relkind-errors", "11, 12, 13, 14"),
            ("pgbench-catchup", "14 and 15"),
            ("perl-warning", "6.4.1"),
            ("vacuum-options", "17.2"),
            ("auto-explain", "8"),
            ("upsert", "9"),
            ("partition-pruning", beyond.as_str()),
        ]
    );
    assert!(
        saved["releases_not_saved"][0]["why"]
            .as_str()
            .unwrap()
            .ends_with(&format!("or 10 to {latest}"))
    );
    assert_eq!(
        release_dates(&db).await,
        4,
        "a release that isn't saved has no date"
    );

    // Thread 2 ended about 2007, before releases were numbered from 10.
    let saved = submit_releases(
        &indexer,
        2,
        &[("hash-indexes", "10"), ("hot-updates", "8.3")],
    )
    .await;
    assert_eq!(saved["releases_not_saved"][0]["slug"], "hash-indexes");
    assert_eq!(saved["releases_not_saved"].as_array().unwrap().len(), 1);

    // A reused feature keeps its release when the one given isn't one.
    claim(&db, &indexer, 1, "aggregate-pushdown", Some("17, 18")).await;
    let kept: String = client
        .query_one(
            "SELECT release FROM features WHERE slug = 'aggregate-pushdown'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, "17");

    graph::sync(&mut client).await.unwrap();
    assert_eq!(
        cypher(&db, "MATCH (r:Release) RETURN r.version").await,
        ["17", latest.as_str(), "8.3", "9.6"]
    );
}

/// How many features have a release's date.
async fn release_dates(db: &TestDb) -> i64 {
    db.indexer()
        .await
        .unwrap()
        .query_one("SELECT count(release_at) FROM features", &[])
        .await
        .unwrap()
        .get(0)
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

#[tokio::test]
async fn a_release_is_saved_by_the_name_it_shipped_as() {
    let db = corpus(&[
        (1, Some(500), "pending", false),
        (2, Some(400), "pending", false),
        (3, Some(300), "pending", false),
        (4, Some(200), "pending", false),
        (5, Some(100), "pending", false),
    ])
    .await;
    let indexer = indexer(&db, &[]);
    let at = |release: &str, thread: i64| (Some(release.to_owned()), Some(thread));

    claim(&db, &indexer, 1, "foreign-keys", Some("6.6")).await;
    assert_eq!(release(&db, "foreign-keys").await, at("7.0", 1));
    claim(&db, &indexer, 2, "savepoints", Some("7.5")).await;
    assert_eq!(release(&db, "savepoints").await, at("8.0", 2));
    claim(&db, &indexer, 3, "sql-pgq", Some("19")).await;
    assert_eq!(
        release(&db, "sql-pgq").await,
        at("19", 3),
        "a release that shipped as it was named"
    );

    claim(&db, &indexer, 1, "plan-clause", None).await;
    claim(&db, &indexer, 4, "plan-clause", Some("8.5")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("9.0", 4),
        "a reused feature's missing release"
    );
    claim(&db, &indexer, 5, "plan-clause", Some("9.7")).await;
    assert_eq!(
        release(&db, "plan-clause").await,
        at("10", 5),
        "a newer thread's release replaces it"
    );
}

/// The strings a Cypher query returns, without agtype's quotes, sorted.
async fn cypher(db: &TestDb, query: &str) -> Vec<String> {
    let mut values: Vec<String> = db
        .reader()
        .await
        .unwrap()
        .query(
            &format!(
                "SELECT x::text FROM ag_catalog.cypher('pghacker_graph', $$ {query} $$) \
                 AS (x ag_catalog.agtype)"
            ),
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0).trim_matches('"').to_owned())
        .collect();
    values.sort();
    values
}

#[tokio::test]
async fn releases_saved_by_development_names_take_their_shipped_names() {
    let db = corpus(&[(1, Some(30), "extracted", false)]).await;
    let mut client = db.indexer().await.unwrap();
    // As images before 0021 saved them.
    client
        .batch_execute(
            "INSERT INTO features (id, slug, name, description, status, release, release_at) VALUES
                 (1, 'foreign-keys', 'Foreign keys', 'A change.', 'committed', '6.6', '1999-09-28'),
                 (2, 'savepoints', 'Savepoints', 'A change.', 'committed', '7.5', '2004-07-11'),
                 (3, 'hot-standby', 'Hot standby', 'A change.', 'committed', '8.5', '2010-01-12'),
                 (4, 'standby-queries', 'Standby queries', 'A change.', 'committed', '8.5', '2009-11-02'),
                 (5, 'pglogical', 'pglogical', 'A change.', 'proposed', '9.7', '2016-04-15'),
                 (6, 'streaming-replication', 'Streaming replication', 'A change.', 'committed', '9.0',
                  '2010-03-23'),
                 (7, 'sql-pgq', 'SQL/PGQ', 'A change.', 'in_progress', '19', '2026-01-01'),
                 (8, 'no-release', 'No release', 'A change.', 'proposed', NULL, NULL);
             UPDATE features SET merged_into = 3 WHERE id = 4;
             INSERT INTO feature_threads (feature_id, thread_id) SELECT id, 1 FROM features;",
        )
        .await
        .unwrap();
    graph::sync(&mut client).await.unwrap();
    let releases = "MATCH (r:Release) RETURN r.version";
    assert_eq!(
        cypher(&db, releases).await,
        ["19", "6.6", "7.5", "8.5", "9.0", "9.7"]
    );
    let dated = "SELECT string_agg(id || ' ' || coalesce(release_at::text, '-'), ', ' ORDER BY id) FROM features";
    let dates: String = client.query_one(dated, &[]).await.unwrap().get(0);

    client
        .batch_execute("DROP FUNCTION shipped_release(text)")
        .await
        .unwrap();
    client
        .batch_execute(include_str!(
            "../../../migrations/0021_shipped_releases.sql"
        ))
        .await
        .unwrap();
    let saved: Vec<(i64, Option<String>)> = client
        .query("SELECT id, release FROM features ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let shipped = |id: i64, release: &str| (id, Some(release.to_owned()));
    assert_eq!(
        saved,
        [
            shipped(1, "7.0"),
            shipped(2, "8.0"),
            shipped(3, "9.0"),
            shipped(4, "9.0"),
            shipped(5, "10"),
            shipped(6, "9.0"),
            shipped(7, "19"),
            (8, None),
        ],
        "every feature, merged or not"
    );
    let unchanged: String = client.query_one(dated, &[]).await.unwrap().get(0);
    assert_eq!(unchanged, dates, "the evidence's dates stay");

    // The change capture brings the graph along, without a full sync.
    let full_requested: bool = client
        .query_one(
            "SELECT full_requested > full_done FROM graph_sync_state",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!full_requested);
    assert!(graph::pending(&client).await.unwrap());
    while graph::sync_changes(&mut client, graph::BATCH)
        .await
        .unwrap()
        .changes
        > 0
    {}
    assert_eq!(
        cypher(&db, releases).await,
        ["10", "19", "7.0", "8.0", "9.0"]
    );
    assert_eq!(
        cypher(
            &db,
            "MATCH (f:Feature)-[:TARGETS]->(r:Release) RETURN f.slug + ' ' + r.version"
        )
        .await,
        [
            "foreign-keys 7.0",
            "hot-standby 9.0",
            "pglogical 10",
            "savepoints 8.0",
            "sql-pgq 19",
            "streaming-replication 9.0"
        ]
    );
    let drift = graph::check(&mut client).await.unwrap();
    assert!(drift.is_empty(), "{drift:#?}");
}

/// Each position: its id, author, feature, decision, stance, summary and
/// how many citations it has.
type Position = (i64, i64, Option<i64>, Option<i64>, String, String, i64);

async fn positions(db: &TestDb) -> Vec<Position> {
    let client = db.indexer().await.unwrap();
    let orphans: i64 = client
        .query_one(
            "SELECT count(*) FROM citations c WHERE c.extract_kind = 'position'
               AND NOT EXISTS (SELECT FROM positions p WHERE p.id = c.extract_id)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(orphans, 0, "no citation outlives its position");
    client
        .query(
            "SELECT p.id, p.author_id, p.feature_id, p.decision_id, p.stance, p.summary,
                    (SELECT count(*) FROM citations c WHERE c.extract_kind = 'position' AND c.extract_id = p.id)
             FROM positions p ORDER BY p.id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get(0),
                row.get(1),
                row.get(2),
                row.get(3),
                row.get(4),
                row.get(5),
                row.get(6),
            )
        })
        .collect()
}

/// How many times `pattern` matches in the graph, after a sync.
async fn graph_count(db: &TestDb, pattern: &str) -> i64 {
    let mut client = db.indexer().await.unwrap();
    graph::sync(&mut client).await.unwrap();
    db.reader()
        .await
        .unwrap()
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

fn cites(message_id: &str, quote: &str) -> Value {
    json!([{ "message_id": message_id, "quote": quote }])
}

#[tokio::test]
async fn a_restated_position_replaces_the_authors_earlier_one_in_the_thread() {
    let db = corpus(&[(1, Some(10), "pending", false)]).await;
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO authors (id, display_name, message_count) VALUES (2, 'Bob Reviewer', 0);
             INSERT INTO messages (message_id, list, thread_id, parent_message_id, author_id, from_name, subject,
                                   sent_at, body, body_clean)
             VALUES ('bob@example.org', 'pgsql-hackers', 1, 't1@example.org', 2, 'Bob Reviewer', 'Re: Thread 1',
                     now() - interval '9 days', 'I am not convinced the planner change is worth it.',
                     'I am not convinced the planner change is worth it.');
             UPDATE threads SET message_count = 2, participant_count = 2 WHERE id = 1;",
        )
        .await
        .unwrap();
    let indexer = indexer(&db, &[]);
    let alice = "t1@example.org";
    let bob = "bob@example.org";

    let token = token_for(&indexer, 1).await;
    let matched = extraction::match_extracts(
        &indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": 1, "kind": "feature", "name": "Planner change", "description": "A change to the planner." },
            { "thread_id": 1, "kind": "decision", "name": "Simplify first", "description": "Simplify it first." }
        ] })),
    )
    .await
    .unwrap();
    let feature_draft = matched["drafts"][0]["draft_id"].as_i64().unwrap();
    let decision_draft = matched["drafts"][1]["draft_id"].as_i64().unwrap();
    let first = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 1,
            "features": [{ "draft_id": feature_draft, "action": "new", "slug": "planner-change", "name": "Planner change",
                           "description": "A change to the planner.", "status": "proposed",
                           "citations": cites(alice, "proposes a change to the planner") }],
            "decisions": [{ "draft_id": decision_draft, "action": "new", "feature_slug": "planner-change",
                            "title": "Simplify first", "outcome": "Simplify the change first",
                            "rationale": "Bob doubts it is worth it",
                            "citations": cites(bob, "not convinced the planner change is worth it") }],
            "positions": [
                { "author_id": 1, "feature_slug": "planner-change", "stance": "proposes", "summary": "Proposes it.",
                  "citations": cites(alice, "proposes a change to the planner") },
                { "author_id": 2, "feature_slug": "planner-change", "stance": "concerned", "summary": "Doubts it.",
                  "citations": cites(bob, "not convinced the planner change is worth it") },
                { "author_id": 1, "decision_draft_id": decision_draft, "stance": "supports",
                  "summary": "Agrees to simplify.", "citations": cites(alice, "Thread 1 proposes a change") }
            ] })),
    )
    .await
    .unwrap();
    assert_eq!(first["accepted"], true, "{first:#}");
    assert_eq!(first["created"]["positions"].as_array().unwrap().len(), 3);
    let [alice_feature, bob_feature, alice_decision] =
        [0, 1, 2].map(|i| first["created"]["positions"][i]["id"].as_i64().unwrap());
    let feature = first["created"]["features"][0]["id"].as_i64().unwrap();
    let decision = first["created"]["decisions"][0]["id"].as_i64().unwrap();

    // A duplicate of Bob's from an earlier extraction, as re-extraction used
    // to leave.
    let client = db.indexer().await.unwrap();
    let duplicate: i64 = client
        .query_one(
            "INSERT INTO positions (author_id, thread_id, feature_id, stance, summary, created_at)
             VALUES (2, 1, $1, 'neutral', 'An older duplicate.', now() - interval '1 day') RETURNING id",
            &[&feature],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            "INSERT INTO citations (extract_kind, extract_id, message_id, quote)
             SELECT 'position', $1, id, 'I am not convinced' FROM messages WHERE message_id = 'bob@example.org'",
            &[&duplicate],
        )
        .await
        .unwrap();
    assert_eq!(graph_count(&db, "(:Position)").await, 4);

    let token = token_for(&indexer, 1).await;
    let again = extraction::submit(
        &indexer,
        args(json!({ "lease_token": token, "thread_id": 1, "positions": [
            { "author_id": 1, "feature_slug": "planner-change", "stance": "supports",
              "summary": "Now supports the simpler version.", "citations": cites(alice, "Thread 1 proposes a change") },
            { "author_id": 2, "feature_slug": "planner-change", "stance": "opposes", "summary": "Opposes it.",
              "citations": cites(bob, "I am not convinced") },
            { "author_id": 1, "decision_id": decision, "stance": "proposes", "summary": "Proposed the simplification.",
              "citations": cites(alice, "a change to the planner") },
            { "author_id": 2, "decision_id": decision, "stance": "supports", "summary": "Wants it simpler.",
              "citations": cites(bob, "not convinced the planner change is worth it") }
        ] })),
    )
    .await
    .unwrap();
    assert_eq!(again["accepted"], true, "{again:#}");
    let updated: Vec<i64> = again["updated"]["positions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect();
    assert_eq!(updated, [alice_feature, bob_feature, alice_decision]);
    let bob_decision = again["created"]["positions"][0]["id"].as_i64().unwrap();

    let text = |s: &str| s.to_owned();
    assert_eq!(
        positions(&db).await,
        [
            (
                alice_feature,
                1,
                Some(feature),
                None,
                text("supports"),
                text("Now supports the simpler version."),
                1
            ),
            (
                bob_feature,
                2,
                Some(feature),
                None,
                text("opposes"),
                text("Opposes it."),
                1
            ),
            (
                alice_decision,
                1,
                None,
                Some(decision),
                text("proposes"),
                text("Proposed the simplification."),
                1
            ),
            (
                bob_decision,
                2,
                None,
                Some(decision),
                text("supports"),
                text("Wants it simpler."),
                1
            ),
        ],
        "one position per author on each feature or decision, with the new citations"
    );
    let quote: String = client
        .query_one(
            "SELECT quote FROM citations WHERE extract_kind = 'position' AND extract_id = $1",
            &[&alice_feature],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(quote, "Thread 1 proposes a change");
    assert_eq!(graph_count(&db, "(:Position)").await, 4);
    assert_eq!(
        graph_count(&db, "(:Position)-[:EVIDENCED_BY]->(:Message)").await,
        4,
        "the graph follows the updated and deleted positions and citations"
    );
}

/// Leases thread 1 and submits `positions`, with new features for
/// `features` (slug, name): the reply.
async fn submit_positions(indexer: &Indexer, features: &[(&str, &str)], positions: Value) -> Value {
    let token = token_for(indexer, 1).await;
    let mut new = Vec::new();
    if !features.is_empty() {
        let drafts: Vec<Value> = features
            .iter()
            .map(|(_, name)| json!({ "thread_id": 1, "kind": "feature", "name": name, "description": name }))
            .collect();
        let matched = extraction::match_extracts(
            indexer,
            args(json!({ "lease_token": token, "drafts": drafts })),
        )
        .await
        .unwrap();
        for (i, (slug, name)) in features.iter().enumerate() {
            new.push(json!({ "draft_id": matched["drafts"][i]["draft_id"], "action": "new", "slug": slug,
                             "name": name, "description": name, "status": "proposed",
                             "citations": cites("t1@example.org", "proposes a change to the planner") }));
        }
    }
    let saved = extraction::submit(
        indexer,
        args(json!({ "lease_token": token, "thread_id": 1, "features": new, "positions": positions })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");
    saved
}

fn ids_of(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn two_stances_of_one_author_in_one_submission_stay_apart() {
    let db = corpus(&[(1, Some(10), "pending", false)]).await;
    let indexer = indexer(&db, &[]);
    let alice = "t1@example.org";
    let first = submit_positions(
        &indexer,
        &[("planner-change", "Planner change")],
        json!([
            { "author_id": 1, "feature_slug": "planner-change", "stance": "supports", "summary": "At first.",
              "citations": cites(alice, "proposes a change to the planner") },
            { "author_id": 1, "feature_slug": "planner-change", "stance": "concerned", "summary": "Then.",
              "citations": cites(alice, "Thread 1 proposes a change") }
        ]),
    )
    .await;
    let [earlier, later] = <[i64; 2]>::try_from(ids_of(&first["created"]["positions"])).unwrap();
    assert!(ids_of(&first["updated"]["positions"]).is_empty());
    let feature = first["created"]["features"][0]["id"].as_i64().unwrap();
    let text = |s: &str| s.to_owned();
    assert_eq!(
        positions(&db).await,
        [
            (
                earlier,
                1,
                Some(feature),
                None,
                text("supports"),
                text("At first."),
                1
            ),
            (
                later,
                1,
                Some(feature),
                None,
                text("concerned"),
                text("Then."),
                1
            ),
        ],
        "each keeps its own stance and citations"
    );

    let again = submit_positions(
        &indexer,
        &[],
        json!([{ "author_id": 1, "feature_slug": "planner-change", "stance": "opposes", "summary": "Now.",
                 "citations": cites(alice, "a change to the planner") }]),
    )
    .await;
    assert_eq!(ids_of(&again["updated"]["positions"]), [later]);
    assert_eq!(
        positions(&db).await,
        [(
            later,
            1,
            Some(feature),
            None,
            text("opposes"),
            text("Now."),
            1
        )]
    );
}

#[tokio::test]
async fn a_position_that_a_merge_moved_is_left_for_undo_merge() {
    let db = corpus(&[(1, Some(10), "pending", false)]).await;
    let indexer = indexer(&db, &[]);
    let alice = "t1@example.org";
    // Alice's position on the hints, the newer, is moved next to hers on
    // vacuum by the merge.
    let first = submit_positions(
        &indexer,
        &[("parallel-vacuum", "Parallel vacuum"), ("planner-hints", "Planner hints")],
        json!([
            { "author_id": 1, "feature_slug": "parallel-vacuum", "stance": "supports", "summary": "On vacuum.",
              "citations": cites(alice, "proposes a change to the planner") },
            { "author_id": 1, "feature_slug": "planner-hints", "stance": "proposes", "summary": "On hints.",
              "citations": cites(alice, "Thread 1 proposes a change") }
        ]),
    )
    .await;
    let [on_vacuum, on_hints] =
        <[i64; 2]>::try_from(ids_of(&first["created"]["positions"])).unwrap();
    let [vacuum, hints] = <[i64; 2]>::try_from(ids_of(&first["created"]["features"])).unwrap();
    let merged = extraction::merge_extracts(
        &indexer,
        args(json!({ "kind": "feature", "from_id": hints, "into_id": vacuum, "reason": "The same work." })),
    )
    .await
    .unwrap();

    let again = submit_positions(
        &indexer,
        &[],
        json!([{ "author_id": 1, "feature_slug": "parallel-vacuum", "stance": "opposes", "summary": "Now.",
                 "citations": cites(alice, "a change to the planner") }]),
    )
    .await;
    assert_eq!(
        ids_of(&again["updated"]["positions"]),
        [on_vacuum],
        "the moved position isn't restated: {again:#}"
    );
    let undone = extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    assert_eq!(undone["undone"], true);
    let text = |s: &str| s.to_owned();
    assert_eq!(
        positions(&db).await,
        [
            (
                on_vacuum,
                1,
                Some(vacuum),
                None,
                text("opposes"),
                text("Now."),
                1
            ),
            (
                on_hints,
                1,
                Some(hints),
                None,
                text("proposes"),
                text("On hints."),
                1
            ),
        ],
        "undoing the merge gives the hints their position back as it was"
    );
}
