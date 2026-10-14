//! The job queue: idempotent jobs keyed by (kind, key), claimed with
//! `FOR UPDATE SKIP LOCKED`, retried with backoff, and recurring jobs that
//! re-queue themselves.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;
use tokio_util::sync::CancellationToken;

use crate::Indexer;

/// Job kinds.
pub mod kind {
    pub const MBOX_MONTH: &str = "mbox_month";
    pub const SEED_THREAD: &str = "seed_thread";
    pub const EMBED: &str = "embed";
    pub const RESOLVE_ANCHORS: &str = "resolve_anchors";
    pub const GRAPH_SYNC: &str = "graph_sync";
    pub const REPAIR_STATS: &str = "repair_stats";
    pub const MAIL_REFRESH: &str = "mail_refresh";
    pub const CODE_HISTORY_SYNC: &str = "code_history_sync";
    pub const PATCH_LINES: &str = "patch_lines";
    pub const DUPLICATES: &str = "duplicates";
    /// Gives mail stored before ingest kept email addresses its addresses
    /// back: a month, or messages from thread pages ([`crate::addresses`]).
    pub const RESTORE_ADDRESSES: &str = "restore_addresses";
}

/// The recurring jobs, with how long each waits after a run that found nothing to do.
pub const RECURRING: &[(&str, Duration)] = &[
    (kind::EMBED, Duration::from_secs(20)),
    (kind::RESOLVE_ANCHORS, Duration::from_secs(60)),
    (kind::GRAPH_SYNC, Duration::from_secs(30)),
    (kind::REPAIR_STATS, Duration::from_secs(120)),
    // Followed lists' current months, hourly.
    (kind::MAIL_REFRESH, Duration::from_secs(3600)),
    // New commits on every branch, hourly.
    (kind::CODE_HISTORY_SYNC, Duration::from_secs(3600)),
    (kind::PATCH_LINES, Duration::from_secs(60)),
    // New features and decisions, compared for find_duplicates.
    (kind::DUPLICATES, Duration::from_secs(60)),
];

/// How long a graph sync job applies batches of changes before it yields.
const GRAPH_SYNC_BUDGET: Duration = Duration::from_secs(10);

/// How long a duplicates job compares extracts before it yields.
const DUPLICATES_BUDGET: Duration = Duration::from_secs(10);

/// Patches a patch_lines run indexes.
const PATCH_LINES_BATCH: i64 = 200;

/// Attempts of an ingest job whose messages partly failed (a deadlock with a
/// concurrent ingest, say) before it settles for what it stored.
const INGEST_ATTEMPTS: i32 = 3;

/// What a job's run decided.
pub enum Outcome {
    Done(Value),
    /// Run again after `after`: recurring jobs, and jobs with more to do.
    Again {
        after: Duration,
        result: Value,
    },
}

/// A claimed job.
#[derive(Debug)]
pub struct Job {
    pub id: i64,
    pub source_id: Option<i64>,
    pub kind: String,
    pub key: String,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
}

/// Queues a job unless one with the same kind and key is queued, running or
/// done; a failed or cancelled one is queued again. Returns whether it queued.
pub async fn enqueue(
    client: &impl GenericClient,
    source_id: Option<i64>,
    kind: &str,
    key: &str,
    payload: &Value,
    priority: i32,
) -> Result<bool> {
    let row = client
        .query_opt(
            "INSERT INTO jobs (source_id, kind, key, payload, priority)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (kind, key) DO UPDATE
                 SET state = 'queued', run_after = now(), attempts = 0, last_error = NULL,
                     payload = EXCLUDED.payload, source_id = EXCLUDED.source_id,
                     priority = EXCLUDED.priority, finished_at = NULL
                 WHERE jobs.state IN ('failed', 'cancelled')
             RETURNING id",
            &[&source_id, &kind, &key, payload, &priority],
        )
        .await?;
    Ok(row.is_some())
}

/// Makes sure every recurring job exists and is queued. They're short and
/// take priority, so a long download doesn't starve embedding or the graph.
pub async fn ensure_recurring(client: &impl GenericClient) -> Result<()> {
    for (kind, _) in RECURRING {
        client
            .execute(
                "INSERT INTO jobs (kind, key, payload, max_attempts, priority) VALUES ($1, 'recurring', '{}', 1000000, 10)
                 ON CONFLICT (kind, key) DO UPDATE SET priority = 10, state = CASE
                     WHEN jobs.state IN ('done', 'failed', 'cancelled') THEN 'queued' ELSE jobs.state END,
                     run_after = CASE WHEN jobs.state IN ('done', 'failed', 'cancelled') THEN now() ELSE jobs.run_after END",
                &[kind],
            )
            .await?;
    }
    Ok(())
}

/// Wakes a recurring job now, such as the graph sync after an ingest.
pub async fn wake(client: &impl GenericClient, kind: &str) -> Result<()> {
    client
        .execute(
            "UPDATE jobs SET run_after = now() WHERE kind = $1 AND key = 'recurring' AND state = 'queued'",
            &[&kind],
        )
        .await?;
    Ok(())
}

/// Claims the next due job, if any: of `kinds` only, when given.
pub async fn claim(
    client: &impl GenericClient,
    node: &str,
    kinds: Option<&[&str]>,
) -> Result<Option<Job>> {
    let kinds: Option<Vec<String>> =
        kinds.map(|kinds| kinds.iter().map(|kind| (*kind).to_owned()).collect());
    let row = client
        .query_opt(
            "UPDATE jobs SET state = 'running', locked_by = $1, locked_at = now(), attempts = attempts + 1
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE state = 'queued' AND run_after <= now() AND ($2::text[] IS NULL OR kind = ANY ($2))
                 ORDER BY priority DESC, run_after, id
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING id, source_id, kind, key, payload, attempts, max_attempts",
            &[&node, &kinds],
        )
        .await?;
    Ok(row.map(|row| Job {
        id: row.get(0),
        source_id: row.get(1),
        kind: row.get(2),
        key: row.get(3),
        payload: row.get(4),
        attempts: row.get(5),
        max_attempts: row.get(6),
    }))
}

/// Waits to be the database's only indexer that runs jobs, which a new
/// deployment's indexer must while the old one still runs: a session
/// advisory lock, held by the returned connection until the process exits.
/// None when `shutdown` comes first.
pub async fn lead(
    database_url: &str,
    shutdown: &CancellationToken,
) -> Result<Option<tokio_postgres::Client>> {
    let client = pghacker_core::db::connect(database_url, "pghacker-indexer leader").await?;
    let mut waiting = false;
    loop {
        let led: bool = client
            .query_one(
                "SELECT pg_try_advisory_lock(hashtext('pghacker.indexer.jobs'))",
                &[],
            )
            .await?
            .get(0);
        if led {
            if waiting {
                tracing::info!("the other indexer stopped; running the jobs");
            }
            return Ok(Some(client));
        }
        if !waiting {
            tracing::info!("another indexer runs the jobs; waiting until it stops");
            waiting = true;
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

/// Returns jobs left running by a stopped indexer to the queue. Only the
/// indexer that [`lead`]s runs jobs, so at its start every running job is
/// orphaned.
pub async fn requeue_orphans(client: &impl GenericClient) -> Result<u64> {
    Ok(client
        .execute(
            "UPDATE jobs SET state = 'queued', locked_by = NULL, locked_at = NULL, run_after = now()
             WHERE state = 'running'",
            &[],
        )
        .await?)
}

async fn finish(client: &impl GenericClient, job: &Job, outcome: Result<Outcome>) -> Result<()> {
    match outcome {
        Ok(Outcome::Done(result)) => {
            client
                .execute(
                    "UPDATE jobs SET state = 'done', result = $2, last_error = NULL, finished_at = now(),
                         locked_by = NULL, locked_at = NULL
                     WHERE id = $1",
                    &[&job.id, &result],
                )
                .await?;
        }
        Ok(Outcome::Again { after, result }) => {
            let seconds = after.as_secs_f64();
            client
                .execute(
                    "UPDATE jobs SET state = 'queued', result = $2, last_error = NULL, attempts = 0,
                         run_after = now() + make_interval(secs => $3), finished_at = now(),
                         locked_by = NULL, locked_at = NULL
                     WHERE id = $1",
                    &[&job.id, &result, &seconds],
                )
                .await?;
        }
        Err(error) => {
            let message = format!("{error:#}");
            let retry = job.attempts < job.max_attempts;
            let backoff = 30.0 * 2f64.powi(job.attempts.clamp(0, 8));
            tracing::warn!(job = job.id, kind = %job.kind, key = %job.key, attempts = job.attempts, retry, error = %message, "a job failed");
            client
                .execute(
                    "UPDATE jobs SET state = CASE WHEN $3 THEN 'queued' ELSE 'failed' END,
                         last_error = $2, run_after = now() + make_interval(secs => $4),
                         finished_at = now(), locked_by = NULL, locked_at = NULL
                     WHERE id = $1",
                    &[&job.id, &message, &retry, &backoff],
                )
                .await?;
        }
    }
    Ok(())
}

/// Runs one job's handler.
async fn execute(indexer: &Indexer, job: &Job) -> Result<Outcome> {
    let text = |name: &str| -> Result<String> {
        job.payload
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("job {} has no {name}", job.id))
    };
    match job.kind.as_str() {
        kind::MBOX_MONTH => {
            let refresh = job
                .payload
                .get("refresh")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let stats = pghacker_mail::ingest::ingest_mbox_month(
                &indexer.pool,
                &indexer.archive,
                &text("list")?,
                &text("month")?,
                job.source_id,
                refresh,
            )
            .await?;
            after_ingest(indexer).await?;
            settle(job, stats)
        }
        kind::SEED_THREAD => {
            let stats = pghacker_mail::ingest::ingest_thread(
                &indexer.pool,
                &indexer.archive,
                &text("list")?,
                &text("message_id")?,
                job.source_id,
            )
            .await?;
            after_ingest(indexer).await?;
            settle(job, stats)
        }
        kind::RESTORE_ADDRESSES => {
            let stats = match job.payload.get("month") {
                Some(_) => {
                    pghacker_mail::ingest::restore_mbox_month(
                        &indexer.pool,
                        &indexer.archive,
                        &text("list")?,
                        &text("month")?,
                    )
                    .await?
                }
                None => {
                    let ids: Vec<String> = serde_json::from_value(
                        job.payload.get("message_ids").cloned().unwrap_or(json!([])),
                    )?;
                    pghacker_mail::ingest::restore_messages(&indexer.pool, &indexer.archive, &ids)
                        .await?
                }
            };
            after_ingest(indexer).await?;
            // Restoring again changes only what's left.
            if stats.failed > 0 && job.attempts < INGEST_ATTEMPTS {
                return Err(anyhow!(
                    "{} messages failed; restoring again for the rest",
                    stats.failed
                ));
            }
            Ok(Outcome::Done(serde_json::to_value(stats)?))
        }
        kind::EMBED => {
            let report = crate::embedding::backfill(indexer, 40).await?;
            let after = if report.more {
                Duration::ZERO
            } else {
                recurring_interval(kind::EMBED)
            };
            Ok(Outcome::Again {
                after,
                result: serde_json::to_value(report)?,
            })
        }
        kind::RESOLVE_ANCHORS => {
            let client = indexer.pool.get().await?;
            let resolved = crate::anchors::resolve(&**client).await?;
            Ok(Outcome::Again {
                after: recurring_interval(kind::RESOLVE_ANCHORS),
                result: json!({ "resolved": resolved }),
            })
        }
        kind::GRAPH_SYNC => {
            let mut client = indexer.pool.get().await?;
            let report = crate::graph::run(
                &mut client,
                GRAPH_SYNC_BUDGET,
                indexer.settings.graph_reconcile_every,
            )
            .await?;
            // Changes still waiting, including those committed while it ran:
            // run again at once.
            let after = if report.more {
                Duration::ZERO
            } else {
                recurring_interval(kind::GRAPH_SYNC)
            };
            Ok(Outcome::Again {
                after,
                result: serde_json::to_value(report)?,
            })
        }
        kind::REPAIR_STATS => {
            let client = indexer.pool.get().await?;
            let repaired = crate::repair::thread_and_author_stats(&**client).await?;
            Ok(Outcome::Again {
                after: recurring_interval(kind::REPAIR_STATS),
                result: json!({ "repaired": repaired }),
            })
        }
        kind::MAIL_REFRESH => {
            let client = indexer.pool.get().await?;
            let queued = crate::sources::refresh_followed(&**client, chrono::Utc::now()).await?;
            if !queued.is_empty() {
                tracing::info!(?queued, "refreshing followed lists");
            }
            Ok(Outcome::Again {
                after: recurring_interval(kind::MAIL_REFRESH),
                result: json!({ "queued": queued }),
            })
        }
        kind::CODE_HISTORY_SYNC => Ok(Outcome::Again {
            after: recurring_interval(kind::CODE_HISTORY_SYNC),
            result: crate::history::sync(indexer).await?,
        }),
        kind::PATCH_LINES => {
            let mut client = indexer.pool.get().await?;
            let indexed = pghacker_mail::patch_lines::index(&mut client, PATCH_LINES_BATCH).await?;
            let after = if indexed.more {
                Duration::ZERO
            } else {
                recurring_interval(kind::PATCH_LINES)
            };
            Ok(Outcome::Again {
                after,
                result: serde_json::to_value(indexed)?,
            })
        }
        kind::DUPLICATES => {
            let mut client = indexer.pool.get().await?;
            let report = crate::duplicates::refresh(&mut client, DUPLICATES_BUDGET).await?;
            let after = if report.more {
                Duration::ZERO
            } else {
                recurring_interval(kind::DUPLICATES)
            };
            Ok(Outcome::Again {
                after,
                result: serde_json::to_value(report)?,
            })
        }
        other => Err(anyhow!("unknown job kind {other}")),
    }
}

/// An ingest job's outcome: done, or retried while some messages failed.
/// Ingesting again is idempotent, so a retry stores only what's missing.
fn settle(job: &Job, stats: pghacker_mail::ingest::IngestStats) -> Result<Outcome> {
    if stats.failed > 0 && job.attempts < INGEST_ATTEMPTS {
        return Err(anyhow!(
            "{} of {} messages failed; retrying for the rest",
            stats.failed,
            stats.fetched
        ));
    }
    Ok(Outcome::Done(serde_json::to_value(stats)?))
}

fn recurring_interval(kind: &str) -> Duration {
    RECURRING
        .iter()
        .find(|(name, _)| *name == kind)
        .map_or(Duration::from_secs(60), |(_, after)| *after)
}

/// After new messages: resolve their anchors, embed them and sync the graph soon.
async fn after_ingest(indexer: &Indexer) -> Result<()> {
    let client = indexer.pool.get().await?;
    for kind in [
        kind::EMBED,
        kind::RESOLVE_ANCHORS,
        kind::REPAIR_STATS,
        kind::GRAPH_SYNC,
        kind::PATCH_LINES,
    ] {
        wake(&**client, kind).await?;
    }
    Ok(())
}

/// Runs `workers` workers until `shutdown` is cancelled; each finishes its
/// current job first. Each worker claims the jobs of its [`lane`].
pub async fn run(indexer: Indexer, workers: usize, shutdown: CancellationToken) -> Result<()> {
    {
        let client = indexer
            .pool
            .get()
            .await
            .context("cannot connect to the database")?;
        let orphans = requeue_orphans(&**client).await?;
        if orphans > 0 {
            tracing::info!(orphans, "requeued jobs left running");
        }
        ensure_recurring(&**client).await?;
    }
    let node = format!("pghacker-indexer-{}", std::process::id());
    let mut handles = Vec::new();
    for worker in 0..workers {
        let indexer = indexer.clone();
        let shutdown = shutdown.clone();
        let node = format!("{node}/{worker}");
        let kinds = lane(worker, workers);
        handles.push(tokio::spawn(async move {
            worker_loop(indexer, node, kinds, shutdown).await
        }));
    }
    for handle in handles {
        handle.await?;
    }
    Ok(())
}

const RECURRING_KINDS: &[&str] = &[
    kind::EMBED,
    kind::RESOLVE_ANCHORS,
    kind::GRAPH_SYNC,
    kind::REPAIR_STATS,
    kind::MAIL_REFRESH,
    kind::CODE_HISTORY_SYNC,
    kind::PATCH_LINES,
    kind::DUPLICATES,
];

/// The job kinds worker `worker` of `workers` claims: only these, when given.
/// With more than one worker, worker 0 runs only the recurring jobs, so
/// embedding, anchors and the graph keep up while the others download; the
/// others run anything.
pub fn lane(worker: usize, workers: usize) -> Option<&'static [&'static str]> {
    (worker == 0 && workers > 1).then_some(RECURRING_KINDS)
}

async fn worker_loop(
    indexer: Indexer,
    node: String,
    kinds: Option<&'static [&'static str]>,
    shutdown: CancellationToken,
) {
    while !shutdown.is_cancelled() {
        let claimed = match indexer.pool.get().await {
            Ok(client) => claim(&**client, &node, kinds).await,
            Err(error) => Err(error.into()),
        };
        match claimed {
            Ok(Some(job)) => {
                let started = std::time::Instant::now();
                let outcome = execute(&indexer, &job).await;
                let ok = outcome.is_ok();
                let finished = match indexer.pool.get().await {
                    Ok(client) => finish(&**client, &job, outcome).await,
                    Err(error) => Err(error.into()),
                };
                if let Err(error) = finished {
                    tracing::error!(job = job.id, error = %format!("{error:#}"), "cannot record a job's outcome");
                }
                if !matches!(
                    job.kind.as_str(),
                    kind::EMBED
                        | kind::RESOLVE_ANCHORS
                        | kind::GRAPH_SYNC
                        | kind::MAIL_REFRESH
                        | kind::DUPLICATES
                ) || !ok
                {
                    tracing::info!(job = job.id, kind = %job.kind, key = %job.key, ok, ms = started.elapsed().as_millis() as u64, "job finished");
                }
            }
            Ok(None) => {
                tokio::select! {
                    () = shutdown.cancelled() => {},
                    () = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "cannot claim a job");
                tokio::select! {
                    () = shutdown.cancelled() => {},
                    () = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
            }
        }
    }
}
