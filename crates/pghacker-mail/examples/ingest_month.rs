// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Ingests monthly archives and seed threads from the real archive into a
//! scratch database, dropped at the end, and reports what was stored and how
//! well it threaded. The archive cache is the repository's `.runtime/cache`.
//!
//! ```text
//! cargo run --release -p pghacker-mail --example ingest_month -- 2026-06 --thread <message-id>
//! ```
//!
//! Without arguments: June 2026 and the SQL/PGQ thread.

use std::time::Instant;

use anyhow::{Context, Result};
use pghacker_core::config::{Env, repo_root};
use pghacker_core::testing::TestDb;
use pghacker_mail::{ArchiveClient, ArchiveConfig, ingest};
use tokio_postgres::Client;

const LIST: &str = "pgsql-hackers";
const SQL_PGQ: &str = "CAExHW5suNPNFcphpabGmJnmF6EmQ2=oJ+KLWAE+o6WWScaSuDQ@mail.gmail.com";

#[tokio::main]
async fn main() -> Result<()> {
    pghacker_core::telemetry::init();
    let mut months = Vec::new();
    let mut threads = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--thread" {
            threads.push(args.next().context("--thread needs a Message-ID")?);
        } else {
            months.push(arg);
        }
    }
    if months.is_empty() && threads.is_empty() {
        months.push("2026-06".to_owned());
        threads.push(SQL_PGQ.to_owned());
    }
    let root = repo_root().context("cannot find the repository")?;
    let env = Env::local()?;
    let client = ArchiveClient::new(ArchiveConfig::from_env(&env, root.join(".runtime/cache"))?)?;
    let db = TestDb::create().await?;
    let pool = db.indexer_pool(4)?;
    let started = Instant::now();

    for month in &months {
        let t = Instant::now();
        let stats = ingest::ingest_mbox_month(&pool, &client, LIST, month, None, false).await?;
        println!(
            "month {month}: {stats:?} in {:.1} s",
            t.elapsed().as_secs_f64()
        );
    }
    for seed in &threads {
        let t = Instant::now();
        let stats = ingest::ingest_thread(&pool, &client, LIST, seed, None).await?;
        println!(
            "thread {seed}: {stats:?} in {:.1} s",
            t.elapsed().as_secs_f64()
        );
    }
    for month in &months {
        let t = Instant::now();
        let stats = ingest::ingest_mbox_month(&pool, &client, LIST, month, None, false).await?;
        println!(
            "again {month}: {stats:?} in {:.1} s",
            t.elapsed().as_secs_f64()
        );
    }
    println!("total {:.1} s\n", started.elapsed().as_secs_f64());

    let indexer = db.indexer().await?;
    report(&indexer, &client, &threads).await
}

async fn scalar(client: &Client, sql: &str) -> Result<i64> {
    Ok(client.query_one(sql, &[]).await?.get(0))
}

async fn report(db: &Client, archive: &ArchiveClient, seeds: &[String]) -> Result<()> {
    for (label, sql) in [
        ("messages", "SELECT count(*) FROM messages"),
        ("threads", "SELECT count(*) FROM threads"),
        ("authors", "SELECT count(*) FROM authors"),
        ("author addresses", "SELECT count(*) FROM author_emails"),
        (
            "messages with patches",
            "SELECT count(*) FROM messages WHERE has_patch",
        ),
        ("patches", "SELECT count(*) FROM patches"),
        (
            "inline patches",
            "SELECT count(*) FROM patches WHERE filename LIKE 'inline-%'",
        ),
        ("chunks", "SELECT count(*) FROM message_chunks"),
        (
            "messages without a date",
            "SELECT count(*) FROM messages WHERE sent_at IS NULL",
        ),
        (
            "messages with an empty body",
            "SELECT count(*) FROM messages WHERE body = ''",
        ),
    ] {
        println!("{label:>28}: {}", scalar(db, sql).await?);
    }
    for row in db
        .query(
            "SELECT kind, count(*), count(DISTINCT value) FROM anchors GROUP BY kind ORDER BY kind",
            &[],
        )
        .await?
    {
        let kind: String = row.get(0);
        println!(
            "{:>28}: {} ({} distinct)",
            format!("anchors {kind}"),
            row.get::<_, i64>(1),
            row.get::<_, i64>(2)
        );
    }
    println!("\nlargest threads:");
    for row in db
        .query(
            "SELECT message_count, participant_count, started_at::date::text, last_at::date::text, subject
             FROM threads ORDER BY message_count DESC, id LIMIT 5",
            &[],
        )
        .await?
    {
        println!(
            "  {:>4} messages, {:>3} participants, {} .. {}  {}",
            row.get::<_, i32>(0),
            row.get::<_, i32>(1),
            row.get::<_, String>(2),
            row.get::<_, String>(3),
            row.get::<_, String>(4)
        );
    }

    println!("\nthreading:");
    for (label, sql) in [
        (
            "single-message threads",
            "SELECT count(*) FROM threads WHERE message_count = 1",
        ),
        (
            "  of which replies (headers)",
            "SELECT count(*) FROM threads t JOIN messages m ON m.thread_id = t.id
             WHERE t.message_count = 1 AND (m.parent_message_id IS NOT NULL)",
        ),
        (
            "  of which 'Re:' without headers",
            "SELECT count(*) FROM threads t JOIN messages m ON m.thread_id = t.id
             WHERE t.message_count = 1 AND m.parent_message_id IS NULL AND m.subject ~* '^\\s*(re|aw|sv|fwd?)\\s*:'",
        ),
        (
            "threads whose root is not stored",
            "SELECT count(*) FROM threads t
             WHERE NOT EXISTS (SELECT FROM messages m WHERE m.message_id = t.root_message_id)",
        ),
        (
            "stored parents in another thread",
            "SELECT count(*) FROM messages m JOIN messages p ON p.message_id = m.parent_message_id
             WHERE p.thread_id <> m.thread_id",
        ),
        (
            "subject keys in several threads",
            "SELECT count(*) FROM (SELECT subject_key FROM threads WHERE subject_key <> ''
             GROUP BY subject_key HAVING count(*) > 1) k",
        ),
    ] {
        println!("{label:>34}: {}", scalar(db, sql).await?);
    }
    println!("\nsubjects in several threads (possible splits):");
    for row in db
        .query(
            "SELECT subject_key, count(*), array_agg(message_count ORDER BY started_at),
                    array_agg(EXISTS (SELECT FROM messages m WHERE m.message_id = t.root_message_id) ORDER BY started_at),
                    array_agg(started_at::date::text ORDER BY started_at)
             FROM threads t WHERE subject_key <> ''
             GROUP BY subject_key HAVING count(*) > 1
             ORDER BY sum(message_count) DESC LIMIT 12",
            &[],
        )
        .await?
    {
        println!(
            "  {:?} x{}: messages {:?}, root stored {:?}, started {:?}",
            row.get::<_, String>(0),
            row.get::<_, i64>(1),
            row.get::<_, Vec<i32>>(2),
            row.get::<_, Vec<bool>>(3),
            row.get::<_, Vec<String>>(4)
        );
    }
    // Which messages share a thread, independent of ingestion order.
    let partition: String = db
        .query_one(
            "SELECT md5(string_agg(ids, ';' ORDER BY ids)) FROM (
                 SELECT string_agg(message_id, ',' ORDER BY message_id) AS ids
                 FROM messages GROUP BY thread_id) t",
            &[],
        )
        .await?
        .get(0);
    println!("\nthread partition fingerprint: {partition}");
    for seed in seeds {
        let listed = archive.thread_message_ids(seed).await?.len();
        let row = db
            .query_one(
                "SELECT t.message_count, t.subject, t.root_message_id
                 FROM messages m JOIN threads t ON t.id = m.thread_id WHERE m.message_id = $1",
                &[seed],
            )
            .await?;
        println!(
            "\nseed thread: {} of {} listed messages in one thread, root {}, {:?}",
            row.get::<_, i32>(0),
            listed,
            row.get::<_, String>(2),
            row.get::<_, String>(1)
        );
    }
    Ok(())
}
