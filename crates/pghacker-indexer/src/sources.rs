//! Sources: the mailing lists and PostgreSQL versions the indexer retrieves.
//! Adding one queues the jobs that retrieve its configured slice.

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use crate::Indexer;
use crate::jobs::{self, kind};
use crate::slices::is_full_sha;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A mailing list, such as pgsql-hackers.
    MailingList,
    /// A PostgreSQL major version's source code, such as 19.
    PostgresVersion,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MailingList => "mailing_list",
            Self::PostgresVersion => "postgres_version",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddSourceArgs {
    pub kind: SourceKind,
    /// The list's name (`pgsql-hackers`) or the major version (`19`).
    pub name: String,
    /// postgres_version only: the branch the commit is pinned from. Defaults to the test slice's.
    #[serde(default)]
    pub branch: Option<String>,
    /// postgres_version only: the full commit SHA. Defaults to the test slice's.
    #[serde(default)]
    pub sha: Option<String>,
    /// postgres_version only: path prefixes to load. Defaults to the test slice's.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// postgres_version only: the git URL. Defaults to the test slice's.
    #[serde(default)]
    pub repo_url: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveSourceArgs {
    pub kind: SourceKind,
    pub name: String,
    /// Also delete what the source loaded: a version's code snapshot. Mailing
    /// lists keep their messages, which extracts cite.
    #[serde(default)]
    pub purge: bool,
}

pub async fn add_source(indexer: &Indexer, args: AddSourceArgs) -> Result<Value> {
    let name = args.name.trim().to_owned();
    let mut client = indexer.pool.get().await?;
    let tx = client.transaction().await?;
    let mut enqueued = Vec::new();
    let source_id = match args.kind {
        SourceKind::MailingList => {
            let slice = &indexer.slices.mail;
            if name != slice.list {
                bail!(
                    "wave 1 retrieves only the test slice of {}; {name} isn't configured in the test slices",
                    slice.list
                );
            }
            let id = upsert(&*tx, args.kind, &name, &json!({})).await?;
            for month in &slice.months {
                let key = format!("{name}/{month}");
                let payload = json!({ "list": name, "month": month, "refresh": false });
                if jobs::enqueue(&*tx, Some(id), kind::MBOX_MONTH, &key, &payload, 1).await? {
                    enqueued.push(key);
                }
            }
            for seed in &slice.seed_threads {
                let key = format!("{name}/thread/{}", seed.message_id);
                let payload =
                    json!({ "list": name, "message_id": seed.message_id, "topic": seed.topic });
                if jobs::enqueue(&*tx, Some(id), kind::SEED_THREAD, &key, &payload, 2).await? {
                    enqueued.push(key);
                }
            }
            id
        }
        SourceKind::PostgresVersion => {
            let slice = &indexer.slices.code;
            if name != slice.version && args.sha.is_none() {
                bail!("version {name} has no default commit; pass branch and sha");
            }
            let branch = args.branch.unwrap_or_else(|| slice.branch.clone());
            let sha = args
                .sha
                .unwrap_or_else(|| slice.sha.clone())
                .to_ascii_lowercase();
            if !is_full_sha(&sha) {
                bail!("sha must be a full 40-character commit SHA");
            }
            let paths = args.paths.unwrap_or_else(|| slice.paths.clone());
            let repo_url = args.repo_url.unwrap_or_else(|| slice.repo_url.clone());
            let config =
                json!({ "repo_url": repo_url, "branch": branch, "sha": sha, "paths": paths });
            let id = upsert(&*tx, args.kind, &name, &config).await?;
            // A loaded snapshot of the same commit with other paths is replaced.
            tx.execute(
                "DELETE FROM code_snapshots WHERE repo = 'postgres' AND sha = $1 AND paths <> $2",
                &[&sha, &paths],
            )
            .await?;
            let key = format!("postgres/{sha}");
            let payload = json!({
                "repo": "postgres", "repo_url": repo_url, "branch": branch, "sha": sha,
                "version": name, "paths": paths,
            });
            tx.execute(
                "UPDATE jobs SET state = 'cancelled' WHERE kind = $1 AND key = $2 AND state = 'done'
                 AND payload <> $3",
                &[&kind::CODE_SNAPSHOT, &key, &payload],
            )
            .await?;
            if jobs::enqueue(&*tx, Some(id), kind::CODE_SNAPSHOT, &key, &payload, 3).await? {
                enqueued.push(key);
            }
            id
        }
    };
    tx.commit().await?;
    Ok(json!({
        "source": { "id": source_id, "kind": args.kind.as_str(), "name": name },
        "jobs_queued": enqueued,
        "next": "Call ingest_status to watch retrieval, parsing, embedding and extraction progress.",
    }))
}

async fn upsert(
    client: &impl GenericClient,
    kind: SourceKind,
    name: &str,
    config: &Value,
) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO sources (kind, name, config) VALUES ($1, $2, $3)
             ON CONFLICT (kind, name) DO UPDATE SET config = EXCLUDED.config, enabled = true, updated_at = now()
             RETURNING id",
            &[&kind.as_str(), &name, config],
        )
        .await?
        .get(0))
}

pub async fn remove_source(indexer: &Indexer, args: RemoveSourceArgs) -> Result<Value> {
    let mut client = indexer.pool.get().await?;
    let tx = client.transaction().await?;
    let Some(row) = tx
        .query_opt(
            "UPDATE sources SET enabled = false, updated_at = now() WHERE kind = $1 AND name = $2 RETURNING id",
            &[&args.kind.as_str(), &args.name],
        )
        .await?
    else {
        bail!("there's no {} source named {}", args.kind.as_str(), args.name);
    };
    let id: i64 = row.get(0);
    let cancelled = tx
        .execute(
            "UPDATE jobs SET state = 'cancelled', finished_at = now() WHERE source_id = $1 AND state = 'queued'",
            &[&id],
        )
        .await?;
    let mut purged = 0;
    if args.purge && args.kind == SourceKind::PostgresVersion {
        purged = tx
            .execute("DELETE FROM code_snapshots WHERE source_id = $1", &[&id])
            .await?;
        tx.execute(
            "UPDATE jobs SET state = 'cancelled' WHERE source_id = $1 AND kind = $2",
            &[&id, &kind::CODE_SNAPSHOT],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(json!({
        "source": { "id": id, "kind": args.kind.as_str(), "name": args.name, "enabled": false },
        "jobs_cancelled": cancelled,
        "snapshots_purged": purged,
    }))
}

pub async fn list_sources(indexer: &Indexer) -> Result<Value> {
    let client = indexer.pool.get().await?;
    let rows = client
        .query(
            "SELECT s.id, s.kind, s.name, s.config, s.enabled, s.created_at,
                    count(j.*) FILTER (WHERE j.state = 'queued') AS queued,
                    count(j.*) FILTER (WHERE j.state = 'running') AS running,
                    count(j.*) FILTER (WHERE j.state = 'done') AS done,
                    count(j.*) FILTER (WHERE j.state = 'failed') AS failed
             FROM sources s LEFT JOIN jobs j ON j.source_id = s.id
             GROUP BY s.id ORDER BY s.id",
            &[],
        )
        .await?;
    let sources: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<_, i64>(0),
                "kind": row.get::<_, String>(1),
                "name": row.get::<_, String>(2),
                "config": row.get::<_, Value>(3),
                "enabled": row.get::<_, bool>(4),
                "created_at": row.get::<_, chrono::DateTime<chrono::Utc>>(5),
                "jobs": { "queued": row.get::<_, i64>(6), "running": row.get::<_, i64>(7),
                          "done": row.get::<_, i64>(8), "failed": row.get::<_, i64>(9) },
            })
        })
        .collect();
    Ok(json!({
        "sources": sources,
        "available": {
            "mailing_lists": [indexer.slices.mail.list],
            "postgres_versions": [{
                "name": indexer.slices.code.version, "branch": indexer.slices.code.branch,
                "sha": indexer.slices.code.sha, "paths": indexer.slices.code.paths,
            }],
            "note": "Wave 1 retrieves the configured test slices only.",
        },
    }))
}
