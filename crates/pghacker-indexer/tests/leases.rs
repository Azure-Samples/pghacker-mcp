// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Extraction leases and their drafts: ended leases are purged with their
//! drafts, and a draft doesn't keep ingest from merging its thread.

use pghacker_core::testing::TestDb;
use pghacker_indexer::extraction;
use pghacker_mail::parse;
use pghacker_mail::store::{self, StoreContext};
use tokio_postgres::Client;
use uuid::Uuid;

/// A lease of `threads` that `ends` (an interval from now: past or future),
/// released then too if `released`, with a draft for each thread.
async fn lease(client: &Client, threads: &[i64], ends: &str, released: bool) -> Uuid {
    let token = Uuid::new_v4();
    client
        .execute(
            &format!(
                "INSERT INTO extraction_leases (token, thread_ids, leased_at, expires_at, released_at)
                 VALUES ($1, $2, now() + interval '{ends}' - interval '20 minutes', now() + interval '{ends}',
                         CASE WHEN $3 THEN now() + interval '{ends}' END)"
            ),
            &[&token, &threads, &released],
        )
        .await
        .unwrap();
    for thread in threads {
        client
            .execute(
                "INSERT INTO drafts (lease_token, thread_id, kind, draft) VALUES ($1, $2, 'feature', '{}')",
                &[&token, thread],
            )
            .await
            .unwrap();
    }
    token
}

async fn count(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

#[tokio::test]
async fn ended_leases_are_purged_with_their_drafts() {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO threads (id, list, root_message_id, subject) VALUES
                 (1, 'pgsql-hackers', 'a@x', 'A'), (2, 'pgsql-hackers', 'b@x', 'B')",
        )
        .await
        .unwrap();
    let live = lease(&client, &[1, 2], "10 minutes", false).await;
    let ended_lately = lease(&client, &[1], "-2 hours", false).await;
    let released_lately = lease(&client, &[2], "-2 hours", true).await;
    lease(&client, &[1, 2], "-2 days", false).await;
    lease(&client, &[1], "-3 days", true).await;

    let purged = extraction::purge_leases(&client, 1).await.unwrap();
    assert_eq!((purged.leases, purged.more), (1, true));
    let purged = extraction::purge_leases(&client, 100).await.unwrap();
    assert_eq!((purged.leases, purged.more), (1, false));
    assert_eq!(
        extraction::purge_leases(&client, 100).await.unwrap().leases,
        0
    );
    let kept: Vec<Uuid> = client
        .query(
            "SELECT token FROM extraction_leases ORDER BY expires_at DESC",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(kept.len(), 3);
    assert_eq!(kept[0], live);
    assert!(kept.contains(&ended_lately) && kept.contains(&released_lately));
    // The kept leases keep their drafts; the purged leases' are gone.
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM drafts d WHERE EXISTS (
                 SELECT FROM extraction_leases l WHERE l.token = d.lease_token)"
        )
        .await,
        4
    );
    assert_eq!(count(&client, "SELECT count(*) FROM drafts").await, 4);
}

#[tokio::test]
async fn the_purge_job_is_recurring() {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    pghacker_indexer::jobs::ensure_recurring(&client)
        .await
        .unwrap();
    assert_eq!(
        count(
            &client,
            "SELECT count(*) FROM jobs WHERE kind = 'purge_leases' AND key = 'recurring' AND state = 'queued'"
        )
        .await,
        1
    );
    assert!(
        pghacker_indexer::jobs::lane(0, 2)
            .unwrap()
            .contains(&pghacker_indexer::jobs::kind::PURGE_LEASES)
    );
}

fn ctx() -> StoreContext {
    StoreContext {
        list: "pgsql-hackers".into(),
        source_id: None,
        archive_month: Some("2026-06".into()),
    }
}

fn message(id: &str, subject: &str, headers: &str) -> parse::ParsedMessage {
    let raw = format!(
        "From: Alice Example <alice@example.org>\nSubject: {subject}\nMessage-ID: <{id}>\n{headers}\nText of {id}.\n"
    );
    parse::parse(raw.as_bytes()).unwrap()
}

async fn store_one(client: &mut Client, msg: &parse::ParsedMessage) -> i64 {
    let tx = client.transaction().await.unwrap();
    let stored = store::upsert_message(&tx, &ctx(), msg).await.unwrap();
    store::refresh_threads(&tx, &[stored.thread_id])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    stored.thread_id
}

#[tokio::test]
async fn a_draft_does_not_keep_ingest_from_merging_its_thread() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    // A reply whose parent hasn't arrived starts a thread of its own...
    let a = message(
        "a@example.org",
        "Topic",
        "Date: 1 Jun 2026 10:00:00 +0000\n",
    );
    let e = message(
        "e@example.org",
        "Re: Topic",
        "Date: 5 Jun 2026 10:00:00 +0000\nIn-Reply-To: <y@example.org>\n",
    );
    let thread_a = store_one(&mut client, &a).await;
    let thread_e = store_one(&mut client, &e).await;
    assert_ne!(thread_a, thread_e);
    // ...which a lease drafted from, long ago.
    lease(&client, &[thread_e], "-2 days", false).await;
    // The parent arrives: the reply's thread merges into the topic's.
    let y = message(
        "y@example.org",
        "Re: Topic",
        "Date: 4 Jun 2026 10:00:00 +0000\nIn-Reply-To: <a@example.org>\nReferences: <a@example.org>\n",
    );
    assert_eq!(store_one(&mut client, &y).await, thread_a);
    assert_eq!(
        count(
            &client,
            &format!("SELECT count(*) FROM messages WHERE thread_id <> {thread_a}")
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &client,
            &format!("SELECT count(*) FROM threads WHERE id = {thread_e}")
        )
        .await,
        0
    );
    assert_eq!(count(&client, "SELECT count(*) FROM drafts").await, 0);
}
