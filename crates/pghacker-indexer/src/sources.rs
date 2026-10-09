//! Sources: the mailing lists and PostgreSQL versions the indexer retrieves.
//! Adding one queues the jobs that retrieve it.
//!
//! `PGHACKER_SOURCES` decides what can be added. `test-slices`, the default,
//! retrieves only the configured test slices. `open` retrieves any list of the
//! postgresql.org archive (chosen months, a range, or the whole archive, plus
//! seed threads, and optionally following the list: its current month is
//! retrieved again every hour) and any branch, tag or commit of PostgreSQL.

use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Datelike, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use crate::Indexer;
use crate::jobs::{self, kind};
use crate::slices::is_full_sha;

/// PostgreSQL's repository, and its GitHub mirror: the code sources allowed.
pub const OFFICIAL_REPOS: &[&str] = &[
    "https://git.postgresql.org/git/postgresql.git",
    "https://github.com/postgres/postgres.git",
];

/// The jobs listed in add_source's reply; the rest are counted.
const LISTED_JOBS: usize = 20;

/// What add_source may add: `PGHACKER_SOURCES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourcesMode {
    /// The configured test slices only.
    #[default]
    TestSlices,
    /// Any list of the archive, and any branch, tag or commit of PostgreSQL.
    Open,
}

impl FromStr for SourcesMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "test-slices" => Ok(Self::TestSlices),
            "open" => Ok(Self::Open),
            other => bail!("PGHACKER_SOURCES must be test-slices or open, not {other:?}"),
        }
    }
}

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
    /// The list's name (`pgsql-hackers`), or the PostgreSQL major version
    /// (`19`, or `master` for the development branch).
    pub name: String,
    /// mailing_list: the months to retrieve, YYYY-MM. Without months,
    /// from, to or seed_threads, the whole archive of the list.
    #[serde(default)]
    pub months: Option<Vec<String>>,
    /// mailing_list: the first month of a range, YYYY-MM (default: the
    /// archive's first).
    #[serde(default)]
    pub from: Option<String>,
    /// mailing_list: the last month of a range, YYYY-MM (default: the
    /// archive's last, the current month).
    #[serde(default)]
    pub to: Option<String>,
    /// mailing_list: whole threads to retrieve, each by one of its messages'
    /// Message-IDs.
    #[serde(default)]
    pub seed_threads: Option<Vec<SeedThreadArg>>,
    /// mailing_list: keep the list current by retrieving its current month
    /// again every hour (true), or stop (false). Unchanged when omitted.
    #[serde(default)]
    pub follow: Option<bool>,
    /// postgres_version: the branch or tag to load (default: REL_<name>_STABLE,
    /// or master while the version has no stable branch).
    #[serde(default)]
    pub branch: Option<String>,
    /// postgres_version: the full commit SHA (default: the branch's or tag's
    /// current commit).
    #[serde(default)]
    pub sha: Option<String>,
    /// postgres_version: path prefixes to load (default: the whole tree).
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// postgres_version: the git URL, PostgreSQL's repository or its GitHub
    /// mirror (default: https://git.postgresql.org/git/postgresql.git).
    #[serde(default)]
    pub repo_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct SeedThreadArg {
    /// The Message-ID of any message of the thread.
    pub message_id: String,
    /// What the thread is about, for progress reports.
    #[serde(default)]
    pub topic: Option<String>,
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

/// A job to queue.
struct PlannedJob {
    kind: &'static str,
    key: String,
    payload: Value,
    priority: i32,
}

/// What adding a source does: its configuration (merged into the stored
/// one), the jobs it queues, and what the reply says about them.
struct Plan {
    config: Value,
    jobs: Vec<PlannedJob>,
    /// A code snapshot replaces a loaded snapshot of the same commit with other paths.
    snapshot: Option<Value>,
    summary: Value,
}

pub async fn add_source(indexer: &Indexer, args: AddSourceArgs) -> Result<Value> {
    let name = args.name.trim().to_owned();
    ensure!(!name.is_empty(), "name is empty");
    let source_kind = args.kind;
    let plan = match (indexer.settings.sources, args.kind) {
        (SourcesMode::TestSlices, SourceKind::MailingList) => {
            test_list_plan(indexer, &name, &args)?
        }
        (SourcesMode::TestSlices, SourceKind::PostgresVersion) => {
            test_version_plan(indexer, &name, args)?
        }
        (SourcesMode::Open, SourceKind::MailingList) => list_plan(indexer, &name, &args).await?,
        (SourcesMode::Open, SourceKind::PostgresVersion) => version_plan(&name, args).await?,
    };

    let mut client = indexer.pool.get().await?;
    let tx = client.transaction().await?;
    let source_id = upsert(&*tx, source_kind, &name, &plan.config).await?;
    if let Some(snapshot) = &plan.snapshot {
        // A loaded snapshot of the same commit with other paths is replaced.
        let sha = snapshot["sha"].as_str().unwrap_or_default();
        let paths: Vec<String> = serde_json::from_value(snapshot["paths"].clone())?;
        tx.execute(
            "DELETE FROM code_snapshots WHERE repo = 'postgres' AND sha = $1 AND paths <> $2",
            &[&sha, &paths],
        )
        .await?;
        tx.execute(
            "UPDATE jobs SET state = 'cancelled' WHERE kind = $1 AND key = $2 AND state = 'done'
             AND payload <> $3",
            &[&kind::CODE_SNAPSHOT, &format!("postgres/{sha}"), snapshot],
        )
        .await?;
    }
    let mut queued = Vec::new();
    for job in &plan.jobs {
        if jobs::enqueue(
            &*tx,
            Some(source_id),
            job.kind,
            &job.key,
            &job.payload,
            job.priority,
        )
        .await?
        {
            queued.push(job.key.clone());
        }
    }
    let config: Value = tx
        .query_one("SELECT config FROM sources WHERE id = $1", &[&source_id])
        .await?
        .get(0);
    tx.commit().await?;

    let mut reply = json!({
        "source": { "id": source_id, "kind": source_kind.as_str(), "name": name, "config": config },
        "jobs_queued": queued.len(),
        "queued": queued.iter().take(LISTED_JOBS).collect::<Vec<_>>(),
        "next": "Call ingest_status to watch retrieval, parsing, embedding and extraction progress.",
    });
    if let (Some(reply), Some(summary)) = (reply.as_object_mut(), plan.summary.as_object()) {
        reply.extend(summary.clone());
    }
    Ok(reply)
}

/// A test-slices list: the configured months and seed threads.
fn test_list_plan(indexer: &Indexer, name: &str, args: &AddSourceArgs) -> Result<Plan> {
    if args.months.is_some()
        || args.from.is_some()
        || args.to.is_some()
        || args.seed_threads.is_some()
        || args.follow.is_some()
    {
        bail!(
            "this deployment retrieves only the configured test slices (PGHACKER_SOURCES=test-slices): \
             months, from, to, seed_threads and follow need PGHACKER_SOURCES=open"
        );
    }
    // Re-read, so seeds added to the file need no restart.
    let slices = crate::slices::Slices::load(&indexer.settings.slices_path)?;
    let slice = &slices.mail;
    if name != slice.list {
        bail!(
            "this deployment retrieves only the test slice of {}; {name} isn't configured in the test slices",
            slice.list
        );
    }
    let seeds = slice
        .seed_threads
        .iter()
        .map(|seed| SeedThreadArg {
            message_id: seed.message_id.clone(),
            topic: Some(seed.topic.clone()),
        })
        .collect::<Vec<_>>();
    Ok(Plan {
        config: json!({}),
        jobs: list_jobs(name, &slice.months, &seeds),
        snapshot: None,
        summary: json!({}),
    })
}

/// Any list of the archive: chosen months, a range, or the whole archive,
/// plus seed threads; and whether to follow it.
async fn list_plan(indexer: &Indexer, name: &str, args: &AddSourceArgs) -> Result<Plan> {
    let archive = match indexer.archive.list_months(name).await {
        Ok(months) => months,
        Err(error) if error.downcast_ref::<pghacker_mail::NotFound>().is_some() => bail!(
            "postgresql.org has no mailing list {name}; its lists are at https://www.postgresql.org/list/"
        ),
        Err(error) => return Err(error).context("cannot read the list's archive"),
    };
    let seeds = args.seed_threads.clone().unwrap_or_default();
    for seed in &seeds {
        ensure!(
            !pghacker_mail::parse::normalize_message_id(&seed.message_id).is_empty(),
            "a seed thread's message_id is empty"
        );
    }
    let months = select_months(
        &archive,
        args.months.as_deref(),
        args.from.as_deref(),
        args.to.as_deref(),
        !seeds.is_empty(),
    )?;
    let config = match args.follow {
        Some(follow) => json!({ "follow": follow }),
        None => json!({}),
    };
    let summary = json!({
        "months": {
            "count": months.len(),
            "first": months.first(),
            "last": months.last(),
            "archive": { "first": archive.first(), "last": archive.last(), "months": archive.len() },
        },
        "seed_threads": seeds.len(),
    });
    Ok(Plan {
        config,
        jobs: list_jobs(name, &months, &seeds),
        snapshot: None,
        summary,
    })
}

/// The jobs of a list's months, newest first, and seed threads, first of all.
fn list_jobs(list: &str, months: &[String], seeds: &[SeedThreadArg]) -> Vec<PlannedJob> {
    let mut planned = Vec::new();
    for seed in seeds {
        let message_id = pghacker_mail::parse::normalize_message_id(&seed.message_id);
        planned.push(PlannedJob {
            kind: kind::SEED_THREAD,
            key: format!("{list}/thread/{message_id}"),
            payload: json!({
                "list": list, "message_id": message_id,
                "topic": seed.topic.clone().unwrap_or_default(),
            }),
            priority: 2,
        });
    }
    for month in months.iter().rev() {
        planned.push(PlannedJob {
            kind: kind::MBOX_MONTH,
            key: format!("{list}/{month}"),
            payload: json!({ "list": list, "month": month, "refresh": false }),
            priority: 1,
        });
    }
    planned
}

/// The months to retrieve of an archive that has `archive` (sorted): the
/// given `months`; the range `from`..=`to`, either end defaulting to the
/// archive's; or, with neither and no seed threads, the whole archive.
pub fn select_months(
    archive: &[String],
    months: Option<&[String]>,
    from: Option<&str>,
    to: Option<&str>,
    has_seeds: bool,
) -> Result<Vec<String>> {
    for month in months
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .chain(from)
        .chain(to)
    {
        ensure!(is_month(month), "{month:?} isn't a month: use YYYY-MM");
    }
    let (Some(first), Some(last)) = (archive.first(), archive.last()) else {
        bail!("the list's archive has no months");
    };
    if let Some(months) = months {
        ensure!(
            from.is_none() && to.is_none(),
            "give months, or a range with from and to, not both"
        );
        let unknown: Vec<&str> = months
            .iter()
            .filter(|month| !archive.contains(month))
            .map(String::as_str)
            .collect();
        ensure!(
            unknown.is_empty(),
            "the archive has no {}: it has {first} to {last}",
            unknown.join(", ")
        );
        let mut months = months.to_vec();
        months.sort();
        months.dedup();
        return Ok(months);
    }
    if from.is_none() && to.is_none() && has_seeds {
        return Ok(Vec::new());
    }
    let start = from.unwrap_or(first);
    let end = to.unwrap_or(last);
    ensure!(start <= end, "from ({start}) is after to ({end})");
    let selected: Vec<String> = archive
        .iter()
        .filter(|month| start <= month.as_str() && month.as_str() <= end)
        .cloned()
        .collect();
    ensure!(
        !selected.is_empty(),
        "the archive has no months from {start} to {end}: it has {first} to {last}"
    );
    Ok(selected)
}

fn is_month(month: &str) -> bool {
    month.len() == 7
        && month.as_bytes()[4] == b'-'
        && month[..4].bytes().all(|b| b.is_ascii_digit())
        && month[5..]
            .parse::<u8>()
            .is_ok_and(|m| (1..=12).contains(&m))
}

/// A test-slices version: the configured commit and paths, or a given commit.
fn test_version_plan(indexer: &Indexer, name: &str, args: AddSourceArgs) -> Result<Plan> {
    let slice = &indexer.slices.code;
    if name != slice.version && args.sha.is_none() {
        bail!("version {name} has no default commit; pass branch and sha");
    }
    let branch = args.branch.unwrap_or_else(|| slice.branch.clone());
    let sha = args
        .sha
        .unwrap_or_else(|| slice.sha.clone())
        .to_ascii_lowercase();
    ensure!(
        is_full_sha(&sha),
        "sha must be a full 40-character commit SHA"
    );
    let paths = args.paths.unwrap_or_else(|| slice.paths.clone());
    let repo_url = args.repo_url.unwrap_or_else(|| slice.repo_url.clone());
    Ok(snapshot_plan(name, &repo_url, &branch, &sha, paths))
}

/// Any branch, tag or commit of PostgreSQL's repository.
async fn version_plan(name: &str, args: AddSourceArgs) -> Result<Plan> {
    ensure!(
        name == "master" || (name.len() <= 2 && name.bytes().all(|b| b.is_ascii_digit())),
        "name must be a major version, such as 19, or master"
    );
    let repo_url = official_repo(args.repo_url.as_deref())?;
    let paths = args.paths.unwrap_or_default();
    for path in &paths {
        ensure!(
            !path.is_empty()
                && !path.starts_with('/')
                && !path.split('/').any(|part| part == ".." || part == "."),
            "paths must be relative paths in the repository, not {path:?}"
        );
    }
    let (branch, sha) = match (args.branch, args.sha) {
        (branch, Some(sha)) => {
            let sha = sha.trim().to_ascii_lowercase();
            ensure!(
                is_full_sha(&sha),
                "sha must be a full 40-character commit SHA"
            );
            let branch = branch.unwrap_or_else(|| stable_branch(name));
            validate_ref(&branch)?;
            (branch, sha)
        }
        (Some(branch), None) => {
            let sha = resolve_ref(&repo_url, &branch)
                .await?
                .ok_or_else(|| anyhow!("{repo_url} has no branch or tag {branch}"))?;
            (branch, sha)
        }
        (None, None) => {
            let stable = stable_branch(name);
            match resolve_ref(&repo_url, &stable).await? {
                Some(sha) => (stable, sha),
                None => {
                    let sha = resolve_ref(&repo_url, "master")
                        .await?
                        .ok_or_else(|| anyhow!("{repo_url} has no master branch"))?;
                    ("master".to_owned(), sha)
                }
            }
        }
    };
    Ok(snapshot_plan(name, &repo_url, &branch, &sha, paths))
}

fn snapshot_plan(name: &str, repo_url: &str, branch: &str, sha: &str, paths: Vec<String>) -> Plan {
    let config = json!({ "repo_url": repo_url, "branch": branch, "sha": sha, "paths": paths });
    let payload = json!({
        "repo": "postgres", "repo_url": repo_url, "branch": branch, "sha": sha,
        "version": name, "paths": paths,
    });
    Plan {
        config,
        jobs: vec![PlannedJob {
            kind: kind::CODE_SNAPSHOT,
            key: format!("postgres/{sha}"),
            payload: payload.clone(),
            priority: 3,
        }],
        snapshot: Some(payload),
        summary: json!({
            "snapshot": {
                "branch": branch, "sha": sha,
                "paths": if paths.is_empty() { json!("the whole tree") } else { json!(paths) },
            },
        }),
    }
}

/// A major version's stable branch: `REL_19_STABLE`; master's is master.
fn stable_branch(name: &str) -> String {
    if name == "master" {
        "master".to_owned()
    } else {
        format!("REL_{name}_STABLE")
    }
}

/// The repository URL, which must be one of [`OFFICIAL_REPOS`].
pub fn official_repo(url: Option<&str>) -> Result<String> {
    let Some(url) = url.map(str::trim).filter(|url| !url.is_empty()) else {
        return Ok(OFFICIAL_REPOS[0].to_owned());
    };
    let normalized = url.trim_end_matches('/');
    let normalized = if normalized.ends_with(".git") {
        normalized.to_owned()
    } else {
        format!("{normalized}.git")
    };
    ensure!(
        OFFICIAL_REPOS.contains(&normalized.as_str()),
        "repo_url must be PostgreSQL's repository: {}",
        OFFICIAL_REPOS.join(" or ")
    );
    Ok(normalized)
}

fn validate_ref(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('-')
            && !name.contains("..")
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/')),
        "{name:?} isn't a branch or tag name"
    );
    Ok(())
}

/// The commit that branch or tag `name` of `url` points to, if it exists:
/// `git ls-remote`, preferring a tag's peeled commit, then the tag, then a branch.
pub async fn resolve_ref(url: &str, name: &str) -> Result<Option<String>> {
    validate_ref(name)?;
    let patterns = [
        format!("refs/tags/{name}^{{}}"),
        format!("refs/tags/{name}"),
        format!("refs/heads/{name}"),
    ];
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("git")
            .arg("ls-remote")
            .arg(url)
            .args(&patterns)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow!("git ls-remote {url} took over 60 s"))?
    .context("cannot run git ls-remote")?;
    ensure!(
        output.status.success(),
        "git ls-remote {url} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(pick_ref(
        &String::from_utf8_lossy(&output.stdout),
        &patterns,
    ))
}

/// The SHA of the first of `patterns` that `ls_remote`'s output lists.
fn pick_ref(ls_remote: &str, patterns: &[String]) -> Option<String> {
    let refs: Vec<(&str, &str)> = ls_remote
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    patterns.iter().find_map(|pattern| {
        refs.iter()
            .find(|(sha, name)| name == pattern && is_full_sha(sha))
            .map(|(sha, _)| (*sha).to_owned())
    })
}

/// Queues the refresh of the current month of every followed list (and of
/// the previous month too in a month's first two days, for late mail).
/// Returns the keys queued.
pub async fn refresh_followed(
    client: &impl GenericClient,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    let lists = client
        .query(
            "SELECT id, name FROM sources
             WHERE kind = 'mailing_list' AND enabled AND (config->>'follow')::boolean",
            &[],
        )
        .await?;
    let mut queued = Vec::new();
    for row in lists {
        let (id, list): (i64, String) = (row.get(0), row.get(1));
        for month in followed_months(now) {
            let key = format!("{list}/{month}");
            let payload = json!({ "list": list, "month": month, "refresh": true });
            let requeued = client
                .query_opt(
                    "INSERT INTO jobs (source_id, kind, key, payload, priority) VALUES ($1, $2, $3, $4, 1)
                     ON CONFLICT (kind, key) DO UPDATE
                         SET state = 'queued', run_after = now(), attempts = 0, last_error = NULL,
                             payload = EXCLUDED.payload, source_id = EXCLUDED.source_id, finished_at = NULL
                         WHERE jobs.state IN ('done', 'failed', 'cancelled')
                     RETURNING id",
                    &[&id, &kind::MBOX_MONTH, &key, &payload],
                )
                .await?;
            if requeued.is_some() {
                queued.push(key);
            }
        }
    }
    Ok(queued)
}

/// The months a followed list refreshes at `now`.
pub fn followed_months(now: DateTime<Utc>) -> Vec<String> {
    let current = format!("{:04}-{:02}", now.year(), now.month());
    if now.day() > 2 {
        return vec![current];
    }
    let (year, month) = if now.month() == 1 {
        (now.year() - 1, 12)
    } else {
        (now.year(), now.month() - 1)
    };
    vec![format!("{year:04}-{month:02}"), current]
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
             ON CONFLICT (kind, name) DO UPDATE
                 SET config = sources.config || EXCLUDED.config, enabled = true, updated_at = now()
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
    let available = match indexer.settings.sources {
        SourcesMode::TestSlices => json!({
            "mode": "test-slices",
            "mailing_lists": [indexer.slices.mail.list],
            "postgres_versions": [{
                "name": indexer.slices.code.version, "branch": indexer.slices.code.branch,
                "sha": indexer.slices.code.sha, "paths": indexer.slices.code.paths,
            }],
            "note": "This deployment retrieves the configured test slices only.",
        }),
        SourcesMode::Open => json!({
            "mode": "open",
            "mailing_lists": "Any list at https://www.postgresql.org/list/, such as pgsql-hackers, \
                pgsql-bugs, pgsql-committers or pgsql-general: chosen months, a range (from, to), or the \
                whole archive; seed threads; and follow, to retrieve the current month again every hour.",
            "postgres_versions": "Any branch, tag or commit of PostgreSQL (https://git.postgresql.org/git/postgresql.git): \
                a major version (19: its REL_19_STABLE branch, or master while it has none) or master, \
                at a branch or tag's current commit or a given sha; the whole tree, or chosen paths.",
        }),
    };
    Ok(json!({ "sources": sources, "available": available }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn months(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| (*m).to_owned()).collect()
    }

    #[test]
    fn selects_months_ranges_and_whole_archives() {
        let archive = months(&["1997-06", "2026-05", "2026-06", "2026-07"]);
        assert_eq!(
            select_months(&archive, None, None, None, false).unwrap(),
            archive
        );
        assert!(
            select_months(&archive, None, None, None, true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            select_months(&archive, None, Some("2026-06"), None, true).unwrap(),
            months(&["2026-06", "2026-07"])
        );
        assert_eq!(
            select_months(&archive, None, None, Some("2026-05"), false).unwrap(),
            months(&["1997-06", "2026-05"])
        );
        let chosen = months(&["2026-07", "2026-05", "2026-07"]);
        assert_eq!(
            select_months(&archive, Some(&chosen), None, None, false).unwrap(),
            months(&["2026-05", "2026-07"])
        );
        let missing = months(&["2026-08"]);
        let error = select_months(&archive, Some(&missing), None, None, false).unwrap_err();
        assert!(error.to_string().contains("1997-06 to 2026-07"), "{error}");
        assert!(select_months(&archive, Some(&chosen), Some("2026-05"), None, false).is_err());
        assert!(select_months(&archive, None, Some("2026-07"), Some("2026-06"), false).is_err());
        assert!(select_months(&archive, None, Some("2025-01"), Some("2025-12"), false).is_err());
        assert!(select_months(&archive, None, Some("2026-6"), None, false).is_err());
        assert!(select_months(&[], None, None, None, false).is_err());
    }

    #[test]
    fn follows_the_current_month_and_late_mail() {
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(followed_months(at("2026-10-09T12:00:00Z")), ["2026-10"]);
        assert_eq!(
            followed_months(at("2026-10-02T23:00:00Z")),
            ["2026-09", "2026-10"]
        );
        assert_eq!(
            followed_months(at("2027-01-01T00:30:00Z")),
            ["2026-12", "2027-01"]
        );
    }

    #[test]
    fn code_comes_only_from_postgresql() {
        assert_eq!(official_repo(None).unwrap(), OFFICIAL_REPOS[0]);
        assert_eq!(
            official_repo(Some("https://github.com/postgres/postgres")).unwrap(),
            OFFICIAL_REPOS[1]
        );
        assert!(official_repo(Some("https://github.com/someone/postgres.git")).is_err());
        assert!(validate_ref("REL_19_STABLE").is_ok());
        assert!(validate_ref("--upload-pack=x").is_err());
        assert!(validate_ref("a..b").is_err());
        assert_eq!(stable_branch("19"), "REL_19_STABLE");
        assert_eq!(stable_branch("master"), "master");
    }

    #[tokio::test]
    #[ignore = "runs git ls-remote against git.postgresql.org"]
    async fn resolves_branches_and_tags_of_postgresql() {
        let url = OFFICIAL_REPOS[0];
        let stable = resolve_ref(url, "REL_18_STABLE").await.unwrap();
        assert!(stable.is_some_and(|sha| is_full_sha(&sha)));
        // An annotated tag resolves to its commit.
        let tag = resolve_ref(url, "REL_18_0").await.unwrap().unwrap();
        assert_eq!(tag.len(), 40);
        assert!(resolve_ref(url, "REL_99_STABLE").await.unwrap().is_none());
    }

    #[test]
    fn picks_a_tags_commit_over_the_tag_and_branches() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let output = format!(
            "{a}\trefs/heads/REL_18_0\n{b}\trefs/tags/REL_18_0\n{c}\trefs/tags/REL_18_0^{{}}\n"
        );
        let patterns = [
            "refs/tags/REL_18_0^{}".to_owned(),
            "refs/tags/REL_18_0".to_owned(),
            "refs/heads/REL_18_0".to_owned(),
        ];
        assert_eq!(pick_ref(&output, &patterns), Some(c));
        assert_eq!(
            pick_ref(&format!("{a}\trefs/heads/REL_18_0\n"), &patterns),
            Some(a)
        );
        assert_eq!(pick_ref("", &patterns), None);
    }
}
