// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! search_threads and get_thread.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use chrono::{DateTime, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::embed::Embedder;
use pghacker_core::text::truncate_chars;

use crate::common::{AuthorRef, Mode, Searcher, resolve_author, snippet};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchThreadsArgs {
    /// What to look for: words, a phrase or a question.
    pub query: String,
    /// fulltext, semantic or hybrid (default).
    #[serde(default)]
    pub mode: Mode,
    /// Only messages by this author: an author id or a name.
    #[serde(default)]
    pub author: Option<AuthorRef>,
    /// Only messages sent on or after this date (YYYY-MM-DD).
    #[serde(default)]
    pub since: Option<NaiveDate>,
    /// Only messages sent before this date (YYYY-MM-DD).
    #[serde(default)]
    pub until: Option<NaiveDate>,
    /// Threads to return, 1 to 30. Default 10.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetThreadArgs {
    /// The thread's id ...
    #[serde(default)]
    pub thread_id: Option<i64>,
    /// ... or any of its messages' Message-ID.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Messages to skip. Default 0.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Messages to return, 1 to 100. Default 30.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Include each message's full text, quoted replies too, instead of its own words only.
    #[serde(default)]
    pub full_text: bool,
}

fn day(date: Option<NaiveDate>) -> Option<DateTime<Utc>> {
    date.and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
}

pub async fn search_threads(
    client: &impl GenericClient,
    embedder: Option<&Embedder>,
    args: SearchThreadsArgs,
) -> Result<Value> {
    let limit = args.limit.unwrap_or(10).clamp(1, 30) as usize;
    let searcher = Searcher::new(args.mode, &args.query, embedder).await?;
    let authors = match &args.author {
        Some(author) => Some(vec![resolve_author(client, author).await?]),
        None => None,
    };
    let ranked = searcher
        .rank_chunks(
            client,
            authors.as_ref(),
            day(args.since),
            day(args.until),
            300,
        )
        .await?;
    // Threads in order of their best chunk, with their best chunks.
    let mut order: Vec<i64> = Vec::new();
    let mut chunks_of: HashMap<i64, Vec<i64>> = HashMap::new();
    for (chunk, thread) in &ranked {
        let entry = chunks_of.entry(*thread).or_default();
        if entry.is_empty() {
            order.push(*thread);
        }
        if entry.len() < 3 {
            entry.push(*chunk);
        }
    }
    order.truncate(limit);
    let thread_ids = order.clone();
    let chunk_ids: Vec<i64> = order.iter().flat_map(|t| chunks_of[t].clone()).collect();
    let threads: HashMap<i64, tokio_postgres::Row> = client
        .query(
            "SELECT id, subject, root_message_id, started_at, last_at, message_count, participant_count
             FROM threads WHERE id = ANY ($1)",
            &[&thread_ids],
        )
        .await?
        .into_iter()
        .map(|row| (row.get(0), row))
        .collect();
    let passages: HashMap<i64, Value> = client
        .query(
            "SELECT c.id, m.message_id, coalesce(a.display_name, m.from_name), m.sent_at, c.content
             FROM message_chunks c JOIN messages m ON m.id = c.message_id LEFT JOIN authors a ON a.id = m.author_id
             WHERE c.id = ANY ($1)",
            &[&chunk_ids],
        )
        .await?
        .iter()
        .map(|row| {
            (
                row.get(0),
                json!({
                    "message_id": row.get::<_, String>(1),
                    "author": row.get::<_, String>(2),
                    "sent_at": row.get::<_, Option<DateTime<Utc>>>(3),
                    "passage": snippet(&row.get::<_, String>(4), searcher.query(), 320),
                }),
            )
        })
        .collect();
    let participants = top_participants(client, &thread_ids, 5).await?;
    let features = linked_features(client, &thread_ids).await?;
    let results: Vec<Value> = order
        .iter()
        .filter_map(|id| {
            let row = threads.get(id)?;
            Some(json!({
                "thread_id": id,
                "subject": row.get::<_, String>(1),
                "root_message_id": row.get::<_, String>(2),
                "started_at": row.get::<_, Option<DateTime<Utc>>>(3),
                "last_at": row.get::<_, Option<DateTime<Utc>>>(4),
                "messages": row.get::<_, i32>(5),
                "participants": participants.get(id).cloned().unwrap_or_default(),
                "features": features.get(id).cloned().unwrap_or_default(),
                "matches": chunks_of[id].iter().filter_map(|c| passages.get(c).cloned()).collect::<Vec<_>>(),
            }))
        })
        .collect();
    let mut out = json!({ "mode": searcher.mode.as_str(), "threads": results });
    if !searcher.notes.is_empty() {
        out["notes"] = json!(searcher.notes);
    }
    Ok(out)
}

/// Each thread's most active participants' names.
async fn top_participants(
    client: &impl GenericClient,
    threads: &[i64],
    n: i64,
) -> Result<HashMap<i64, Vec<String>>> {
    let rows = client
        .query(
            "SELECT thread_id, array_agg(name ORDER BY messages DESC, name) FROM (
                 SELECT m.thread_id, coalesce(a.display_name, m.from_name) AS name, count(*) AS messages,
                        row_number() OVER (PARTITION BY m.thread_id ORDER BY count(*) DESC) AS rank
                 FROM messages m LEFT JOIN authors a ON a.id = m.author_id
                 WHERE m.thread_id = ANY ($1) GROUP BY m.thread_id, 2
             ) x WHERE rank <= $2 GROUP BY thread_id",
            &[&threads, &n],
        )
        .await?;
    Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
}

async fn linked_features(
    client: &impl GenericClient,
    threads: &[i64],
) -> Result<HashMap<i64, Vec<Value>>> {
    let rows = client
        .query(
            "SELECT ft.thread_id, f.slug, f.name, f.status, f.release FROM feature_threads ft
             JOIN features f ON f.id = ft.feature_id WHERE ft.thread_id = ANY ($1) AND f.merged_into IS NULL
             ORDER BY ft.thread_id, f.id",
            &[&threads],
        )
        .await?;
    let mut out: HashMap<i64, Vec<Value>> = HashMap::new();
    for row in rows {
        out.entry(row.get(0)).or_default().push(json!({
            "slug": row.get::<_, String>(1), "name": row.get::<_, String>(2),
            "status": row.get::<_, String>(3), "release": row.get::<_, Option<String>>(4),
        }));
    }
    Ok(out)
}

pub async fn get_thread(client: &impl GenericClient, args: GetThreadArgs) -> Result<Value> {
    let thread_id: i64 = match (args.thread_id, &args.message_id) {
        (Some(id), _) => id,
        (None, Some(message_id)) => {
            let id = message_id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            client
                .query_opt(
                    "SELECT thread_id FROM messages WHERE message_id = $1",
                    &[&id],
                )
                .await?
                .map(|row| row.get(0))
                .ok_or_else(|| anyhow!("there's no message {id}"))?
        }
        (None, None) => return Err(anyhow!("pass thread_id or message_id")),
    };
    let thread = client
        .query_opt(
            "SELECT id, subject, root_message_id, started_at, last_at, message_count, participant_count, list
             FROM threads WHERE id = $1",
            &[&thread_id],
        )
        .await?
        .ok_or_else(|| anyhow!("there's no thread {thread_id}"))?;
    let offset = i64::from(args.offset.unwrap_or(0));
    let limit = i64::from(args.limit.unwrap_or(30).clamp(1, 100));
    let rows = client
        .query(
            "SELECT m.id, m.message_id, m.author_id, coalesce(a.display_name, m.from_name), m.sent_at, m.subject,
                    m.parent_message_id, CASE WHEN $4 THEN m.body ELSE m.body_clean END, m.has_patch
             FROM messages m LEFT JOIN authors a ON a.id = m.author_id
             WHERE m.thread_id = $1 ORDER BY m.sent_at NULLS LAST, m.id OFFSET $2 LIMIT $3",
            &[&thread_id, &offset, &limit, &args.full_text],
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
            "filename": row.get::<_, String>(1), "files": row.get::<_, Vec<String>>(2),
            "added": row.get::<_, i32>(3), "removed": row.get::<_, i32>(4),
        }));
    }
    let mut mentions: HashMap<i64, Vec<Value>> = HashMap::new();
    for row in client
        .query(
            "SELECT message_id, kind, value, resolved FROM anchors WHERE message_id = ANY ($1) ORDER BY id",
            &[&ids],
        )
        .await?
    {
        mentions.entry(row.get(0)).or_default().push(json!({
            "kind": row.get::<_, String>(1), "value": row.get::<_, String>(2),
            "file_id": row.get::<_, Option<String>>(3),
        }));
    }
    let messages: Vec<Value> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get(0);
            let text: String = row.get(7);
            json!({
                "message_id": row.get::<_, String>(1),
                "author_id": row.get::<_, Option<i64>>(2),
                "author": row.get::<_, String>(3),
                "sent_at": row.get::<_, Option<DateTime<Utc>>>(4),
                "subject": row.get::<_, String>(5),
                "in_reply_to": row.get::<_, Option<String>>(6),
                "text": truncate_chars(&text, 6000),
                "text_truncated": text.chars().count() > 6000,
                "patches": patches.remove(&id).unwrap_or_default(),
                "mentions": mentions.remove(&id).unwrap_or_default(),
            })
        })
        .collect();
    let total: i32 = thread.get(5);
    let next = (offset + rows.len() as i64) < i64::from(total);
    Ok(json!({
        "thread_id": thread_id,
        "list": thread.get::<_, String>(7),
        "subject": thread.get::<_, String>(1),
        "root_message_id": thread.get::<_, String>(2),
        "started_at": thread.get::<_, Option<DateTime<Utc>>>(3),
        "last_at": thread.get::<_, Option<DateTime<Utc>>>(4),
        "message_count": total,
        "participant_count": thread.get::<_, i32>(6),
        "extracts": thread_extracts(client, thread_id).await?,
        "offset": offset,
        "messages": messages,
        "next_offset": next.then_some(offset + rows.len() as i64),
        "archive_url": format!("https://www.postgresql.org/message-id/flat/{}", thread.get::<_, String>(2)),
    }))
}

async fn thread_extracts(client: &impl GenericClient, thread_id: i64) -> Result<Value> {
    let features: Vec<Value> = client
        .query(
            "SELECT f.id, f.slug, f.name, f.status, f.release FROM feature_threads ft JOIN features f ON f.id = ft.feature_id
             WHERE ft.thread_id = $1 AND f.merged_into IS NULL ORDER BY f.id",
            &[&thread_id],
        )
        .await?
        .iter()
        .map(|row| json!({ "id": row.get::<_, i64>(0), "slug": row.get::<_, String>(1), "name": row.get::<_, String>(2),
                           "status": row.get::<_, String>(3), "release": row.get::<_, Option<String>>(4) }))
        .collect();
    let decisions: Vec<Value> = client
        .query(
            "SELECT id, title, outcome, decided_at FROM decisions WHERE thread_id = $1 AND merged_into IS NULL ORDER BY id",
            &[&thread_id],
        )
        .await?
        .iter()
        .map(|row| json!({ "id": row.get::<_, i64>(0), "title": row.get::<_, String>(1), "outcome": row.get::<_, String>(2),
                           "decided_at": row.get::<_, Option<DateTime<Utc>>>(3) }))
        .collect();
    let positions: Vec<Value> = client
        .query(
            "SELECT p.id, a.display_name, p.author_id, p.stance, p.summary, f.slug FROM positions p
             JOIN authors a ON a.id = p.author_id LEFT JOIN features f ON f.id = p.feature_id
             WHERE p.thread_id = $1 ORDER BY p.id",
            &[&thread_id],
        )
        .await?
        .iter()
        .map(|row| json!({ "id": row.get::<_, i64>(0), "author": row.get::<_, String>(1), "author_id": row.get::<_, i64>(2),
                           "stance": row.get::<_, String>(3), "summary": row.get::<_, String>(4),
                           "feature": row.get::<_, Option<String>>(5) }))
        .collect();
    Ok(json!({ "features": features, "decisions": decisions, "positions": positions }))
}
