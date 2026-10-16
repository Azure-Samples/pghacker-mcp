// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Patch series through ingest and through the backfill, on a fresh database.

use pghacker_core::testing::TestDb;
use pghacker_mail::parse::{self, ParsedMessage};
use pghacker_mail::series;
use pghacker_mail::store::{self, StoreContext};
use tokio_postgres::Client;

fn ctx() -> StoreContext {
    StoreContext {
        list: "pgsql-hackers".into(),
        source_id: None,
        archive_month: Some("2025-01".into()),
    }
}

/// A message sent on `date` with patch attachments named `names`, replying
/// to the last of `refs`.
fn message(id: &str, date: &str, subject: &str, refs: &[&str], names: &[&str]) -> ParsedMessage {
    let mut raw = format!(
        "From: Alice <alice@example.org>\nSubject: {subject}\nMessage-ID: <{id}>\n\
         Date: {date} 10:00:00 +0000\n"
    );
    if let Some(parent) = refs.last() {
        let list: Vec<String> = refs.iter().map(|r| format!("<{r}>")).collect();
        raw.push_str(&format!(
            "In-Reply-To: <{parent}>\nReferences: {}\n",
            list.join(" ")
        ));
    }
    raw.push_str(
        "MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"b\"\n\n\
         --b\nContent-Type: text/plain\n\nHere it is.\n",
    );
    for name in names {
        raw.push_str(&format!(
            "--b\nContent-Type: text/x-patch; name=\"{name}\"\n\
             Content-Disposition: attachment; filename=\"{name}\"\n\n\
             diff --git a/src/a.c b/src/a.c\n--- a/src/a.c\n+++ b/src/a.c\n@@ -1 +1 @@\n-a\n+b\n"
        ));
    }
    raw.push_str("--b--\n");
    parse::parse(raw.as_bytes()).unwrap()
}

async fn store_one(client: &mut Client, msg: &ParsedMessage) {
    let tx = client.transaction().await.unwrap();
    let stored = store::upsert_message(&tx, &ctx(), msg).await.unwrap();
    store::refresh_threads(&tx, &[stored.thread_id])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

type Row = (String, Option<String>, Option<i32>);

async fn series_of(client: &Client) -> Vec<Row> {
    client
        .query(
            "SELECT filename, series_version, part FROM patches ORDER BY filename, id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

fn row(name: &str, version: Option<&str>, part: Option<i32>) -> Row {
    (name.to_owned(), version.map(str::to_owned), part)
}

#[tokio::test]
async fn ingest_sets_versions_that_a_thread_s_posts_confirm() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let first = message(
        "a@example.org",
        "1 Jan 2025",
        "[PATCH] psql dp",
        &[],
        &["psql-dp-3.patch", "v2-0001-Other.patch"],
    );
    let later = message(
        "b@example.org",
        "1 Feb 2025",
        "Re: [PATCH] psql dp",
        &["a@example.org"],
        &["psql-dp-4.patch"],
    );
    // The later post arrives first: alone, its number isn't a version.
    store_one(&mut client, &later).await;
    assert_eq!(
        series_of(&client).await,
        [row("psql-dp-4.patch", None, None)]
    );
    // The earlier one makes it a rising series of the name.
    store_one(&mut client, &first).await;
    assert_eq!(
        series_of(&client).await,
        [
            row("psql-dp-3.patch", Some("3"), None),
            row("psql-dp-4.patch", Some("4"), None),
            row("v2-0001-Other.patch", Some("2"), Some(1)),
        ]
    );
}

#[tokio::test]
async fn reparse_corrects_every_thread_once() {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO threads (id, list, root_message_id, subject) VALUES
                 (1, 'pgsql-hackers', 'a@x', 'A'), (2, 'pgsql-hackers', 'c@x', 'B'),
                 (3, 'pgsql-hackers', 'd@x', 'C');
             INSERT INTO messages (id, message_id, list, thread_id, subject, sent_at, has_patch, from_name) VALUES
                 (10, 'a@x', 'pgsql-hackers', 1, 'Fix', '2025-01-01', true, 'A'),
                 (11, 'b@x', 'pgsql-hackers', 1, 'Re: Fix', '2025-02-01', true, 'A'),
                 (20, 'c@x', 'pgsql-hackers', 2, '[PATCH v4] Avoid shifts', '2025-03-01', true, 'B'),
                 (30, 'd@x', 'pgsql-hackers', 3, 'Re: [PATCH v3 2/7] Fix', '2025-03-01', true, 'B');
             INSERT INTO patches (message_id, filename, series_version, part, content) VALUES
                 (10, 'psql-dp-3.patch', NULL, NULL, 'x'),
                 (11, 'psql-dp-4.patch', NULL, NULL, 'x'),
                 (11, 'v20261003-0002-Fix.patch', '20261003', 2, 'x'),
                 (20, 'inline-1', NULL, NULL, 'x'),
                 (30, 'inline-1', '3', 2, 'x');",
        )
        .await
        .unwrap();
    // A thread per batch.
    let (mut after, mut changed, mut runs) = (0, 0, 0);
    loop {
        let batch = series::reparse(&client, after, 1).await.unwrap();
        runs += 1;
        changed += batch.refreshed.changed;
        after = batch.last_thread.unwrap_or(after);
        if !batch.more {
            break;
        }
    }
    assert_eq!((runs, changed), (4, 4));
    assert_eq!(
        series_of(&client).await,
        [
            row("inline-1", Some("4"), None),
            row("inline-1", None, None),
            row("psql-dp-3.patch", Some("3"), None),
            row("psql-dp-4.patch", Some("4"), None),
            row("v20261003-0002-Fix.patch", Some("20261003"), Some(2)),
        ]
    );
    let again = series::reparse(&client, 0, 10).await.unwrap();
    assert_eq!(
        (again.refreshed.threads, again.refreshed.changed, again.more),
        (3, 0, false)
    );
}
