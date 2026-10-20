// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A feature's status follows its most recent evidence, whatever the order
//! in which its threads are extracted, merged or unmerged, on fresh
//! databases, without the network or an embeddings endpoint.

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, extraction};
use serde_json::{Value, json};

/// A database with threads (id, days since their latest message, or None for
/// no date), each with one message by Alice, never extracted.
async fn corpus(threads: &[(i64, Option<u32>)]) -> TestDb {
    let db = TestDb::create().await.unwrap();
    let mut sql = String::from(
        "INSERT INTO authors (id, display_name, message_count) VALUES (1, 'Alice Hacker', 0);\n",
    );
    for &(id, days) in threads {
        let at = days.map_or("NULL".to_owned(), |d| {
            format!("now() - interval '{d} days'")
        });
        sql.push_str(&format!(
            "INSERT INTO threads (id, list, root_message_id, subject, started_at, last_at, message_count,
                                  participant_count)
             VALUES ({id}, 'pgsql-hackers', 't{id}@example.org', 'Thread {id}', {at}, {at}, 1, 1);
             INSERT INTO messages (message_id, list, thread_id, author_id, from_name, subject, sent_at, body, body_clean)
             VALUES ('t{id}@example.org', 'pgsql-hackers', {id}, 1, 'Alice Hacker', 'Thread {id}', {at},
                     'Thread {id} discusses SQL/JSON.', 'Thread {id} discusses SQL/JSON.');\n"
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

fn indexer(db: &TestDb) -> Indexer {
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

/// A feature with no status yet, linked to no thread: its id.
async fn feature(db: &TestDb, slug: &str) -> i64 {
    db.indexer()
        .await
        .unwrap()
        .query_one(
            "INSERT INTO features (slug, name, description, status) VALUES ($1, $1, 'SQL/JSON.', 'unknown')
             RETURNING id",
            &[&slug],
        )
        .await
        .unwrap()
        .get(0)
}

/// Extracts thread `thread` as showing feature `id` with `status`.
async fn claim(indexer: &Indexer, thread: i64, id: i64, status: &str) {
    let batch = extraction::next_batch(indexer, args(json!({ "thread_ids": [thread] })))
        .await
        .unwrap();
    let token = batch["lease_token"].as_str().unwrap().to_owned();
    let matched = extraction::match_extracts(
        indexer,
        args(json!({ "lease_token": token, "drafts": [
            { "thread_id": thread, "kind": "feature", "name": "SQL/JSON", "description": "SQL/JSON." }
        ] })),
    )
    .await
    .unwrap();
    let saved = extraction::submit(
        indexer,
        args(json!({ "lease_token": token, "thread_id": thread, "features": [{
            "draft_id": matched["drafts"][0]["draft_id"], "action": "reuse", "existing_id": id,
            "name": "SQL/JSON", "description": "SQL/JSON.", "status": status,
            "citations": [{ "message_id": format!("t{thread}@example.org"), "quote": "discusses SQL/JSON" }] }] })),
    )
    .await
    .unwrap();
    assert_eq!(saved["accepted"], true, "{saved:#}");
}

/// Feature `id`'s status, and the thread whose latest message dates its
/// evidence (None: no date).
async fn status(db: &TestDb, id: i64) -> (String, Option<i64>) {
    let row = db
        .indexer()
        .await
        .unwrap()
        .query_one(
            "SELECT f.status, (SELECT min(t.id) FROM threads t WHERE t.last_at = f.status_at)
             FROM features f WHERE f.id = $1",
            &[&id],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

fn at(status: &str, thread: i64) -> (String, Option<i64>) {
    (status.to_owned(), Some(thread))
}

/// Every order of `items`.
fn orders<T: Copy>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for (i, &first) in items.iter().enumerate() {
        let mut rest = items.to_vec();
        rest.remove(i);
        for mut order in orders(&rest) {
            order.insert(0, first);
            out.push(order);
        }
    }
    out
}

/// Gives feature `id` each (thread, status) of `order`, in turn.
async fn claim_all(indexer: &Indexer, id: i64, order: &[(i64, &str)]) {
    for &(thread, claimed) in order {
        claim(indexer, thread, id, claimed).await;
    }
}

#[tokio::test]
async fn the_status_follows_the_most_recent_evidence_in_any_order() {
    // SQL/JSON: committed for 15 (thread 4), reverted (thread 3), proposed
    // again (thread 2), committed again (thread 1, the newest).
    let db = corpus(&[
        (1, Some(100)),
        (2, Some(500)),
        (3, Some(1000)),
        (4, Some(1100)),
    ])
    .await;
    let indexer = indexer(&db);
    let history = [
        (4, "committed"),
        (3, "reverted"),
        (2, "proposed"),
        (1, "committed"),
    ];
    for (n, order) in orders(&history).into_iter().enumerate() {
        let id = feature(&db, &format!("sql-json-{n}")).await;
        claim_all(&indexer, id, &order).await;
        assert_eq!(status(&db, id).await, at("committed", 1), "{order:?}");
    }
    for (n, order) in orders(&history[..3]).into_iter().enumerate() {
        let id = feature(&db, &format!("revived-{n}")).await;
        claim_all(&indexer, id, &order).await;
        assert_eq!(status(&db, id).await, at("proposed", 2), "{order:?}");
    }
    for follow_up in ["proposed", "in_progress"] {
        for (n, order) in orders(&[(3, "committed"), (1, follow_up)])
            .into_iter()
            .enumerate()
        {
            let id = feature(&db, &format!("{follow_up}-after-commit-{n}")).await;
            claim_all(&indexer, id, &order).await;
            assert_eq!(status(&db, id).await, at("committed", 3), "{order:?}");
        }
    }

    // Each step, newest first as the backfill reads, then oldest first.
    let id = feature(&db, "newest-first").await;
    claim(&indexer, 1, id, "committed").await;
    claim(&indexer, 3, id, "reverted").await;
    assert_eq!(
        status(&db, id).await,
        at("committed", 1),
        "an older thread's revert doesn't undo a newer commit"
    );
    let id = feature(&db, "oldest-first").await;
    claim(&indexer, 3, id, "reverted").await;
    assert_eq!(status(&db, id).await, at("reverted", 3));
    claim(&indexer, 1, id, "committed").await;
    assert_eq!(
        status(&db, id).await,
        at("committed", 1),
        "a newer commit leaves a revert"
    );
}

#[tokio::test]
async fn a_rejected_feature_that_is_revived_takes_its_new_status() {
    let db = corpus(&[(1, Some(100)), (2, Some(1000)), (3, Some(2000))]).await;
    let indexer = indexer(&db);
    for order in orders(&[(2, "rejected"), (1, "committed")]) {
        let id = feature(&db, &format!("committed-{}", order[0].0)).await;
        for &(thread, claimed) in &order {
            claim(&indexer, thread, id, claimed).await;
        }
        assert_eq!(status(&db, id).await, at("committed", 1), "{order:?}");
    }
    for order in orders(&[(3, "rejected"), (2, "proposed"), (1, "unknown")]) {
        let id = feature(&db, &format!("proposed-{}-{}", order[0].0, order[1].0)).await;
        for &(thread, claimed) in &order {
            claim(&indexer, thread, id, claimed).await;
        }
        assert_eq!(
            status(&db, id).await,
            at("proposed", 2),
            "unknown is no evidence: {order:?}"
        );
    }
}

#[tokio::test]
async fn evidence_of_the_same_date_or_none_keeps_the_more_advanced_status() {
    let db = corpus(&[(1, Some(100)), (2, None)]).await;
    let indexer = indexer(&db);
    let id = feature(&db, "same-thread").await;
    claim(&indexer, 1, id, "committed").await;
    claim(&indexer, 1, id, "proposed").await;
    assert_eq!(status(&db, id).await, at("committed", 1));
    claim(&indexer, 1, id, "reverted").await;
    assert_eq!(status(&db, id).await, at("reverted", 1));

    let id = feature(&db, "undated").await;
    claim(&indexer, 2, id, "proposed").await;
    assert_eq!(status(&db, id).await, ("proposed".to_owned(), None));
    claim(&indexer, 2, id, "rejected").await;
    assert_eq!(status(&db, id).await, ("rejected".to_owned(), None));
    claim(&indexer, 1, id, "in_progress").await;
    assert_eq!(
        status(&db, id).await,
        at("in_progress", 1),
        "dated evidence is more recent than undated"
    );
    claim(&indexer, 2, id, "committed").await;
    assert_eq!(status(&db, id).await, at("in_progress", 1));
}

#[tokio::test]
async fn a_proposal_never_uncommits_a_feature_but_revives_others() {
    let db = corpus(&[(1, Some(100)), (2, Some(1000)), (3, Some(2000))]).await;
    let indexer = indexer(&db);
    for (n, follow_up) in ["proposed", "in_progress", "unknown"]
        .into_iter()
        .enumerate()
    {
        let id = feature(&db, &format!("committed-{n}")).await;
        claim(&indexer, 2, id, "committed").await;
        claim(&indexer, 1, id, follow_up).await;
        assert_eq!(
            status(&db, id).await,
            at("committed", 2),
            "a newer {follow_up} thread leaves the commit and its date"
        );
    }
    let id = feature(&db, "committed-then-reverted").await;
    claim(&indexer, 2, id, "committed").await;
    claim(&indexer, 1, id, "reverted").await;
    assert_eq!(status(&db, id).await, at("reverted", 1));

    for (n, ended) in ["reverted", "rejected", "returned", "withdrawn"]
        .into_iter()
        .enumerate()
    {
        let id = feature(&db, &format!("revived-{n}")).await;
        claim(&indexer, 3, id, ended).await;
        claim(&indexer, 2, id, "proposed").await;
        assert_eq!(status(&db, id).await, at("proposed", 2), "{ended}");
        claim(&indexer, 1, id, "committed").await;
        assert_eq!(status(&db, id).await, at("committed", 1), "{ended}");
    }

    // The same in a merge, and its undo.
    let committed = feature(&db, "merged-committed").await;
    claim(&indexer, 2, committed, "committed").await;
    let proposed = feature(&db, "merged-proposed").await;
    claim(&indexer, 1, proposed, "proposed").await;
    let merged = extraction::merge_extracts(
        &indexer,
        args(json!({ "kind": "feature", "from_id": proposed, "into_id": committed, "reason": "The same feature." })),
    )
    .await
    .unwrap();
    assert!(merged["moved"].get("status").is_none(), "{merged:#}");
    assert_eq!(status(&db, committed).await, at("committed", 2));
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    let merged = extraction::merge_extracts(
        &indexer,
        args(json!({ "kind": "feature", "from_id": committed, "into_id": proposed, "reason": "The same feature." })),
    )
    .await
    .unwrap();
    assert_eq!(
        status(&db, proposed).await,
        at("committed", 2),
        "the merged feature folds both's evidence"
    );
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    assert_eq!(status(&db, proposed).await, at("proposed", 1));
    assert_eq!(status(&db, committed).await, at("committed", 2));
}

#[tokio::test]
async fn a_merge_takes_the_more_recent_status_and_its_undo_restores_it() {
    let db = corpus(&[(1, Some(100)), (2, Some(1000))]).await;
    let indexer = indexer(&db);
    let old = feature(&db, "reverted-feature").await;
    claim(&indexer, 2, old, "reverted").await;
    let new = feature(&db, "committed-feature").await;
    claim(&indexer, 1, new, "committed").await;

    let merge = |from: i64, into: i64| {
        let indexer = &indexer;
        async move {
            extraction::merge_extracts(
                indexer,
                args(json!({ "kind": "feature", "from_id": from, "into_id": into, "reason": "The same feature." })),
            )
            .await
            .unwrap()
        }
    };
    let undo = |merged: Value| {
        let indexer = &indexer;
        async move {
            extraction::undo_merge(indexer, args(json!({ "merge_id": merged["merge_id"] })))
                .await
                .unwrap();
        }
    };

    let merged = merge(old, new).await;
    assert!(merged["moved"].get("status").is_none(), "{merged:#}");
    assert_eq!(status(&db, new).await, at("committed", 1));
    undo(merged).await;

    let merged = merge(new, old).await;
    assert_eq!(status(&db, old).await, at("committed", 1));
    undo(merged).await;
    assert_eq!(status(&db, old).await, at("reverted", 2));
    assert_eq!(status(&db, new).await, at("committed", 1));

    // Evidence an extraction gave since the merge counts; on a link the
    // merge added, undoing the merge takes it away with the link.
    let merged = merge(new, old).await;
    claim(&indexer, 1, old, "reverted").await;
    assert_eq!(status(&db, old).await, at("reverted", 1));
    undo(merged).await;
    assert_eq!(status(&db, old).await, at("reverted", 2));
    assert_eq!(status(&db, new).await, at("committed", 1));
}

#[tokio::test]
async fn statuses_set_before_take_their_features_most_recent_thread() {
    let db = corpus(&[(1, Some(300)), (2, Some(30))]).await;
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO features (id, slug, name, description, status) VALUES
                 (1, 'linked', 'Linked', 'A change.', 'reverted'),
                 (2, 'unknown', 'Unknown', 'A change.', 'unknown'),
                 (3, 'unlinked', 'Unlinked', 'A change.', 'committed');
             INSERT INTO feature_threads (feature_id, thread_id) VALUES (1, 1), (1, 2), (2, 2);
             ALTER TABLE features DROP COLUMN status_at;
             ALTER TABLE feature_threads DROP COLUMN status, DROP COLUMN status_at;",
        )
        .await
        .unwrap();
    client
        .batch_execute(include_str!(
            "../../../migrations/0027_feature_status_at.sql"
        ))
        .await
        .unwrap();
    assert_eq!(status(&db, 1).await, at("reverted", 2));
    assert_eq!(status(&db, 2).await, ("unknown".to_owned(), None));
    assert_eq!(status(&db, 3).await, ("committed".to_owned(), None));

    // A newer thread's commit then replaces the revert; an older one's doesn't.
    let db = corpus(&[(1, Some(300)), (2, Some(30)), (3, Some(10))]).await;
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO features (id, slug, name, description, status) VALUES (1, 'linked', 'Linked', 'A change.', 'reverted');
             INSERT INTO feature_threads (feature_id, thread_id) VALUES (1, 1), (1, 2);
             ALTER TABLE features DROP COLUMN status_at;
             ALTER TABLE feature_threads DROP COLUMN status, DROP COLUMN status_at;",
        )
        .await
        .unwrap();
    client
        .batch_execute(include_str!(
            "../../../migrations/0027_feature_status_at.sql"
        ))
        .await
        .unwrap();
    let indexer = indexer(&db);
    claim(&indexer, 1, 1, "committed").await;
    assert_eq!(status(&db, 1).await, at("reverted", 2));
    claim(&indexer, 3, 1, "committed").await;
    assert_eq!(status(&db, 1).await, at("committed", 3));

    // A status without evidence recorded counts as its own when evidence
    // comes.
    client
        .batch_execute(
            "INSERT INTO features (id, slug, name, description, status) VALUES (2, 'legacy', 'Legacy', 'A change.', 'committed');
             INSERT INTO feature_threads (feature_id, thread_id) VALUES (2, 2);",
        )
        .await
        .unwrap();
    claim(&indexer, 3, 2, "proposed").await;
    assert_eq!(status(&db, 2).await, ("committed".to_owned(), None));
    claim(&indexer, 3, 2, "reverted").await;
    assert_eq!(status(&db, 2).await, at("reverted", 3));
}
