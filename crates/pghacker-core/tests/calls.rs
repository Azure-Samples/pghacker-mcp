// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Calls that can be abandoned, on a real database: an abandoned call's
//! statement is cancelled and its connection closed rather than returned
//! busy; a call that finishes gives its connection back; a pool waits a
//! bounded time for a connection; and a pool's startup options hold on its
//! connections.

use std::time::{Duration, Instant};

use pghacker_core::calls::{CallPool, abandonable};
use pghacker_core::db;
use pghacker_core::testing::TestDb;

const APP: &str = "pghacker-test calls";

/// The pool's connections running pg_sleep.
async fn sleeping(admin: &tokio_postgres::Client) -> i64 {
    admin
        .query_one(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND application_name = $1
               AND state = 'active' AND query LIKE 'SELECT pg_sleep%'",
            &[&APP],
        )
        .await
        .unwrap()
        .get(0)
}

/// Waits up to `within` for `sleeping` to be `count`.
async fn until_sleeping(admin: &tokio_postgres::Client, count: i64, within: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < within {
        if sleeping(admin).await == count {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn an_abandoned_calls_statement_is_cancelled_and_its_connection_closed() {
    let db = TestDb::create().await.unwrap();
    let admin = db.admin().await.unwrap();
    let pool = CallPool::new(
        db::pool(&db.indexer_url, 2, APP).unwrap(),
        Duration::from_secs(5),
    );

    let call = tokio::spawn(abandonable({
        let pool = pool.clone();
        async move {
            let client = pool.get().await.unwrap();
            client.execute("SELECT pg_sleep(30)", &[]).await
        }
    }));
    assert!(until_sleeping(&admin, 1, Duration::from_secs(5)).await);
    call.abort();
    assert!(
        until_sleeping(&admin, 0, Duration::from_secs(2)).await,
        "the abandoned call's statement still runs"
    );
    assert_eq!(
        pool.pool().status().size,
        0,
        "its connection went back to the pool"
    );
    let client = pool.get().await.unwrap();
    let started = Instant::now();
    client.execute("SELECT 1", &[]).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn a_call_that_finishes_gives_its_connection_back() {
    let db = TestDb::create().await.unwrap();
    let pool = CallPool::new(
        db::pool(&db.indexer_url, 2, APP).unwrap(),
        Duration::from_secs(5),
    );
    let one: i32 = abandonable({
        let pool = pool.clone();
        async move {
            let client = pool.get().await.unwrap();
            client.query_one("SELECT 1", &[]).await.unwrap().get(0)
        }
    })
    .await;
    assert_eq!(one, 1);
    let status = pool.pool().status();
    assert_eq!((status.size, status.available), (1, 1));
}

#[tokio::test]
async fn a_busy_pool_answers_within_its_wait_and_options_hold() {
    let db = TestDb::create().await.unwrap();
    let pool = CallPool::new(
        db::pool_with_options(&db.indexer_url, 1, APP, "-c statement_timeout=1500").unwrap(),
        Duration::from_millis(200),
    );
    let held = pool.get().await.unwrap();
    let timeout: String = held
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(timeout, "1500ms");
    let started = Instant::now();
    let Err(error) = pool.get().await else {
        panic!("a second connection from a pool of one");
    };
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(error.to_string().contains("busy"), "{error}");
    let error = held.execute("SELECT pg_sleep(5)", &[]).await.unwrap_err();
    assert_eq!(
        error.code(),
        Some(&tokio_postgres::error::SqlState::QUERY_CANCELED),
        "{error:?}"
    );
}
