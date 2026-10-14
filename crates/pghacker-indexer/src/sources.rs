//! Sources: the mailing lists the indexer retrieves, and the PostgreSQL
//! versions of the commit history. Adding a list queues the jobs that
//! retrieve it; adding a version loads nothing, since the history holds every
//! branch, tag and commit, and names the commit it has ([`in_history`]).
//!
//! `PGHACKER_SOURCES` decides which lists can be added. `test-slices`, the
//! default, retrieves only the configured test slices. `open` retrieves any
//! list of the postgresql.org archive (chosen months, a range, or the whole
//! archive, plus seed threads, and optionally following the list: its current
//! month is retrieved again every hour).

use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use crate::Indexer;
use crate::jobs::{self, kind};

/// The jobs listed in add_source's reply; the rest are counted.
const LISTED_JOBS: usize = 20;

/// Which lists add_source may add: `PGHACKER_SOURCES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourcesMode {
    /// The configured test slices only.
    #[default]
    TestSlices,
    /// Any list of the archive.
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
    /// postgres_version: the branch or tag to look up in the commit history
    /// (default: REL_<name>_STABLE, or master while the version has no
    /// stable branch).
    #[serde(default)]
    pub branch: Option<String>,
    /// postgres_version: a full commit SHA to look up instead of a branch or
    /// tag's current commit.
    #[serde(default)]
    pub sha: Option<String>,
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
}

/// A job to queue.
struct PlannedJob {
    kind: &'static str,
    key: String,
    payload: Value,
    priority: i32,
}

/// What adding a list does: its configuration (merged into the stored one),
/// the jobs it queues, and what the reply says about them.
struct Plan {
    config: Value,
    jobs: Vec<PlannedJob>,
    summary: Value,
}

pub async fn add_source(indexer: &Indexer, args: AddSourceArgs) -> Result<Value> {
    let name = args.name.trim().to_owned();
    ensure!(!name.is_empty(), "name is empty");
    let source_kind = args.kind;
    if source_kind == SourceKind::PostgresVersion {
        let client = indexer.tools.get().await?;
        let schema = pghacker_core::code::history_schema();
        return in_history(&**client, schema, &name, &args).await;
    }
    let plan = match indexer.settings.sources {
        SourcesMode::TestSlices => test_list_plan(indexer, &name, &args)?,
        SourcesMode::Open => list_plan(indexer, &name, &args).await?,
    };

    let mut client = indexer.tools.get().await?;
    let tx = client.transaction().await?;
    let source_id = upsert(&*tx, source_kind, &name, &plan.config).await?;
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

/// A major version's stable branch: `REL_19_STABLE`; master's is master.
fn stable_branch(name: &str) -> String {
    if name == "master" {
        "master".to_owned()
    } else {
        format!("REL_{name}_STABLE")
    }
}

/// A PostgreSQL version, from the commit history in `schema` that the code
/// tools read: it holds every branch, tag and commit, so nothing is loaded.
/// The commit the version names (its `sha`, else its `branch`, else its
/// stable branch, else master), or, when the history lacks it, the history
/// sync is asked to run now.
pub async fn in_history(
    client: &impl GenericClient,
    schema: &str,
    name: &str,
    args: &AddSourceArgs,
) -> Result<Value> {
    ensure!(
        name == "master" || (name.len() <= 2 && name.bytes().all(|b| b.is_ascii_digit())),
        "name must be a major version, such as 19, or master"
    );
    let s = pghacker_code::history::store::readable_schema(schema)?;
    let history = client
        .query_opt(&format!("SELECT url, paths FROM {s}.code_history"), &[])
        .await
        .with_context(|| format!("cannot read the commit history in schema {s}"))?
        .with_context(|| format!("schema {s} has no commit history installed"))?;
    let (url, paths): (Option<String>, Vec<String>) = (history.get(0), history.get(1));
    let (wanted, found) = match (&args.sha, &args.branch) {
        (Some(sha), _) => {
            let sha = sha.trim().to_ascii_lowercase();
            ensure!(
                is_full_sha(&sha),
                "sha must be a full 40-character commit SHA"
            );
            let found = client
                .query_opt(
                    &format!("SELECT sha, sha, committed_at FROM {s}.code_commits WHERE sha = $1"),
                    &[&sha],
                )
                .await?;
            (format!("commit {sha}"), found)
        }
        (None, branch) => {
            let refs = match branch {
                Some(branch) => {
                    validate_ref(branch)?;
                    vec![branch.clone()]
                }
                None => vec![stable_branch(name), "master".to_owned()],
            };
            let found = client
                .query_opt(
                    &format!(
                        "SELECT r.name, c.sha, c.committed_at FROM {s}.code_refs r \
                         JOIN {s}.code_commits c ON c.addr = r.addr WHERE r.name = ANY($1) \
                         ORDER BY array_position($1, r.name), r.kind LIMIT 1"
                    ),
                    &[&refs],
                )
                .await?;
            (refs[0].clone(), found)
        }
    };
    let whole = |paths: &[String]| {
        if paths.is_empty() {
            json!("the whole tree")
        } else {
            json!(paths)
        }
    };
    match found {
        Some(row) => {
            let (revision, sha, at): (String, String, DateTime<Utc>) =
                (row.get(0), row.get(1), row.get(2));
            Ok(json!({
                "covered": true,
                "revision": revision,
                "sha": sha,
                "committed_at": at,
                "paths": whole(&paths),
                "jobs_queued": 0,
                "note": format!(
                    "The commit history already holds every branch, tag and commit of PostgreSQL, \
                     so nothing is loaded: the code tools read {revision} at {}.", &sha[..12]
                ),
                "next": "Name the version (a major version, release, branch, tag or commit SHA) \
                         in search, grep, read_file, list_files or blame.",
            }))
        }
        None => {
            jobs::wake(client, kind::CODE_HISTORY_SYNC).await?;
            Ok(json!({
                "covered": false,
                "jobs_queued": 0,
                "note": format!(
                    "The commit history has no {wanted}. Its sync runs now, fetching every new \
                     branch, tag and commit from {}; if {wanted} is there, the code tools read it \
                     afterwards. list_sources shows the last sync.",
                    url.as_deref().unwrap_or("its clone")
                ),
            }))
        }
    }
}

/// A full, lower-case commit SHA.
fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
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
    let mut client = indexer.tools.get().await?;
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
    tx.commit().await?;
    Ok(json!({
        "source": { "id": id, "kind": args.kind.as_str(), "name": args.name, "enabled": false },
        "jobs_cancelled": cancelled,
    }))
}

pub async fn list_sources(indexer: &Indexer) -> Result<Value> {
    let client = indexer.tools.get().await?;
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
    let versions = "Any branch, tag or commit of PostgreSQL in the commit history (this reply's history): \
        a major version (19: its REL_19_STABLE branch, or master while it has none) or master, at a branch \
        or tag's current commit or a given sha. Nothing is loaded: add_source names the commit, or starts \
        the history's sync when the history lacks it.";
    let available = match indexer.settings.sources {
        SourcesMode::TestSlices => json!({
            "mode": "test-slices",
            "mailing_lists": [indexer.slices.mail.list],
            "postgres_versions": versions,
            "note": "This deployment retrieves the configured test slices of mail only.",
        }),
        SourcesMode::Open => json!({
            "mode": "open",
            "mailing_lists": "Any list at https://www.postgresql.org/list/, such as pgsql-hackers, \
                pgsql-bugs, pgsql-committers or pgsql-general: chosen months, a range (from, to), or the \
                whole archive; seed threads; and follow, to retrieve the current month again every hour.",
            "postgres_versions": versions,
        }),
    };
    let history = crate::history::summary(&**client).await?;
    Ok(json!({ "sources": sources, "history": history, "available": available }))
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
    fn versions_name_valid_refs_and_shas() {
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha("0123456789ABCDEF0123456789abcdef01234567"));
        assert!(!is_full_sha("0123456"));
        assert!(validate_ref("REL_19_STABLE").is_ok());
        assert!(validate_ref("--upload-pack=x").is_err());
        assert!(validate_ref("a..b").is_err());
        assert_eq!(stable_branch("19"), "REL_19_STABLE");
        assert_eq!(stable_branch("master"), "master");
    }
}
