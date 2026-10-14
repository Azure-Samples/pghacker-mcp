//! The extraction workflow crawler sessions follow: lease threads, read
//! them, draft features and decisions, match the drafts against existing
//! extracts, decide to reuse or create, and submit. The tools hold facts to
//! the record: citations must be verbatim quotes from the thread's messages,
//! a near-duplicate needs a reason, and nothing is half-applied.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use pgvector::HalfVector;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;
use uuid::Uuid;

use pghacker_core::text::{quote_in, truncate_chars};

use crate::Indexer;

/// Characters of messages per page, across messages.
const PAGE_CHARS: usize = 48_000;
/// Characters of one message's text in a page.
const MESSAGE_CHARS: usize = 8_000;
/// A new extract at or above these similarities to an existing one needs `distinct_because`.
pub const SEMANTIC_DUPLICATE: f64 = 0.85;
pub const NAME_DUPLICATE: f64 = 0.6;
/// How long a find_duplicates call waits for room before it fails as busy.
const FIND_DUPLICATES_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextBatchArgs {
    /// How many threads to lease, 1 to 3. Default 1.
    #[serde(default)]
    pub threads: Option<u32>,
    /// Lease these threads instead of the next pending ones.
    #[serde(default)]
    pub thread_ids: Option<Vec<i64>>,
    /// The lease's length in minutes, 5 to 180. Default 20. Every call that
    /// uses the lease renews it for at least 20 minutes more.
    #[serde(default)]
    pub lease_minutes: Option<u32>,
    /// Your PilotSwarm session ID, recorded with what you submit.
    #[serde(default)]
    pub session: Option<String>,
    /// Your model, recorded with what you submit.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMessagesArgs {
    pub lease_token: String,
    pub thread_id: i64,
    /// The `next_offset` of the previous page.
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DraftKind {
    Feature,
    Decision,
}

impl DraftKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Decision => "decision",
        }
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct DraftIn {
    pub thread_id: i64,
    pub kind: DraftKind,
    /// A feature's name, or a decision's title.
    pub name: String,
    /// What the feature does, or the decision's outcome and rationale, in a sentence or two.
    pub description: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MatchArgs {
    pub lease_token: String,
    pub drafts: Vec<DraftIn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Use an existing extract: one of the draft's candidates.
    Reuse,
    /// Create a new extract.
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    Proposed,
    InProgress,
    Committed,
    Reverted,
    Rejected,
    Returned,
    Withdrawn,
    Unknown,
}

impl FeatureStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::InProgress => "in_progress",
            Self::Committed => "committed",
            Self::Reverted => "reverted",
            Self::Rejected => "rejected",
            Self::Returned => "returned",
            Self::Withdrawn => "withdrawn",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    Proposes,
    Supports,
    Opposes,
    Concerned,
    Neutral,
    Reviews,
}

impl Stance {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proposes => "proposes",
            Self::Supports => "supports",
            Self::Opposes => "opposes",
            Self::Concerned => "concerned",
            Self::Neutral => "neutral",
            Self::Reviews => "reviews",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CitationIn {
    /// The cited message's Message-ID, from this thread.
    pub message_id: String,
    /// A verbatim quote from that message, at least 12 characters.
    pub quote: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FeatureIn {
    /// The draft_id that match_extracts returned for this feature.
    pub draft_id: i64,
    pub action: Action,
    /// reuse: the existing feature's id.
    #[serde(default)]
    pub existing_id: Option<i64>,
    /// new: a unique lowercase-hyphenated slug, such as sql-pgq.
    #[serde(default)]
    pub slug: Option<String>,
    pub name: String,
    pub description: String,
    pub status: FeatureStatus,
    /// The major version it targeted or shipped in, such as "19".
    #[serde(default)]
    pub release: Option<String>,
    /// Paths of files whose changes implement it, from this thread's patches or file anchors.
    #[serde(default)]
    pub files: Vec<String>,
    pub citations: Vec<CitationIn>,
    /// new: why it differs from a near-duplicate candidate.
    #[serde(default)]
    pub distinct_because: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionIn {
    /// The draft_id that match_extracts returned for this decision.
    pub draft_id: i64,
    pub action: Action,
    /// reuse: the existing decision's id.
    #[serde(default)]
    pub existing_id: Option<i64>,
    /// The feature it decides: a slug from this submission or an existing feature's.
    #[serde(default)]
    pub feature_slug: Option<String>,
    pub title: String,
    pub outcome: String,
    #[serde(default)]
    pub alternatives: Option<String>,
    pub rationale: String,
    /// When it was decided (ISO 8601); defaults to the first cited message's date.
    #[serde(default)]
    pub decided_at: Option<DateTime<Utc>>,
    pub citations: Vec<CitationIn>,
    /// new: why it differs from a near-duplicate candidate.
    #[serde(default)]
    pub distinct_because: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PositionIn {
    /// The author holding it: an author_id from the thread's participants.
    pub author_id: i64,
    /// What it's about: a feature slug (from this submission or existing) ...
    #[serde(default)]
    pub feature_slug: Option<String>,
    /// ... or a decision in this submission, by its draft_id ...
    #[serde(default)]
    pub decision_draft_id: Option<i64>,
    /// ... or an existing decision's id.
    #[serde(default)]
    pub decision_id: Option<i64>,
    pub stance: Stance,
    /// The author's position in a sentence or two.
    pub summary: String,
    /// Quotes from the author's own messages.
    pub citations: Vec<CitationIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitArgs {
    pub lease_token: String,
    pub thread_id: i64,
    /// Instead of extracts: why the thread has nothing to extract (no technical discussion).
    #[serde(default)]
    pub skip_reason: Option<String>,
    #[serde(default)]
    pub features: Vec<FeatureIn>,
    #[serde(default)]
    pub decisions: Vec<DecisionIn>,
    #[serde(default)]
    pub positions: Vec<PositionIn>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeAuthorsArgs {
    pub from_id: i64,
    pub into_id: i64,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeExtractsArgs {
    pub kind: DraftKind,
    pub from_id: i64,
    pub into_id: i64,
    pub reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UndoMergeArgs {
    /// The merge_id that merge_authors or merge_extracts returned.
    pub merge_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDuplicatesArgs {
    pub kind: DraftKind,
    /// Minimum semantic similarity, 0.8 to 1. Default 0.85.
    #[serde(default)]
    pub min_similarity: Option<f64>,
    /// Pairs to list, 1 to 100. Default 20.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MarkDistinctArgs {
    pub kind: DraftKind,
    pub a_id: i64,
    pub b_id: i64,
    /// One sentence: what differs.
    pub reason: String,
    /// Your PilotSwarm session ID, recorded with the verdict.
    #[serde(default)]
    pub session: Option<String>,
    /// Your model, recorded with the verdict.
    #[serde(default)]
    pub model: Option<String>,
}

struct Lease {
    thread_ids: Vec<i64>,
    session: Option<String>,
    model: Option<String>,
}

/// How long a lease lasts by default, and how far each use renews it: a
/// working session keeps its threads, and an abandoned lease lapses soon.
const LEASE_MINUTES: i32 = 20;

async fn lease(client: &impl GenericClient, token: &str) -> Result<Lease> {
    let token = lease_token(token)?;
    if let Some(row) = client
        .query_opt(
            "UPDATE extraction_leases
             SET expires_at = greatest(expires_at, now() + make_interval(mins => $2))
             WHERE token = $1 AND released_at IS NULL AND expires_at > now()
             RETURNING thread_ids, session, model",
            &[&token, &LEASE_MINUTES],
        )
        .await?
    {
        return Ok(Lease {
            thread_ids: row.get(0),
            session: row.get(1),
            model: row.get(2),
        });
    }
    let released: Option<bool> = client
        .query_opt(
            "SELECT released_at IS NOT NULL FROM extraction_leases WHERE token = $1",
            &[&token],
        )
        .await?
        .map(|row| row.get(0));
    Err(match released {
        None => anyhow!("there's no such lease"),
        Some(true) => anyhow!("the lease was released; call next_extraction_batch"),
        Some(false) => anyhow!("the lease expired; call next_extraction_batch"),
    })
}

fn lease_token(token: &str) -> Result<Uuid> {
    token
        .parse()
        .map_err(|_| anyhow!("lease_token isn't a lease token"))
}

/// Leases the next threads that need extraction, with their first page of messages.
pub async fn next_batch(indexer: &Indexer, args: NextBatchArgs) -> Result<Value> {
    let count = i64::from(args.threads.unwrap_or(1).clamp(1, 3));
    let minutes = f64::from(
        args.lease_minutes
            .unwrap_or(LEASE_MINUTES.unsigned_abs())
            .clamp(5, 180),
    );
    let mut client = indexer.tools.get().await?;
    let tx = client.transaction().await?;
    tx.batch_execute("SELECT pg_advisory_xact_lock(hashtext('pghacker.extraction_leases'))")
        .await?;
    let free = "NOT EXISTS (SELECT FROM extraction_leases l
                            WHERE t.id = ANY (l.thread_ids) AND l.released_at IS NULL AND l.expires_at > now())";
    let ids: Vec<i64> = match &args.thread_ids {
        Some(ids) if !ids.is_empty() => tx
            .query(
                &format!("SELECT t.id FROM threads t WHERE t.id = ANY ($1) AND {free} ORDER BY t.id"),
                &[ids],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect(),
        _ => tx
            .query(
                &format!(
                    "SELECT t.id FROM threads t
                     WHERE (t.extraction_state = 'pending'
                            OR (t.extraction_state = 'extracted' AND t.extracted_version < t.content_version))
                       AND t.message_count > 0 AND {free}
                     ORDER BY (t.extraction_state = 'pending') DESC, t.message_count DESC, t.id
                     LIMIT $1"
                ),
                &[&count],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect(),
    };
    if ids.is_empty() {
        tx.commit().await?;
        return Ok(json!({
            "lease_token": null,
            "threads": [],
            "done": true,
            "message": "No threads need extraction now. Check ingest_status: retrieval may still be running.",
        }));
    }
    let token = Uuid::new_v4();
    let expires: DateTime<Utc> = tx
        .query_one(
            "INSERT INTO extraction_leases (token, thread_ids, session, model, expires_at)
             VALUES ($1, $2, $3, $4, now() + make_interval(mins => $5::int)) RETURNING expires_at",
            &[&token, &ids, &args.session, &args.model, &(minutes as i32)],
        )
        .await?
        .get(0);
    tx.commit().await?;
    let mut threads = Vec::new();
    for id in &ids {
        threads.push(thread_overview(&**client, *id).await?);
    }
    Ok(json!({
        "lease_token": token.to_string(),
        "expires_at": expires,
        "threads": threads,
        "next": "Read each thread (read_messages for later pages), draft its features and decisions, \
                 call match_extracts, decide reuse or new for each draft, then submit_extracts per thread.",
    }))
}

async fn thread_overview(client: &impl GenericClient, thread_id: i64) -> Result<Value> {
    let thread = client
        .query_one(
            "SELECT id, subject, root_message_id, message_count, started_at, last_at, extraction_state
             FROM threads WHERE id = $1",
            &[&thread_id],
        )
        .await?;
    let participants: Vec<Value> = client
        .query(
            "SELECT a.id, a.display_name, count(*) FROM messages m JOIN authors a ON a.id = m.author_id
             WHERE m.thread_id = $1 GROUP BY a.id ORDER BY count(*) DESC, a.id",
            &[&thread_id],
        )
        .await?
        .iter()
        .map(|row| json!({ "author_id": row.get::<_, i64>(0), "name": row.get::<_, String>(1), "messages": row.get::<_, i64>(2) }))
        .collect();
    let existing = existing_extracts(client, thread_id).await?;
    let signals = commit_signals(client, thread_id).await?;
    let page = messages_page(client, thread_id, 0).await?;
    Ok(json!({
        "thread_id": thread.get::<_, i64>(0),
        "subject": thread.get::<_, String>(1),
        "root_message_id": thread.get::<_, String>(2),
        "message_count": thread.get::<_, i32>(3),
        "started_at": thread.get::<_, Option<DateTime<Utc>>>(4),
        "last_at": thread.get::<_, Option<DateTime<Utc>>>(5),
        "previously": thread.get::<_, String>(6),
        "participants": participants,
        "existing_extracts": existing,
        "status_signals": signals,
        "messages": page["messages"],
        "next_offset": page["next_offset"],
    }))
}

/// The thread's lines that report a commit, push, revert, rejection or
/// CommitFest return, latest last: deterministic evidence for statuses that a
/// long thread's last pages settle.
async fn commit_signals(client: &impl GenericClient, thread_id: i64) -> Result<Vec<Value>> {
    let rows = client
        .query(
            r"SELECT message_id, author, sent_at, line FROM (
                  SELECT m.message_id, coalesce(a.display_name, m.from_name) AS author, m.sent_at,
                         left(regexp_replace(substring(m.body_clean from
                             '(?i)[^\n]*\m(?:committed|pushed|reverted|reverting|rejected|returned with feedback)\M[^\n]*'),
                             '\s+', ' ', 'g'), 200) AS line
                  FROM messages m LEFT JOIN authors a ON a.id = m.author_id
                  WHERE m.thread_id = $1
                    AND m.body_clean ~* '\m(committed|pushed|reverted|reverting|rejected|returned with feedback)\M'
                  ORDER BY m.sent_at DESC NULLS LAST LIMIT 12
              ) x ORDER BY sent_at NULLS FIRST",
            &[&thread_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            json!({
                "message_id": row.get::<_, String>(0),
                "author": row.get::<_, String>(1),
                "sent_at": row.get::<_, Option<DateTime<Utc>>>(2),
                "line": row.get::<_, Option<String>>(3),
            })
        })
        .collect())
}

async fn existing_extracts(client: &impl GenericClient, thread_id: i64) -> Result<Value> {
    let features: Vec<Value> = client
        .query(
            "SELECT f.id, f.slug, f.name, f.status, f.release FROM feature_threads ft
             JOIN features f ON f.id = ft.feature_id WHERE ft.thread_id = $1 ORDER BY f.id",
            &[&thread_id],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "id": row.get::<_, i64>(0), "slug": row.get::<_, String>(1), "name": row.get::<_, String>(2),
                    "status": row.get::<_, String>(3), "release": row.get::<_, Option<String>>(4) })
        })
        .collect();
    let decisions: Vec<Value> = client
        .query(
            "SELECT id, title, outcome FROM decisions WHERE thread_id = $1 AND merged_into IS NULL ORDER BY id",
            &[&thread_id],
        )
        .await?
        .iter()
        .map(|row| json!({ "id": row.get::<_, i64>(0), "title": row.get::<_, String>(1), "outcome": row.get::<_, String>(2) }))
        .collect();
    let positions: i64 = client
        .query_one(
            "SELECT count(*) FROM positions WHERE thread_id = $1",
            &[&thread_id],
        )
        .await?
        .get(0);
    Ok(json!({ "features": features, "decisions": decisions, "positions": positions }))
}

/// A page of the thread's messages from `offset`, within the page budget.
async fn messages_page(client: &impl GenericClient, thread_id: i64, offset: u32) -> Result<Value> {
    let rows = client
        .query(
            "SELECT m.id, m.message_id, m.author_id, coalesce(a.display_name, m.from_name), m.sent_at,
                    m.subject, m.parent_message_id, m.body_clean, m.has_patch
             FROM messages m LEFT JOIN authors a ON a.id = m.author_id
             WHERE m.thread_id = $1 ORDER BY m.sent_at NULLS LAST, m.id OFFSET $2",
            &[&thread_id, &i64::from(offset)],
        )
        .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
    let mut patches: HashMap<i64, Vec<Value>> = HashMap::new();
    for row in client
        .query(
            "SELECT message_id, filename, paths, added, removed FROM patches WHERE message_id = ANY ($1) ORDER BY id",
            &[&ids],
        )
        .await?
    {
        patches.entry(row.get(0)).or_default().push(json!({
            "filename": row.get::<_, String>(1), "paths": row.get::<_, Vec<String>>(2),
            "added": row.get::<_, i32>(3), "removed": row.get::<_, i32>(4),
        }));
    }
    let mut anchors: HashMap<i64, Vec<Value>> = HashMap::new();
    for row in client
        .query(
            "SELECT message_id, kind, value FROM anchors WHERE message_id = ANY ($1) ORDER BY id",
            &[&ids],
        )
        .await?
    {
        anchors
            .entry(row.get(0))
            .or_default()
            .push(json!({ "kind": row.get::<_, String>(1), "value": row.get::<_, String>(2) }));
    }
    let mut messages = Vec::new();
    let mut used = 0;
    let mut next_offset = None;
    let mut previous_subject: Option<String> = None;
    for (index, row) in rows.iter().enumerate() {
        let text: String = row.get(7);
        let text = truncate_chars(&text, MESSAGE_CHARS);
        if !messages.is_empty() && used + text.len() > PAGE_CHARS {
            next_offset = Some(offset as usize + index);
            break;
        }
        used += text.len();
        let id: i64 = row.get(0);
        let subject: String = row.get(5);
        let subject_changed = previous_subject.as_deref() != Some(subject.as_str());
        previous_subject = Some(subject.clone());
        messages.push(json!({
            "message_id": row.get::<_, String>(1),
            "author_id": row.get::<_, Option<i64>>(2),
            "author": row.get::<_, String>(3),
            "sent_at": row.get::<_, Option<DateTime<Utc>>>(4),
            "subject": subject_changed.then_some(subject),
            "in_reply_to": row.get::<_, Option<String>>(6),
            "text": text,
            "text_truncated": text.len() < row.get::<_, String>(7).len(),
            "patches": patches.remove(&id).unwrap_or_default(),
            "anchors": anchors.remove(&id).unwrap_or_default(),
        }));
    }
    Ok(json!({ "messages": messages, "next_offset": next_offset }))
}

pub async fn read_messages(indexer: &Indexer, args: ReadMessagesArgs) -> Result<Value> {
    let client = indexer.tools.get().await?;
    let lease = lease(&**client, &args.lease_token).await?;
    if !lease.thread_ids.contains(&args.thread_id) {
        return Err(anyhow!("thread {} isn't in this lease", args.thread_id));
    }
    let mut page = messages_page(&**client, args.thread_id, args.offset).await?;
    page["thread_id"] = json!(args.thread_id);
    Ok(page)
}

/// A candidate existing extract for a draft.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub id: i64,
    pub label: String,
    pub summary: String,
    pub status: Option<String>,
    pub release: Option<String>,
    /// Cosine similarity of the embeddings, when both have one.
    pub semantic: Option<f64>,
    /// Trigram similarity of the names.
    pub name_similarity: f64,
}

impl Candidate {
    pub fn near_duplicate(&self) -> bool {
        self.semantic.is_some_and(|s| s >= SEMANTIC_DUPLICATE)
            || self.name_similarity >= NAME_DUPLICATE
    }
}

/// Existing features or decisions like a draft: by semantic search, words and name.
async fn candidates(
    client: &impl GenericClient,
    kind: DraftKind,
    name: &str,
    text: &str,
    embedding: Option<&HalfVector>,
) -> Result<Vec<Candidate>> {
    let (table, label, summary, extra) = match kind {
        DraftKind::Feature => ("features", "name", "description", "status, release"),
        DraftKind::Decision => ("decisions", "title", "outcome", "NULL::text, NULL::text"),
    };
    let mut ids: Vec<i64> = client
        .query(
            &format!(
                "SELECT id FROM {table} WHERE merged_into IS NULL
                 ORDER BY similarity({label}, $1) DESC, id LIMIT 5"
            ),
            &[&name],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let index = format!("pghacker.{table}_bm25");
    ids.extend(
        client
            .query(
                &format!(
                    "SELECT id FROM {table} WHERE merged_into IS NULL
                     ORDER BY search_text <@> to_bm25query($1, $2) LIMIT 5"
                ),
                &[&text, &index],
            )
            .await?
            .iter()
            .map(|row| row.get::<_, i64>(0)),
    );
    if let Some(vector) = embedding {
        ids.extend(
            client
                .query(
                    &format!(
                        "SELECT id FROM {table} WHERE merged_into IS NULL AND embedding IS NOT NULL
                         ORDER BY embedding <=> $1 LIMIT 5"
                    ),
                    &[vector],
                )
                .await?
                .iter()
                .map(|row| row.get::<_, i64>(0)),
        );
    }
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(*id));
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = client
        .query(
            &format!(
                "SELECT id, {label}, {summary}, {extra}, similarity({label}, $2)::float8,
                        CASE WHEN $3::halfvec IS NOT NULL AND embedding IS NOT NULL
                             THEN 1 - (embedding <=> $3::halfvec) END::float8
                 FROM {table} WHERE id = ANY ($1)"
            ),
            &[&ids, &name, &embedding],
        )
        .await?;
    let mut list: Vec<Candidate> = rows
        .iter()
        .map(|row| Candidate {
            id: row.get(0),
            label: row.get(1),
            summary: truncate_chars(&row.get::<_, String>(2), 400).to_owned(),
            status: row.get(3),
            release: row.get(4),
            name_similarity: row.get::<_, Option<f64>>(5).unwrap_or(0.0),
            semantic: row.get(6),
        })
        .collect();
    list.sort_by(|a, b| {
        let score = |c: &Candidate| c.semantic.unwrap_or(0.0).max(c.name_similarity);
        score(b).total_cmp(&score(a))
    });
    list.truncate(5);
    Ok(list)
}

pub async fn match_extracts(indexer: &Indexer, args: MatchArgs) -> Result<Value> {
    if args.drafts.is_empty() || args.drafts.len() > 40 {
        return Err(anyhow!("send 1 to 40 drafts"));
    }
    let client = indexer.tools.get().await?;
    let lease_record = lease(&**client, &args.lease_token).await?;
    let token = lease_token(&args.lease_token)?;
    for draft in &args.drafts {
        if !lease_record.thread_ids.contains(&draft.thread_id) {
            return Err(anyhow!("thread {} isn't in this lease", draft.thread_id));
        }
        if draft.name.trim().is_empty() || draft.description.trim().is_empty() {
            return Err(anyhow!("every draft needs a name and a description"));
        }
    }
    let texts: Vec<String> = args
        .drafts
        .iter()
        .map(|draft| format!("{}\n{}", draft.name.trim(), draft.description.trim()))
        .collect();
    let embeddings: Vec<Option<HalfVector>> = match &indexer.embedder {
        Some(embedder) => embedder
            .embed(&texts)
            .await?
            .into_iter()
            .map(Some)
            .collect(),
        None => vec![None; texts.len()],
    };
    let mut out = Vec::new();
    for ((draft, text), embedding) in args.drafts.iter().zip(&texts).zip(&embeddings) {
        let found = candidates(
            &**client,
            draft.kind,
            draft.name.trim(),
            text,
            embedding.as_ref(),
        )
        .await?;
        let id: i64 = client
            .query_one(
                "INSERT INTO drafts (lease_token, thread_id, kind, draft, embedding, candidates)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
                &[
                    &token,
                    &draft.thread_id,
                    &draft.kind.as_str(),
                    &serde_json::to_value(draft)?,
                    embedding,
                    &serde_json::to_value(&found)?,
                ],
            )
            .await?
            .get(0);
        out.push(json!({
            "draft_id": id,
            "kind": draft.kind.as_str(),
            "name": draft.name,
            "near_duplicate": found.iter().any(Candidate::near_duplicate),
            "candidates": found.iter().map(|c| json!({
                "id": c.id, "name": c.label, "summary": c.summary, "status": c.status, "release": c.release,
                "semantic": c.semantic.map(|s| (s * 1000.0).round() / 1000.0),
                "name_similarity": (c.name_similarity * 1000.0).round() / 1000.0,
                "near_duplicate": c.near_duplicate(),
            })).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({
        "drafts": out,
        "rule": format!(
            "For each draft, reuse a candidate that is the same feature or decision (action reuse, existing_id), \
             or create it (action new). A new extract whose candidate is a near duplicate (semantic >= {SEMANTIC_DUPLICATE} \
             or name similarity >= {NAME_DUPLICATE}) needs distinct_because."
        ),
    }))
}

/// A cited message, by Message-ID, in the thread.
struct ThreadMessage {
    id: i64,
    author_id: Option<i64>,
    body: String,
    sent_at: Option<DateTime<Utc>>,
}

static SLUG: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new("^[a-z0-9]+(-[a-z0-9]+)*$").unwrap());

struct Draft {
    thread_id: i64,
    kind: String,
    candidates: Vec<Candidate>,
}

/// Validates and applies one thread's extracts, all or nothing. Errors list
/// every problem so the caller can fix them and resubmit.
pub async fn submit(indexer: &Indexer, args: SubmitArgs) -> Result<Value> {
    let mut client = indexer.tools.get().await?;
    let lease_record = lease(&**client, &args.lease_token).await?;
    let token = lease_token(&args.lease_token)?;
    if !lease_record.thread_ids.contains(&args.thread_id) {
        return Err(anyhow!("thread {} isn't in this lease", args.thread_id));
    }
    let thread_version: i32 = client
        .query_one(
            "SELECT content_version FROM threads WHERE id = $1",
            &[&args.thread_id],
        )
        .await?
        .get(0);

    if let Some(reason) = &args.skip_reason {
        if !args.features.is_empty() || !args.decisions.is_empty() || !args.positions.is_empty() {
            return Err(anyhow!("send either skip_reason or extracts, not both"));
        }
        if reason.trim().len() < 10 {
            return Err(anyhow!(
                "skip_reason must say why the thread has nothing to extract"
            ));
        }
        client
            .execute(
                "UPDATE threads SET extraction_state = 'skipped', extracted_version = $2, extracted_at = now()
                 WHERE id = $1",
                &[&args.thread_id, &thread_version],
            )
            .await?;
        release_if_done(&**client, token).await?;
        return Ok(json!({ "accepted": true, "thread_id": args.thread_id, "skipped": reason }));
    }
    if args.features.is_empty() && args.decisions.is_empty() && args.positions.is_empty() {
        return Err(anyhow!(
            "send extracts, or skip_reason when the thread has none"
        ));
    }

    let messages: HashMap<String, ThreadMessage> = client
        .query(
            "SELECT message_id, id, author_id, body, sent_at FROM messages WHERE thread_id = $1",
            &[&args.thread_id],
        )
        .await?
        .into_iter()
        .map(|row| {
            (
                row.get(0),
                ThreadMessage {
                    id: row.get(1),
                    author_id: row.get(2),
                    body: row.get(3),
                    sent_at: row.get(4),
                },
            )
        })
        .collect();
    let thread_paths: HashSet<String> = client
        .query(
            "SELECT unnest(p.paths) FROM patches p JOIN messages m ON m.id = p.message_id WHERE m.thread_id = $1
             UNION SELECT a.value FROM anchors a JOIN messages m ON m.id = a.message_id
                   WHERE m.thread_id = $1 AND a.kind = 'file'",
            &[&args.thread_id],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let draft_ids: Vec<i64> = args
        .features
        .iter()
        .map(|f| f.draft_id)
        .chain(args.decisions.iter().map(|d| d.draft_id))
        .collect();
    let drafts: HashMap<i64, Draft> = client
        .query(
            "SELECT id, thread_id, kind, candidates FROM drafts WHERE lease_token = $1 AND id = ANY ($2)",
            &[&token, &draft_ids],
        )
        .await?
        .into_iter()
        .map(|row| {
            let candidates: Vec<Candidate> = serde_json::from_value(row.get(3)).unwrap_or_default();
            (
                row.get(0),
                Draft {
                    thread_id: row.get(1),
                    kind: row.get(2),
                    candidates,
                },
            )
        })
        .collect();

    let mut errors: Vec<String> = Vec::new();
    let check_citations = |errors: &mut Vec<String>, what: &str, citations: &[CitationIn]| {
        if citations.is_empty() {
            errors.push(format!("{what}: cite at least one message"));
        }
        for citation in citations {
            let id = citation
                .message_id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            match messages.get(id) {
                None => errors.push(format!("{what}: message {id} isn't in thread {}", args.thread_id)),
                Some(message) if !quote_in(&message.body, &citation.quote) => errors.push(format!(
                    "{what}: the quote \"{}\" isn't verbatim in message {id} (or is shorter than 12 characters)",
                    truncate_chars(&citation.quote, 80)
                )),
                Some(_) => {}
            }
        }
    };
    let check_draft = |errors: &mut Vec<String>,
                       what: &str,
                       draft_id: i64,
                       kind: &str,
                       action: Action,
                       existing: Option<i64>,
                       distinct: &Option<String>| {
        match drafts.get(&draft_id) {
            None => errors.push(format!(
                "{what}: draft {draft_id} isn't from this lease; call match_extracts first"
            )),
            Some(draft) if draft.thread_id != args.thread_id || draft.kind != kind => {
                errors.push(format!(
                    "{what}: draft {draft_id} is a {} draft of thread {}",
                    draft.kind, draft.thread_id
                ));
            }
            Some(draft) => match action {
                Action::Reuse if existing.is_none() => {
                    errors.push(format!("{what}: reuse needs existing_id"))
                }
                Action::New => {
                    if let Some(dup) = draft.candidates.iter().find(|c| c.near_duplicate())
                        && distinct
                            .as_deref()
                            .is_none_or(|reason| reason.trim().len() < 10)
                    {
                        errors.push(format!(
                            "{what}: it resembles existing {kind} {} \"{}\" (semantic {:?}, name {:.2}); \
                             reuse it, or explain in distinct_because why it differs",
                            dup.id, dup.label, dup.semantic.map(|s| (s * 100.0).round() / 100.0), dup.name_similarity
                        ));
                    }
                }
                Action::Reuse => {}
            },
        }
    };

    let mut new_slugs = HashSet::new();
    for (index, feature) in args.features.iter().enumerate() {
        let what = format!("features[{index}] ({})", feature.name);
        check_draft(
            &mut errors,
            &what,
            feature.draft_id,
            "feature",
            feature.action,
            feature.existing_id,
            &feature.distinct_because,
        );
        check_citations(&mut errors, &what, &feature.citations);
        if feature.action == Action::New {
            match feature.slug.as_deref() {
                Some(slug) if SLUG.is_match(slug) && slug.len() <= 60 => {
                    if !new_slugs.insert(slug.to_owned()) {
                        errors.push(format!("{what}: slug {slug} is used twice"));
                    }
                }
                _ => errors.push(format!(
                    "{what}: new needs a lowercase-hyphenated slug of up to 60 characters"
                )),
            }
        }
        for path in &feature.files {
            if !thread_paths.contains(path) {
                errors.push(format!(
                    "{what}: file {path} isn't in this thread's patches or file mentions"
                ));
            }
        }
        if feature.name.trim().is_empty() || feature.description.trim().is_empty() {
            errors.push(format!("{what}: needs a name and a description"));
        }
    }
    for (index, decision) in args.decisions.iter().enumerate() {
        let what = format!("decisions[{index}] ({})", decision.title);
        check_draft(
            &mut errors,
            &what,
            decision.draft_id,
            "decision",
            decision.action,
            decision.existing_id,
            &decision.distinct_because,
        );
        check_citations(&mut errors, &what, &decision.citations);
        if decision.outcome.trim().is_empty() || decision.rationale.trim().is_empty() {
            errors.push(format!("{what}: needs an outcome and a rationale"));
        }
    }
    let decision_drafts: HashSet<i64> = args.decisions.iter().map(|d| d.draft_id).collect();
    for (index, position) in args.positions.iter().enumerate() {
        let what = format!("positions[{index}] (author {})", position.author_id);
        check_citations(&mut errors, &what, &position.citations);
        let own = position.citations.iter().any(|citation| {
            let id = citation
                .message_id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            messages
                .get(id)
                .is_some_and(|m| m.author_id == Some(position.author_id))
        });
        if !own {
            errors.push(format!(
                "{what}: cite at least one of the author's own messages"
            ));
        }
        let targets = [
            position.feature_slug.is_some(),
            position.decision_draft_id.is_some(),
            position.decision_id.is_some(),
        ];
        if targets.iter().filter(|t| **t).count() != 1 {
            errors.push(format!(
                "{what}: give exactly one of feature_slug, decision_draft_id, decision_id"
            ));
        }
        if let Some(draft) = position.decision_draft_id
            && !decision_drafts.contains(&draft)
        {
            errors.push(format!(
                "{what}: decision draft {draft} isn't in this submission"
            ));
        }
        if position.summary.trim().is_empty() {
            errors.push(format!("{what}: needs a summary"));
        }
    }

    // Existing ids and slugs.
    let reuse_features: Vec<i64> = args
        .features
        .iter()
        .filter_map(|f| f.existing_id.filter(|_| f.action == Action::Reuse))
        .collect();
    let existing_features: HashMap<i64, String> = client
        .query(
            "SELECT id, slug FROM features WHERE id = ANY ($1) AND merged_into IS NULL",
            &[&reuse_features],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    for id in &reuse_features {
        if !existing_features.contains_key(id) {
            errors.push(format!("feature {id} doesn't exist or was merged"));
        }
    }
    let taken: Vec<String> = client
        .query(
            "SELECT slug FROM features WHERE slug = ANY ($1)",
            &[&new_slugs.iter().cloned().collect::<Vec<_>>()],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for slug in taken {
        errors.push(format!(
            "slug {slug} already exists: reuse that feature, or pick another slug"
        ));
    }
    let mut slugs: HashSet<String> = new_slugs.clone();
    slugs.extend(existing_features.values().cloned());
    let referenced: Vec<String> = args
        .decisions
        .iter()
        .filter_map(|d| d.feature_slug.clone())
        .chain(args.positions.iter().filter_map(|p| p.feature_slug.clone()))
        .filter(|slug| !slugs.contains(slug))
        .collect();
    let known: HashSet<String> = client
        .query(
            "SELECT slug FROM features WHERE slug = ANY ($1) AND merged_into IS NULL",
            &[&referenced],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for slug in &referenced {
        if !known.contains(slug) {
            errors.push(format!(
                "feature slug {slug} is neither in this submission nor an existing feature"
            ));
        }
    }
    let decision_ids: Vec<i64> = args
        .decisions
        .iter()
        .filter_map(|d| d.existing_id.filter(|_| d.action == Action::Reuse))
        .chain(args.positions.iter().filter_map(|p| p.decision_id))
        .collect();
    let known_decisions: HashSet<i64> = client
        .query(
            "SELECT id FROM decisions WHERE id = ANY ($1) AND merged_into IS NULL",
            &[&decision_ids],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for id in &decision_ids {
        if !known_decisions.contains(id) {
            errors.push(format!("decision {id} doesn't exist or was merged"));
        }
    }
    if !errors.is_empty() {
        return Ok(json!({ "accepted": false, "errors": errors,
                          "next": "Fix every error and call submit_extracts again; nothing was saved." }));
    }

    // Embeddings for what's new, before the transaction.
    let mut texts = Vec::new();
    for feature in args.features.iter().filter(|f| f.action == Action::New) {
        texts.push(format!(
            "{}\n{}",
            feature.name.trim(),
            feature.description.trim()
        ));
    }
    for decision in args.decisions.iter().filter(|d| d.action == Action::New) {
        texts.push(format!(
            "{}\n{}\n{}\n{}",
            decision.title.trim(),
            decision.outcome.trim(),
            decision.rationale.trim(),
            decision.alternatives.as_deref().unwrap_or("").trim()
        ));
    }
    for position in &args.positions {
        texts.push(position.summary.trim().to_owned());
    }
    let mut vectors: std::vec::IntoIter<Option<HalfVector>> = match &indexer.embedder {
        Some(embedder) if !texts.is_empty() => embedder
            .embed(&texts)
            .await
            .map(|v| v.into_iter().map(Some).collect::<Vec<_>>())
            .unwrap_or_else(|error| {
                tracing::warn!(error = %format!("{error:#}"), "cannot embed extracts now; the backfill will");
                vec![None; texts.len()]
            })
            .into_iter(),
        _ => vec![None; texts.len()].into_iter(),
    };

    let tx = client.transaction().await?;
    tx.batch_execute(
        "CREATE OR REPLACE FUNCTION pg_temp.status_rank(text) RETURNS int LANGUAGE sql IMMUTABLE AS $$
             SELECT CASE $1 WHEN 'unknown' THEN 0 WHEN 'proposed' THEN 1 WHEN 'in_progress' THEN 2
                            WHEN 'returned' THEN 3 WHEN 'withdrawn' THEN 3 WHEN 'rejected' THEN 4
                            WHEN 'committed' THEN 4 WHEN 'reverted' THEN 5 ELSE 0 END
         $$",
    )
    .await?;
    let session = lease_record.session.clone();
    let model = lease_record.model.clone();
    let mut judged_distinct = 0;
    let mut slug_ids: HashMap<String, i64> = existing_features
        .iter()
        .map(|(id, slug)| (slug.clone(), *id))
        .collect();
    let mut created = json!({ "features": [], "decisions": [], "positions": [] });
    let mut reused = json!({ "features": [], "decisions": [] });
    let cite = |kind: &'static str,
                extract_id: i64,
                citations: &[CitationIn]|
     -> Vec<(String, i64, i64, String)> {
        citations
            .iter()
            .map(|c| {
                let id = c
                    .message_id
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>');
                (
                    kind.to_owned(),
                    extract_id,
                    messages[id].id,
                    c.quote.trim().to_owned(),
                )
            })
            .collect()
    };
    let mut citation_rows = Vec::new();
    for feature in &args.features {
        let id = match feature.action {
            Action::New => {
                let slug = feature.slug.clone().expect("validated");
                let embedding = vectors.next().flatten();
                let id: i64 = tx
                    .query_one(
                        "INSERT INTO features (slug, name, description, status, release, embedding, created_by, model)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
                        &[&slug, &feature.name.trim(), &feature.description.trim(), &feature.status.as_str(),
                          &feature.release, &embedding, &session, &model],
                    )
                    .await?
                    .get(0);
                slug_ids.insert(slug.clone(), id);
                created["features"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({ "id": id, "slug": slug }));
                if let Some(reason) = given(&feature.distinct_because)
                    && let Some(draft) = drafts.get(&feature.draft_id)
                {
                    let verdict = Distinct {
                        kind: "feature",
                        reason,
                        judged_by: "extractor",
                        session: session.as_deref(),
                        model: model.as_deref(),
                    };
                    judged_distinct += verdict.record_against(&*tx, id, &draft.candidates).await?;
                }
                id
            }
            Action::Reuse => {
                let id = feature.existing_id.expect("validated");
                // A status only moves forward: a follow-up thread doesn't undo a commit.
                tx.execute(
                    "UPDATE features SET
                         status = CASE WHEN pg_temp.status_rank($2) > pg_temp.status_rank(status) THEN $2 ELSE status END,
                         release = coalesce($3, release), updated_at = now()
                     WHERE id = $1",
                    &[&id, &feature.status.as_str(), &feature.release],
                )
                .await?;
                reused["features"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({ "id": id, "slug": existing_features[&id] }));
                id
            }
        };
        tx.execute(
            "INSERT INTO feature_threads (feature_id, thread_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&id, &args.thread_id],
        )
        .await?;
        for path in &feature.files {
            tx.execute(
                "INSERT INTO feature_files (feature_id, path, thread_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&id, path, &args.thread_id],
            )
            .await?;
        }
        citation_rows.extend(cite("feature", id, &feature.citations));
    }
    let known_slugs: HashMap<String, i64> = tx
        .query(
            "SELECT slug, id FROM features WHERE slug = ANY ($1)",
            &[&referenced],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let feature_id_for = |slug: &Option<String>| -> Option<i64> {
        slug.as_ref()
            .and_then(|s| slug_ids.get(s).or_else(|| known_slugs.get(s)).copied())
    };
    let mut decision_of_draft: HashMap<i64, i64> = HashMap::new();
    for decision in &args.decisions {
        let feature_id = feature_id_for(&decision.feature_slug);
        let decided_at = decision.decided_at.or_else(|| {
            decision.citations.iter().find_map(|c| {
                let id = c
                    .message_id
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>');
                messages.get(id).and_then(|m| m.sent_at)
            })
        });
        let id = match decision.action {
            Action::New => {
                let embedding = vectors.next().flatten();
                let id: i64 = tx
                    .query_one(
                        "INSERT INTO decisions (feature_id, thread_id, title, outcome, alternatives, rationale, decided_at,
                                                embedding, created_by, model)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
                        &[&feature_id, &args.thread_id, &decision.title.trim(), &decision.outcome.trim(),
                          &decision.alternatives.as_deref().unwrap_or("").trim(), &decision.rationale.trim(),
                          &decided_at, &embedding, &session, &model],
                    )
                    .await?
                    .get(0);
                created["decisions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({ "id": id, "title": decision.title }));
                if let Some(reason) = given(&decision.distinct_because)
                    && let Some(draft) = drafts.get(&decision.draft_id)
                {
                    let verdict = Distinct {
                        kind: "decision",
                        reason,
                        judged_by: "extractor",
                        session: session.as_deref(),
                        model: model.as_deref(),
                    };
                    judged_distinct += verdict.record_against(&*tx, id, &draft.candidates).await?;
                }
                id
            }
            Action::Reuse => {
                let id = decision.existing_id.expect("validated");
                if let Some(feature_id) = feature_id {
                    tx.execute(
                        "UPDATE decisions SET feature_id = coalesce(feature_id, $2), updated_at = now() WHERE id = $1",
                        &[&id, &feature_id],
                    )
                    .await?;
                }
                reused["decisions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({ "id": id }));
                id
            }
        };
        decision_of_draft.insert(decision.draft_id, id);
        citation_rows.extend(cite("decision", id, &decision.citations));
    }
    for position in &args.positions {
        let feature_id = feature_id_for(&position.feature_slug);
        let decision_id = position.decision_id.or_else(|| {
            position
                .decision_draft_id
                .and_then(|d| decision_of_draft.get(&d).copied())
        });
        let embedding = vectors.next().flatten();
        let id: i64 = tx
            .query_one(
                "INSERT INTO positions (author_id, thread_id, feature_id, decision_id, stance, summary, embedding,
                                        created_by, model)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
                &[&position.author_id, &args.thread_id, &feature_id, &decision_id, &position.stance.as_str(),
                  &position.summary.trim(), &embedding, &session, &model],
            )
            .await?
            .get(0);
        created["positions"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": id, "author_id": position.author_id }));
        citation_rows.extend(cite("position", id, &position.citations));
    }
    for (kind, extract_id, message_id, quote) in &citation_rows {
        tx.execute(
            "INSERT INTO citations (extract_kind, extract_id, message_id, quote) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
            &[kind, extract_id, message_id, quote],
        )
        .await?;
    }
    tx.execute(
        "UPDATE threads SET extraction_state = 'extracted', extracted_version = $2, extracted_at = now() WHERE id = $1",
        &[&args.thread_id, &thread_version],
    )
    .await?;
    tx.commit().await?;
    release_if_done(&**client, token).await?;
    crate::jobs::wake(&**client, crate::jobs::kind::GRAPH_SYNC).await?;
    crate::jobs::wake(&**client, crate::jobs::kind::DUPLICATES).await?;
    Ok(json!({
        "accepted": true,
        "thread_id": args.thread_id,
        "created": created,
        "reused": reused,
        "citations": citation_rows.len(),
        "judged_distinct": judged_distinct,
    }))
}

/// The text, unless it's missing or blank.
fn given(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// A verdict that two features, or two decisions, are different things.
struct Distinct<'a> {
    kind: &'static str,
    reason: &'a str,
    judged_by: &'static str,
    session: Option<&'a str>,
    model: Option<&'a str>,
}

impl Distinct<'_> {
    /// Records the pair; false when it was already recorded.
    async fn record(&self, client: &impl GenericClient, a: i64, b: i64) -> Result<bool> {
        let inserted = client
            .execute(
                "INSERT INTO distinct_pairs (kind, low_id, high_id, reason, judged_by, session, model)
                 VALUES ($1, least($2::bigint, $3::bigint), greatest($2::bigint, $3::bigint), $4, $5, $6, $7)
                 ON CONFLICT DO NOTHING",
                &[&self.kind, &a, &b, &self.reason, &self.judged_by, &self.session, &self.model],
            )
            .await?;
        Ok(inserted == 1)
    }

    /// Records `id` as different from each of its draft's near-duplicate
    /// candidates, the ones its distinct_because answered. Returns how many
    /// pairs are new.
    async fn record_against(
        &self,
        client: &impl GenericClient,
        id: i64,
        candidates: &[Candidate],
    ) -> Result<u64> {
        let mut recorded = 0;
        for candidate in candidates.iter().filter(|c| c.near_duplicate()) {
            if self.record(client, id, candidate.id).await? {
                recorded += 1;
            }
        }
        Ok(recorded)
    }
}

/// Releases a lease once each of its threads has been submitted or skipped.
async fn release_if_done(client: &impl GenericClient, token: Uuid) -> Result<()> {
    client
        .execute(
            "UPDATE extraction_leases l SET released_at = now()
             WHERE l.token = $1 AND NOT EXISTS (
                 SELECT FROM threads t WHERE t.id = ANY (l.thread_ids)
                   AND (t.extraction_state = 'pending' OR t.extracted_at < l.leased_at OR t.extracted_at IS NULL))",
            &[&token],
        )
        .await?;
    Ok(())
}

/// Folds one author into another: messages, positions, names and emails move;
/// the merge is logged and can be undone.
pub async fn merge_authors(indexer: &Indexer, args: MergeAuthorsArgs) -> Result<Value> {
    if args.from_id == args.into_id {
        return Err(anyhow!("from_id and into_id are the same author"));
    }
    let mut client = indexer.tools.get().await?;
    let tx = client.transaction().await?;
    let found: i64 = tx
        .query_one(
            "SELECT count(*) FROM authors WHERE id IN ($1, $2) AND merged_into IS NULL",
            &[&args.from_id, &args.into_id],
        )
        .await?
        .get(0);
    if found != 2 {
        return Err(anyhow!("both authors must exist and not be merged already"));
    }
    let moved_messages: Vec<i64> = tx
        .query(
            "UPDATE messages SET author_id = $2 WHERE author_id = $1 RETURNING id",
            &[&args.from_id, &args.into_id],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let moved_positions: Vec<i64> = tx
        .query(
            "UPDATE positions SET author_id = $2 WHERE author_id = $1 RETURNING id",
            &[&args.from_id, &args.into_id],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    // Emails move too; the log keeps their hashes, never the addresses.
    let moved_emails: Vec<String> = tx
        .query(
            "UPDATE author_emails SET author_id = $2 WHERE author_id = $1
             RETURNING encode(sha256(convert_to(email, 'UTF8')), 'hex')",
            &[&args.from_id, &args.into_id],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let names: Value = tx
        .query_one(
            "SELECT coalesce(jsonb_agg(jsonb_build_object('name', name, 'count', count)), '[]') FROM author_names
             WHERE author_id = $1",
            &[&args.from_id],
        )
        .await?
        .get(0);
    tx.execute(
        "INSERT INTO author_names (author_id, name, count) SELECT $2, name, count FROM author_names WHERE author_id = $1
         ON CONFLICT (author_id, name) DO UPDATE SET count = author_names.count + EXCLUDED.count",
        &[&args.from_id, &args.into_id],
    )
    .await?;
    tx.execute(
        "DELETE FROM author_names WHERE author_id = $1",
        &[&args.from_id],
    )
    .await?;
    tx.execute(
        "UPDATE authors SET merged_into = $2, message_count = 0, updated_at = now() WHERE id = $1",
        &[&args.from_id, &args.into_id],
    )
    .await?;
    refresh_author(&*tx, args.into_id).await?;
    let moved = json!({ "messages": moved_messages, "positions": moved_positions, "emails": moved_emails, "names": names });
    let merge_id: i64 = tx
        .query_one(
            "INSERT INTO merge_log (kind, from_id, into_id, moved, reason, actor) VALUES ('author', $1, $2, $3, $4, 'crawler')
             RETURNING id",
            &[&args.from_id, &args.into_id, &moved, &args.reason],
        )
        .await?
        .get(0);
    tx.commit().await?;
    crate::jobs::wake(&**client, crate::jobs::kind::GRAPH_SYNC).await?;
    Ok(
        json!({ "merge_id": merge_id, "messages_moved": moved_messages.len(), "positions_moved": moved_positions.len() }),
    )
}

async fn refresh_author(client: &impl GenericClient, author_id: i64) -> Result<()> {
    client
        .execute(
            "UPDATE authors a SET message_count = s.n, first_seen = s.first, last_seen = s.last,
                 display_name = coalesce((SELECT name FROM author_names WHERE author_id = a.id
                                          ORDER BY count DESC, name LIMIT 1), a.display_name),
                 updated_at = now()
             FROM (SELECT count(*)::int AS n, min(sent_at) AS first, max(sent_at) AS last
                   FROM messages WHERE author_id = $1) s
             WHERE a.id = $1",
            &[&author_id],
        )
        .await?;
    Ok(())
}

/// Folds one feature or decision into another; logged and reversible.
pub async fn merge_extracts(indexer: &Indexer, args: MergeExtractsArgs) -> Result<Value> {
    if args.from_id == args.into_id {
        return Err(anyhow!("from_id and into_id are the same"));
    }
    let mut client = indexer.tools.get().await?;
    let tx = client.transaction().await?;
    let table = match args.kind {
        DraftKind::Feature => "features",
        DraftKind::Decision => "decisions",
    };
    let found: i64 = tx
        .query_one(
            &format!("SELECT count(*) FROM {table} WHERE id IN ($1, $2) AND merged_into IS NULL"),
            &[&args.from_id, &args.into_id],
        )
        .await?
        .get(0);
    if found != 2 {
        return Err(anyhow!("both must exist and not be merged already"));
    }
    let ids = |rows: Vec<tokio_postgres::Row>| -> Vec<i64> {
        rows.iter().map(|row| row.get(0)).collect()
    };
    let citations = ids(tx
        .query(
            "UPDATE citations c SET extract_id = $3 WHERE extract_kind = $1 AND extract_id = $2
               AND NOT EXISTS (SELECT FROM citations d WHERE d.extract_kind = $1 AND d.extract_id = $3
                               AND d.message_id = c.message_id AND d.quote = c.quote)
             RETURNING id",
            &[&args.kind.as_str(), &args.from_id, &args.into_id],
        )
        .await?);
    let moved = match args.kind {
        DraftKind::Feature => {
            let threads = ids(tx
                .query(
                    "INSERT INTO feature_threads (feature_id, thread_id) SELECT $2, thread_id FROM feature_threads
                     WHERE feature_id = $1 ON CONFLICT DO NOTHING RETURNING thread_id",
                    &[&args.from_id, &args.into_id],
                )
                .await?);
            let files: Vec<String> = tx
                .query(
                    "INSERT INTO feature_files (feature_id, path, thread_id) SELECT $2, path, thread_id FROM feature_files
                     WHERE feature_id = $1 ON CONFLICT DO NOTHING RETURNING path",
                    &[&args.from_id, &args.into_id],
                )
                .await?
                .iter()
                .map(|row| row.get(0))
                .collect();
            let decisions = ids(tx
                .query(
                    "UPDATE decisions SET feature_id = $2 WHERE feature_id = $1 RETURNING id",
                    &[&args.from_id, &args.into_id],
                )
                .await?);
            let positions = ids(tx
                .query(
                    "UPDATE positions SET feature_id = $2 WHERE feature_id = $1 RETURNING id",
                    &[&args.from_id, &args.into_id],
                )
                .await?);
            json!({ "citations": citations, "threads_added": threads, "files_added": files,
                    "decisions": decisions, "positions": positions })
        }
        DraftKind::Decision => {
            let positions = ids(tx
                .query(
                    "UPDATE positions SET decision_id = $2 WHERE decision_id = $1 RETURNING id",
                    &[&args.from_id, &args.into_id],
                )
                .await?);
            json!({ "citations": citations, "positions": positions })
        }
    };
    tx.execute(
        &format!("UPDATE {table} SET merged_into = $2, updated_at = now() WHERE id = $1"),
        &[&args.from_id, &args.into_id],
    )
    .await?;
    let merge_id: i64 = tx
        .query_one(
            "INSERT INTO merge_log (kind, from_id, into_id, moved, reason, actor) VALUES ($1, $2, $3, $4, $5, 'crawler')
             RETURNING id",
            &[&args.kind.as_str(), &args.from_id, &args.into_id, &moved, &args.reason],
        )
        .await?
        .get(0);
    tx.commit().await?;
    crate::jobs::wake(&**client, crate::jobs::kind::GRAPH_SYNC).await?;
    Ok(json!({ "merge_id": merge_id, "moved": moved }))
}

/// Refuses to undo author merge `merge_id` while its `into` author no longer
/// holds what the merge moved: a later merge folded `into` into another
/// author, or the messages moved on. Undoing it then would take the names
/// and counts from `into` while the messages, addresses and names sit with
/// another author. Locks the merge's messages first, which merge_authors
/// moves first: a merge of `into` running alongside finishes before the
/// check, or waits until the undo is done.
async fn undo_author_allowed(
    client: &impl GenericClient,
    merge_id: i64,
    into_id: i64,
    messages: &[i64],
) -> Result<()> {
    // Locked whole: with the author test in the query, PostgreSQL would lock
    // only the rows that pass it as of the snapshot, and so not wait for a
    // merge that is moving them.
    let moved_on: Vec<i64> = client
        .query(
            "SELECT id, author_id FROM messages WHERE id = ANY ($1) FOR NO KEY UPDATE",
            &[&messages],
        )
        .await?
        .iter()
        .filter(|row| row.get::<_, Option<i64>>(1) != Some(into_id))
        .map(|row| row.get(0))
        .collect();
    let merged_into: Option<i64> = client
        .query_opt("SELECT merged_into FROM authors WHERE id = $1", &[&into_id])
        .await?
        .and_then(|row| row.get(0));
    if let Some(next) = merged_into {
        // The merges from into_id on, the last one first.
        let later: Vec<String> = client
            .query(
                "WITH RECURSIVE chain (id, merged_into, depth) AS (
                     SELECT id, merged_into, 0 FROM authors WHERE id = $1
                     UNION ALL
                     SELECT a.id, a.merged_into, c.depth + 1
                     FROM chain c JOIN authors a ON a.id = c.merged_into
                     WHERE c.depth < 32
                 )
                 SELECT c.id, c.merged_into,
                        (SELECT max(l.id) FROM merge_log l
                         WHERE l.kind = 'author' AND l.from_id = c.id AND l.into_id = c.merged_into
                           AND l.reverted_at IS NULL)
                 FROM chain c WHERE c.merged_into IS NOT NULL ORDER BY c.depth DESC",
                &[&into_id],
            )
            .await?
            .iter()
            .map(|row| match row.get::<_, Option<i64>>(2) {
                Some(merge) => format!("merge {merge}"),
                None => format!(
                    "author {}'s merge into author {} (not in the log)",
                    row.get::<_, i64>(0),
                    row.get::<_, i64>(1)
                ),
            })
            .collect();
        let order = match later.len() {
            1 => later.concat(),
            _ => format!("{},", later.join(", then ")),
        };
        return Err(anyhow!(
            "author {into_id} has since been merged into author {next}, so merge {merge_id} can't be \
             undone yet: undo {order} first"
        ));
    }
    if !moved_on.is_empty() {
        let later: Option<i64> = client
            .query_one(
                "SELECT max(l.id) FROM merge_log l
                 WHERE l.kind = 'author' AND l.reverted_at IS NULL AND l.id > $1
                   AND EXISTS (SELECT FROM unnest($2::int8[]) AS m (id)
                               WHERE l.moved -> 'messages' @> to_jsonb(m.id))",
                &[&merge_id, &moved_on],
            )
            .await?
            .get(0);
        let n = moved_on.len();
        return Err(match later {
            Some(later) => anyhow!(
                "{n} of the messages that merge {merge_id} moved to author {into_id} belong to other \
                 authors now, so it can't be undone yet: undo merge {later} first"
            ),
            None => anyhow!(
                "{n} of the messages that merge {merge_id} moved to author {into_id} belong to other \
                 authors now, and no later merge in the log moved them, so it can't be undone"
            ),
        });
    }
    Ok(())
}

/// Undoes a merge from the log.
pub async fn undo_merge(indexer: &Indexer, args: UndoMergeArgs) -> Result<Value> {
    let mut client = indexer.tools.get().await?;
    let tx = client.transaction().await?;
    let row = tx
        .query_opt(
            "SELECT kind, from_id, into_id, moved FROM merge_log WHERE id = $1 AND reverted_at IS NULL FOR UPDATE",
            &[&args.merge_id],
        )
        .await?
        .context("there's no such merge, or it was undone")?;
    let kind: String = row.get(0);
    let from_id: i64 = row.get(1);
    let into_id: i64 = row.get(2);
    let moved: Value = row.get(3);
    let list = |name: &str| -> Vec<i64> {
        moved[name]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default()
    };
    match kind.as_str() {
        "author" => {
            undo_author_allowed(&*tx, args.merge_id, into_id, &list("messages")).await?;
            tx.execute(
                "UPDATE messages SET author_id = $1 WHERE id = ANY ($2)",
                &[&from_id, &list("messages")],
            )
            .await?;
            tx.execute(
                "UPDATE positions SET author_id = $1 WHERE id = ANY ($2)",
                &[&from_id, &list("positions")],
            )
            .await?;
            let hashes: Vec<String> = moved["emails"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            tx.execute(
                "UPDATE author_emails SET author_id = $1
                 WHERE author_id = $2 AND encode(sha256(convert_to(email, 'UTF8')), 'hex') = ANY ($3)",
                &[&from_id, &into_id, &hashes],
            )
            .await?;
            if moved["names"].is_array() {
                // The names count each message under its sender's name as
                // stored. That is the logged names' unless restore_addresses
                // gave the moved messages' names their addresses back since.
                let names: Vec<(String, i32)> = tx
                    .query(
                        "SELECT from_name, count(*)::int FROM messages WHERE id = ANY ($1) GROUP BY from_name",
                        &[&list("messages")],
                    )
                    .await?
                    .iter()
                    .map(|row| (row.get(0), row.get(1)))
                    .collect();
                for (name, count) in &names {
                    tx.execute(
                        "INSERT INTO author_names (author_id, name, count) VALUES ($1, $2, $3)
                         ON CONFLICT (author_id, name) DO UPDATE SET count = EXCLUDED.count",
                        &[&from_id, name, count],
                    )
                    .await?;
                    tx.execute(
                        "UPDATE author_names SET count = count - $3 WHERE author_id = $1 AND name = $2",
                        &[&into_id, name, count],
                    )
                    .await?;
                }
                tx.execute(
                    "DELETE FROM author_names WHERE author_id = $1 AND count <= 0",
                    &[&into_id],
                )
                .await?;
            }
            tx.execute(
                "UPDATE authors SET merged_into = NULL WHERE id = $1",
                &[&from_id],
            )
            .await?;
            refresh_author(&*tx, from_id).await?;
            refresh_author(&*tx, into_id).await?;
        }
        "feature" | "decision" => {
            tx.execute(
                "UPDATE citations SET extract_id = $1 WHERE id = ANY ($2)",
                &[&from_id, &list("citations")],
            )
            .await?;
            if kind == "feature" {
                tx.execute(
                    "DELETE FROM feature_threads WHERE feature_id = $1 AND thread_id = ANY ($2)",
                    &[&into_id, &list("threads_added")],
                )
                .await?;
                let files: Vec<String> = moved["files_added"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                tx.execute(
                    "DELETE FROM feature_files WHERE feature_id = $1 AND path = ANY ($2)",
                    &[&into_id, &files],
                )
                .await?;
                tx.execute(
                    "UPDATE decisions SET feature_id = $1 WHERE id = ANY ($2)",
                    &[&from_id, &list("decisions")],
                )
                .await?;
                tx.execute(
                    "UPDATE positions SET feature_id = $1 WHERE id = ANY ($2)",
                    &[&from_id, &list("positions")],
                )
                .await?;
                tx.execute(
                    "UPDATE features SET merged_into = NULL, updated_at = now() WHERE id = $1",
                    &[&from_id],
                )
                .await?;
            } else {
                tx.execute(
                    "UPDATE positions SET decision_id = $1 WHERE id = ANY ($2)",
                    &[&from_id, &list("positions")],
                )
                .await?;
                tx.execute(
                    "UPDATE decisions SET merged_into = NULL, updated_at = now() WHERE id = $1",
                    &[&from_id],
                )
                .await?;
            }
            // Extracts that came while it was merged weren't compared with
            // it: compare it again.
            tx.execute(
                "DELETE FROM duplicate_checks WHERE kind = $1 AND extract_id = $2",
                &[&kind, &from_id],
            )
            .await?;
        }
        other => return Err(anyhow!("cannot undo a {other} merge")),
    }
    tx.execute(
        "UPDATE merge_log SET reverted_at = now() WHERE id = $1",
        &[&args.merge_id],
    )
    .await?;
    tx.commit().await?;
    crate::jobs::wake(&**client, crate::jobs::kind::GRAPH_SYNC).await?;
    crate::jobs::wake(&**client, crate::jobs::kind::DUPLICATES).await?;
    Ok(json!({ "merge_id": args.merge_id, "undone": true }))
}

/// Pairs of features or decisions that look like the same thing, except
/// pairs already judged different: the duplicates job's candidates whose two
/// extracts are both live.
pub async fn find_duplicates(indexer: &Indexer, args: FindDuplicatesArgs) -> Result<Value> {
    let min = args
        .min_similarity
        .unwrap_or(SEMANTIC_DUPLICATE)
        .clamp(crate::duplicates::SEMANTIC_FLOOR, 1.0);
    let limit = i64::from(args.limit.unwrap_or(20).clamp(1, 100));
    let (table, label, summary) = match args.kind {
        DraftKind::Feature => ("features", "name", "description"),
        DraftKind::Decision => ("decisions", "title", "outcome"),
    };
    let kind = args.kind.as_str();
    let _permit = tokio::time::timeout(FIND_DUPLICATES_WAIT, indexer.find_duplicates.acquire())
        .await
        .map_err(|_| {
            anyhow!(
                "find_duplicates is already running {} times; try again in a minute",
                crate::FIND_DUPLICATES_AT_ONCE
            )
        })??;
    let client = indexer.tools.get().await?;
    let rows = client
        .query(
            &format!(
                "SELECT a.id, a.{label}, a.{summary}, b.id, b.{label}, b.{summary}, c.semantic, c.name_similarity
                 FROM duplicate_candidates c
                 JOIN {table} a ON a.id = c.low_id AND a.merged_into IS NULL
                 JOIN {table} b ON b.id = c.high_id AND b.merged_into IS NULL
                 WHERE c.kind = $4 AND (c.semantic >= $1 OR c.name_similarity >= $2)
                   AND NOT EXISTS (SELECT FROM distinct_pairs p
                                   WHERE p.kind = c.kind AND p.low_id = c.low_id AND p.high_id = c.high_id)
                 ORDER BY greatest(coalesce(c.semantic, 0), c.name_similarity) DESC, c.low_id, c.high_id
                 LIMIT $3"
            ),
            &[&min, &NAME_DUPLICATE, &limit, &kind],
        )
        .await?;
    let pairs: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "a": { "id": row.get::<_, i64>(0), "name": row.get::<_, String>(1), "summary": truncate_chars(&row.get::<_, String>(2), 300) },
                "b": { "id": row.get::<_, i64>(3), "name": row.get::<_, String>(4), "summary": truncate_chars(&row.get::<_, String>(5), 300) },
                "semantic": row.get::<_, Option<f64>>(6).map(|s| (s * 1000.0).round() / 1000.0),
                "name_similarity": (row.get::<_, f64>(7) * 1000.0).round() / 1000.0,
            })
        })
        .collect();
    let judged: i64 = client
        .query_one(
            "SELECT count(*) FROM distinct_pairs WHERE kind = $1",
            &[&kind],
        )
        .await?
        .get(0);
    let pending = crate::duplicates::pending(&**client, args.kind).await?;
    let mut next = "Merge true duplicates with merge_extracts (from the less complete into the more complete), \
                    and record every pair that differs with mark_distinct. Pairs already judged different \
                    (judged_distinct) aren't listed."
        .to_owned();
    if pending > 0 {
        let (noun, verb) = if pending == 1 {
            (kind.to_owned(), "isn't")
        } else {
            (format!("{kind}s"), "aren't")
        };
        next.push_str(&format!(
            " {pending} {noun} {verb} compared yet: the indexer compares them in the background, \
             so later calls may list more pairs."
        ));
    }
    Ok(
        json!({ "kind": kind, "min_similarity": min, "pairs": pairs, "judged_distinct": judged,
               "pending": pending, "next": next }),
    )
}

/// Records a deduplicator's verdict that two features, or two decisions, are
/// different things, so find_duplicates doesn't offer the pair again.
pub async fn mark_distinct(indexer: &Indexer, args: MarkDistinctArgs) -> Result<Value> {
    if args.a_id == args.b_id {
        return Err(anyhow!("a_id and b_id are the same"));
    }
    let reason = args.reason.trim();
    if reason.chars().count() < 10 {
        return Err(anyhow!("reason must say what differs"));
    }
    let table = match args.kind {
        DraftKind::Feature => "features",
        DraftKind::Decision => "decisions",
    };
    let kind = args.kind.as_str();
    let client = indexer.tools.get().await?;
    let found: i64 = client
        .query_one(
            &format!("SELECT count(*) FROM {table} WHERE id IN ($1, $2) AND merged_into IS NULL"),
            &[&args.a_id, &args.b_id],
        )
        .await?
        .get(0);
    if found != 2 {
        return Err(anyhow!("both must exist and not be merged"));
    }
    let verdict = Distinct {
        kind,
        reason,
        judged_by: "dedup",
        session: args.session.as_deref(),
        model: args.model.as_deref(),
    };
    let pair = [args.a_id.min(args.b_id), args.a_id.max(args.b_id)];
    if verdict.record(&**client, args.a_id, args.b_id).await? {
        return Ok(json!({ "recorded": true, "kind": kind, "pair": pair }));
    }
    let earlier = client
        .query_one(
            "SELECT reason, judged_by, created_at FROM distinct_pairs WHERE kind = $1 AND low_id = $2 AND high_id = $3",
            &[&kind, &pair[0], &pair[1]],
        )
        .await?;
    Ok(json!({
        "recorded": false,
        "kind": kind,
        "pair": pair,
        "already_judged": {
            "reason": earlier.get::<_, String>(0),
            "judged_by": earlier.get::<_, String>(1),
            "at": earlier.get::<_, DateTime<Utc>>(2),
        },
    }))
}
