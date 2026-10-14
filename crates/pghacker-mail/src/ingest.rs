//! The ingestion pipelines the indexer's job workers run: a month's mbox, or
//! a thread from its flat page and raw messages. Both are idempotent and
//! commit in batches, so an interrupted run resumes where it stopped. So are
//! the restores that give mail stored before ingest kept email addresses its
//! addresses back, from the same months and raw messages.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pghacker_core::db::Pool;
use serde::Serialize;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;

use crate::client::{ArchiveClient, Mbox};
use crate::mbox;
use crate::parse::{self, ParsedMessage, normalize_message_id};
use crate::store::{self, StoreContext, Stored};

/// Messages per transaction.
const BATCH: usize = 100;
/// Raw messages retrieved per transaction when ingesting a thread.
const THREAD_BATCH: usize = 25;

/// What an ingestion did.
#[derive(Debug, Default, Clone, Serialize)]
pub struct IngestStats {
    /// Messages in the mbox, or raw messages retrieved for a thread.
    pub fetched: usize,
    /// Messages parsed (those already stored are skipped before parsing).
    pub parsed: usize,
    pub inserted: usize,
    /// Messages that were already stored.
    pub existing: usize,
    /// Messages that could not be retrieved, parsed or stored.
    pub failed: usize,
    /// Threads that gained messages.
    pub threads_touched: usize,
    /// The month hasn't changed since it was ingested in full.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}

/// Retrieves `list`'s mbox for `month` (YYYY-MM; again if `refresh` and the
/// month is not complete) and stores the messages not stored yet. Once all
/// are stored, the archive client records it, so that an unchanged month
/// needn't be retrieved again.
pub async fn ingest_mbox_month(
    pool: &Pool,
    client: &ArchiveClient,
    list: &str,
    month: &str,
    source_id: Option<i64>,
    refresh: bool,
) -> Result<IngestStats> {
    let started = Instant::now();
    let data = match client.mbox_month(list, month, refresh).await? {
        Mbox::Data(data) => Arc::new(data),
        Mbox::Unchanged => {
            tracing::debug!(%list, %month, "the month hasn't changed since it was ingested");
            return Ok(IngestStats {
                unchanged: true,
                ..Default::default()
            });
        }
    };
    let ranges = {
        let data = data.clone();
        tokio::task::spawn_blocking(move || mbox::ranges(&data)).await?
    };
    let ctx = StoreContext {
        list: list.to_owned(),
        source_id,
        archive_month: Some(month.to_owned()),
    };
    let mut stats = IngestStats {
        fetched: ranges.len(),
        ..Default::default()
    };
    let mut touched = BTreeSet::new();
    for batch in ranges.chunks(BATCH) {
        let batch = batch.to_vec();
        let ids = {
            let data = data.clone();
            let batch = batch.clone();
            tokio::task::spawn_blocking(move || {
                batch
                    .iter()
                    .map(|range| parse::header_message_id(&data[range.clone()]))
                    .collect::<Vec<_>>()
            })
            .await?
        };
        let known = stored_ids(pool, ids.iter().flatten().cloned().collect()).await?;
        let mut todo = Vec::new();
        for (range, id) in batch.into_iter().zip(ids) {
            match id {
                Some(id) if known.contains(&id) => stats.existing += 1,
                _ => todo.push(range),
            }
        }
        if todo.is_empty() {
            continue;
        }
        let parsed = {
            let data = data.clone();
            tokio::task::spawn_blocking(move || {
                todo.into_iter()
                    .map(|range| parse::parse(&data[range]))
                    .collect::<Vec<_>>()
            })
            .await?
        };
        let mut messages = Vec::with_capacity(parsed.len());
        for result in parsed {
            match result {
                Ok(message) => {
                    stats.parsed += 1;
                    messages.push(message);
                }
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(%month, error = %format!("{error:#}"), "cannot parse a message");
                }
            }
        }
        store_batch(pool, &ctx, &messages, &mut stats, &mut touched).await?;
    }
    stats.threads_touched = touched.len();
    if stats.failed == 0 {
        client.mark_mbox_ingested(list, month).await?;
    }
    tracing::info!(
        %list,
        %month,
        ?stats,
        seconds = started.elapsed().as_secs_f64(),
        "ingested a month"
    );
    Ok(stats)
}

/// Stores the thread that contains `seed_message_id`: lists its messages
/// from the flat thread page, then retrieves and stores those not stored yet.
pub async fn ingest_thread(
    pool: &Pool,
    client: &ArchiveClient,
    list: &str,
    seed_message_id: &str,
    source_id: Option<i64>,
) -> Result<IngestStats> {
    let started = Instant::now();
    let seed = normalize_message_id(seed_message_id);
    let mut ids = client
        .thread_message_ids(&seed)
        .await
        .with_context(|| format!("cannot list the thread of {seed}"))?;
    if !ids.contains(&seed) {
        ids.push(seed.clone());
    }
    let known = stored_ids(pool, ids.clone()).await?;
    let ctx = StoreContext {
        list: list.to_owned(),
        source_id,
        archive_month: None,
    };
    let mut stats = IngestStats {
        existing: ids.iter().filter(|id| known.contains(*id)).count(),
        ..Default::default()
    };
    let missing: Vec<String> = ids.into_iter().filter(|id| !known.contains(id)).collect();
    let mut touched = BTreeSet::new();
    for batch in missing.chunks(THREAD_BATCH) {
        let mut raws = Vec::with_capacity(batch.len());
        for id in batch {
            match client.raw_message(id).await {
                Ok(raw) => {
                    stats.fetched += 1;
                    raws.push((id.clone(), raw));
                }
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(message_id = %id, error = %format!("{error:#}"), "cannot retrieve a message");
                }
            }
        }
        let parsed = tokio::task::spawn_blocking(move || {
            raws.into_iter()
                .map(|(id, raw)| (id, parse::parse(&raw)))
                .collect::<Vec<_>>()
        })
        .await?;
        let mut messages = Vec::with_capacity(parsed.len());
        for (id, result) in parsed {
            match result {
                Ok(message) => {
                    stats.parsed += 1;
                    if message.message_id != id {
                        tracing::debug!(requested = %id, parsed = %message.message_id, "the archive's Message-ID differs from the header's");
                    }
                    messages.push(message);
                }
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(message_id = %id, error = %format!("{error:#}"), "cannot parse a message");
                }
            }
        }
        store_batch(pool, &ctx, &messages, &mut stats, &mut touched).await?;
    }
    stats.threads_touched = touched.len();
    tracing::info!(
        %list,
        seed = %seed,
        ?stats,
        seconds = started.elapsed().as_secs_f64(),
        "ingested a thread"
    );
    Ok(stats)
}

/// Which of `ids` are stored.
async fn stored_ids(pool: &Pool, ids: Vec<String>) -> Result<HashSet<String>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let client = pool
        .get()
        .await
        .context("cannot get a database connection")?;
    let rows = client
        .query_typed(
            "SELECT message_id FROM messages WHERE message_id = ANY($1)",
            &[(&ids, Type::TEXT_ARRAY)],
        )
        .await?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

/// Stores messages in one transaction and refreshes the threads that gained
/// some. A message that fails is counted and skipped; one that deadlocked
/// with a concurrent ingestion is tried again after the commit.
async fn store_batch(
    pool: &Pool,
    ctx: &StoreContext,
    messages: &[ParsedMessage],
    stats: &mut IngestStats,
    touched: &mut BTreeSet<i64>,
) -> Result<()> {
    let mut pending: Vec<&ParsedMessage> = messages.iter().collect();
    let mut round = 0;
    while !pending.is_empty() {
        round += 1;
        let mut client = pool
            .get()
            .await
            .context("cannot get a database connection")?;
        let tx = client.transaction().await?;
        let mut batch_touched = BTreeSet::new();
        let mut deferred = Vec::new();
        for message in pending {
            match store::upsert_message(&tx, ctx, message).await {
                Ok(Stored {
                    inserted: true,
                    thread_id,
                    ..
                }) => {
                    stats.inserted += 1;
                    batch_touched.insert(thread_id);
                }
                Ok(_) => stats.existing += 1,
                Err(error) if round < RETRY_ROUNDS && is_transient(&error) => {
                    deferred.push(message)
                }
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(
                        message_id = %message.message_id,
                        error = %format!("{error:#}"),
                        "cannot store a message"
                    );
                }
            }
        }
        let ids: Vec<i64> = batch_touched.iter().copied().collect();
        refresh(&tx, &ids).await?;
        tx.commit().await?;
        touched.extend(batch_touched);
        pending = deferred;
        if !pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(100 * round as u64)).await;
        }
    }
    Ok(())
}

/// Rounds of [`store_batch`] for messages that deadlocked.
const RETRY_ROUNDS: usize = 3;

/// [`store::refresh_threads`], retried if it deadlocks; if it still fails,
/// the batch commits without it: the counts that inserts maintain stay
/// right, and the threads' next refresh fixes the rest.
async fn refresh(tx: &tokio_postgres::Transaction<'_>, ids: &[i64]) -> Result<()> {
    for attempt in 1..=RETRY_ROUNDS {
        tx.batch_execute("SAVEPOINT pghacker_refresh_threads")
            .await?;
        match store::refresh_threads(tx, ids).await {
            Ok(()) => {
                tx.batch_execute("RELEASE SAVEPOINT pghacker_refresh_threads")
                    .await?;
                return Ok(());
            }
            Err(error) => {
                tx.batch_execute(
                    "ROLLBACK TO SAVEPOINT pghacker_refresh_threads; \
                     RELEASE SAVEPOINT pghacker_refresh_threads",
                )
                .await?;
                if attempt == RETRY_ROUNDS || !is_transient(&error) {
                    tracing::warn!(error = %format!("{error:#}"), "cannot refresh threads");
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
            }
        }
    }
    Ok(())
}

fn is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            .is_some_and(|code| {
                *code == SqlState::T_R_DEADLOCK_DETECTED
                    || *code == SqlState::T_R_SERIALIZATION_FAILURE
            })
    })
}

/// What a restore of email addresses did.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RestoreStats {
    /// Messages in the mbox, or raw messages retrieved.
    pub fetched: usize,
    /// Of those, the stored ones, compared with what's stored.
    pub stored: usize,
    /// Stored messages that got their addresses back.
    pub restored: usize,
    /// Stored messages whose text isn't what ingest once made of the
    /// archive's, left as they are.
    pub differs: usize,
    /// Messages that could not be retrieved, parsed or restored.
    pub failed: usize,
}

/// Gives the stored messages of `list`'s mbox for `month` the email
/// addresses that ingest once replaced ([`store::restore_addresses`]), from
/// the month as the archive or the cache has it.
pub async fn restore_mbox_month(
    pool: &Pool,
    client: &ArchiveClient,
    list: &str,
    month: &str,
) -> Result<RestoreStats> {
    let started = Instant::now();
    let data = Arc::new(client.mbox_month_again(list, month).await?);
    let ranges = {
        let data = data.clone();
        tokio::task::spawn_blocking(move || mbox::ranges(&data)).await?
    };
    let mut stats = RestoreStats {
        fetched: ranges.len(),
        ..Default::default()
    };
    for batch in ranges.chunks(BATCH) {
        let batch = batch.to_vec();
        let ids = {
            let data = data.clone();
            let batch = batch.clone();
            tokio::task::spawn_blocking(move || {
                batch
                    .iter()
                    .map(|range| parse::header_message_id(&data[range.clone()]))
                    .collect::<Vec<_>>()
            })
            .await?
        };
        let known = stored_ids(pool, ids.iter().flatten().cloned().collect()).await?;
        let todo: Vec<_> = batch
            .into_iter()
            .zip(ids)
            .filter(|(_, id)| id.as_ref().is_some_and(|id| known.contains(id)))
            .map(|(range, _)| range)
            .collect();
        stats.stored += todo.len();
        let parsed = {
            let data = data.clone();
            tokio::task::spawn_blocking(move || {
                todo.into_iter()
                    .map(|range| parse::parse(&data[range]))
                    .collect::<Vec<_>>()
            })
            .await?
        };
        let mut messages = Vec::with_capacity(parsed.len());
        for result in parsed {
            match result {
                Ok(message) => messages.push(message),
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(%month, error = %format!("{error:#}"), "cannot parse a message");
                }
            }
        }
        restore_batch(pool, &messages, &mut stats).await?;
    }
    tracing::info!(
        %list,
        %month,
        ?stats,
        seconds = started.elapsed().as_secs_f64(),
        "restored a month's email addresses"
    );
    Ok(stats)
}

/// Gives stored messages the email addresses that ingest once replaced
/// ([`store::restore_addresses`]), each retrieved raw from the archive or
/// the cache: those that came from thread pages rather than a month.
pub async fn restore_messages(
    pool: &Pool,
    client: &ArchiveClient,
    message_ids: &[String],
) -> Result<RestoreStats> {
    let started = Instant::now();
    let mut stats = RestoreStats::default();
    for batch in message_ids.chunks(THREAD_BATCH) {
        let known = stored_ids(pool, batch.to_vec()).await?;
        let mut raws = Vec::with_capacity(batch.len());
        for id in batch.iter().filter(|id| known.contains(*id)) {
            match client.raw_message(id).await {
                Ok(raw) => {
                    stats.fetched += 1;
                    raws.push(raw);
                }
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(message_id = %id, error = %format!("{error:#}"), "cannot retrieve a message");
                }
            }
        }
        stats.stored += raws.len();
        let parsed = tokio::task::spawn_blocking(move || {
            raws.iter().map(|raw| parse::parse(raw)).collect::<Vec<_>>()
        })
        .await?;
        let mut messages = Vec::with_capacity(parsed.len());
        for result in parsed {
            match result {
                Ok(message) => messages.push(message),
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(error = %format!("{error:#}"), "cannot parse a message");
                }
            }
        }
        restore_batch(pool, &messages, &mut stats).await?;
    }
    tracing::info!(
        messages = message_ids.len(),
        ?stats,
        seconds = started.elapsed().as_secs_f64(),
        "restored messages' email addresses"
    );
    Ok(stats)
}

/// Restores messages in one transaction and refreshes the threads whose
/// subjects may have changed. A message that fails is counted and skipped.
async fn restore_batch(
    pool: &Pool,
    messages: &[ParsedMessage],
    stats: &mut RestoreStats,
) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let mut client = pool
        .get()
        .await
        .context("cannot get a database connection")?;
    let tx = client.transaction().await?;
    let mut threads = BTreeSet::new();
    for message in messages {
        match store::restore_addresses(&tx, message).await {
            Ok(store::Restored::Restored { thread_id }) => {
                stats.restored += 1;
                threads.insert(thread_id);
            }
            Ok(store::Restored::Differs) => stats.differs += 1,
            Ok(store::Restored::Unchanged) => {}
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(
                    message_id = %message.message_id,
                    error = %format!("{error:#}"),
                    "cannot restore a message's email addresses"
                );
            }
        }
    }
    let ids: Vec<i64> = threads.into_iter().collect();
    refresh(&tx, &ids).await?;
    tx.commit().await?;
    Ok(())
}
