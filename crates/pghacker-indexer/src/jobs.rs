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
    pub const CODE_SNAPSHOT: &str = "code_snapshot";
    pub const EMBED: &str = "embed";
    pub const RESOLVE_ANCHORS: &str = "resolve_anchors";
    pub const GRAPH_SYNC: &str = "graph_sync";
}

/// The recurring jobs, with how long each waits after a run that found nothing to do.
pub const RECURRING: &[(&str, Duration)] = &[
    (kind::EMBED, Duration::from_secs(20)),
    (kind::RESOLVE_ANCHORS, Duration::from_secs(60)),
    (kind::GRAPH_SYNC, Duration::from_secs(60)),
];

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

/// Makes sure every recurring job exists and is queued.
pub async fn ensure_recurring(client: &impl GenericClient) -> Result<()> {
    for (kind, _) in RECURRING {
        client
            .execute(
                "INSERT INTO jobs (kind, key, payload, max_attempts) VALUES ($1, 'recurring', '{}', 1000000)
                 ON CONFLICT (kind, key) DO UPDATE SET state = 'queued', run_after = now()
                 WHERE jobs.state IN ('done', 'failed', 'cancelled')",
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

/// Claims the next due job, if any.
pub async fn claim(client: &impl GenericClient, node: &str) -> Result<Option<Job>> {
    let row = client
        .query_opt(
            "UPDATE jobs SET state = 'running', locked_by = $1, locked_at = now(), attempts = attempts + 1
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE state = 'queued' AND run_after <= now()
                 ORDER BY priority DESC, run_after, id
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING id, source_id, kind, key, payload, attempts, max_attempts",
            &[&node],
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

/// Returns jobs left running by a stopped indexer to the queue. One indexer
/// runs per database, so at startup every running job is orphaned.
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
            Ok(Outcome::Done(serde_json::to_value(stats)?))
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
            Ok(Outcome::Done(serde_json::to_value(stats)?))
        }
        kind::CODE_SNAPSHOT => {
            let paths: Vec<String> =
                serde_json::from_value(job.payload.get("paths").cloned().unwrap_or(json!([])))?;
            let spec = pghacker_code::SnapshotSpec {
                repo: text("repo")?,
                git_ref: text("branch")?,
                sha: text("sha")?,
                version: job
                    .payload
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                paths,
                source: pghacker_code::RepoSource::Remote {
                    url: text("repo_url")?,
                    cache_dir: indexer.settings.git_dir.clone(),
                },
                source_id: job.source_id,
            };
            let mut client = indexer.pool.get().await?;
            let stats = pghacker_code::load_snapshot(&mut client, &spec).await?;
            wake(&**client, kind::RESOLVE_ANCHORS).await?;
            wake(&**client, kind::GRAPH_SYNC).await?;
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
            let stats = crate::graph::sync(&mut client).await?;
            Ok(Outcome::Again {
                after: recurring_interval(kind::GRAPH_SYNC),
                result: serde_json::to_value(stats)?,
            })
        }
        other => Err(anyhow!("unknown job kind {other}")),
    }
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
    for kind in [kind::EMBED, kind::RESOLVE_ANCHORS, kind::GRAPH_SYNC] {
        wake(&**client, kind).await?;
    }
    Ok(())
}

/// Runs `workers` workers until `shutdown` is cancelled; each finishes its
/// current job first.
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
        handles.push(tokio::spawn(async move {
            worker_loop(indexer, node, shutdown).await
        }));
    }
    for handle in handles {
        handle.await?;
    }
    Ok(())
}

async fn worker_loop(indexer: Indexer, node: String, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        let claimed = match indexer.pool.get().await {
            Ok(client) => claim(&**client, &node).await,
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
                    kind::EMBED | kind::RESOLVE_ANCHORS | kind::GRAPH_SYNC
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
