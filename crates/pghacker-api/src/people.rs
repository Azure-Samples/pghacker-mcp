//! search_authors, get_author and author_perspective.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::embed::Embedder;

use crate::common::{AuthorRef, Mode, Searcher, citations, resolve_author, snippet};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchAuthorsArgs {
    /// A name or part of one.
    pub query: String,
    /// Results, 1 to 50. Default 10.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetAuthorArgs {
    /// An author id or a name.
    pub author: AuthorRef,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuthorPerspectiveArgs {
    /// An author id or a name.
    pub author: AuthorRef,
    /// The topic: a feature, decision or question, such as "SQL/JSON revert".
    pub topic: String,
    /// fulltext, semantic or hybrid (default).
    #[serde(default)]
    pub mode: Mode,
    /// Positions to return, 1 to 20. Default 8.
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn search_authors(client: &impl GenericClient, args: SearchAuthorsArgs) -> Result<Value> {
    let limit = i64::from(args.limit.unwrap_or(10).clamp(1, 50));
    let query = args.query.trim();
    let rows = client
        .query(
            "SELECT a.id, a.display_name, a.message_count, a.first_seen, a.last_seen,
                    array_agg(DISTINCT n.name) FILTER (WHERE n.name <> a.display_name),
                    max(similarity(n.name, $1))::float8 AS score,
                    (SELECT count(*) FROM positions p WHERE p.author_id = a.id)
             FROM author_names n JOIN authors a ON a.id = n.author_id
             WHERE a.merged_into IS NULL AND (n.name % $1 OR n.name ILIKE '%' || $1 || '%')
             GROUP BY a.id ORDER BY score DESC, a.message_count DESC LIMIT $2",
            &[&query, &limit],
        )
        .await?;
    let authors: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "author_id": row.get::<_, i64>(0), "name": row.get::<_, String>(1),
                "messages": row.get::<_, i32>(2), "first_seen": row.get::<_, Option<DateTime<Utc>>>(3),
                "last_seen": row.get::<_, Option<DateTime<Utc>>>(4),
                "other_names": row.get::<_, Option<Vec<String>>>(5).unwrap_or_default(),
                "positions": row.get::<_, i64>(7),
            })
        })
        .collect();
    Ok(json!({ "authors": authors }))
}

pub async fn get_author(client: &impl GenericClient, args: GetAuthorArgs) -> Result<Value> {
    let id = resolve_author(client, &args.author).await?;
    let author = client
        .query_one(
            "SELECT id, display_name, message_count, first_seen, last_seen FROM authors WHERE id = $1",
            &[&id],
        )
        .await?;
    let names: Vec<Value> = client
        .query(
            "SELECT name, count FROM author_names WHERE author_id = $1 ORDER BY count DESC, name",
            &[&id],
        )
        .await?
        .iter()
        .map(|row| json!({ "name": row.get::<_, String>(0), "messages": row.get::<_, i32>(1) }))
        .collect();
    let threads: Vec<Value> = client
        .query(
            "SELECT t.id, t.subject, count(*) AS mine, t.message_count, min(m.sent_at), max(m.sent_at)
             FROM messages m JOIN threads t ON t.id = m.thread_id WHERE m.author_id = $1
             GROUP BY t.id ORDER BY count(*) DESC, max(m.sent_at) DESC LIMIT 15",
            &[&id],
        )
        .await?
        .iter()
        .map(|row| {
            json!({ "thread_id": row.get::<_, i64>(0), "subject": row.get::<_, String>(1),
                    "their_messages": row.get::<_, i64>(2), "messages": row.get::<_, i32>(3),
                    "first": row.get::<_, Option<DateTime<Utc>>>(4), "last": row.get::<_, Option<DateTime<Utc>>>(5) })
        })
        .collect();
    let stances: Value = client
        .query_one(
            "SELECT coalesce(jsonb_object_agg(stance, n), '{}') FROM
             (SELECT stance, count(*) AS n FROM positions WHERE author_id = $1 GROUP BY stance) x",
            &[&id],
        )
        .await?
        .get(0);
    let features: Vec<Value> = client
        .query(
            "SELECT f.slug, f.name, f.status, array_agg(DISTINCT p.stance) FROM positions p JOIN features f ON f.id = p.feature_id
             WHERE p.author_id = $1 AND f.merged_into IS NULL GROUP BY f.id ORDER BY count(*) DESC LIMIT 20",
            &[&id],
        )
        .await?
        .iter()
        .map(|row| json!({ "slug": row.get::<_, String>(0), "name": row.get::<_, String>(1),
                           "status": row.get::<_, String>(2), "stances": row.get::<_, Vec<String>>(3) }))
        .collect();
    Ok(json!({
        "author_id": id,
        "name": author.get::<_, String>(1),
        "messages": author.get::<_, i32>(2),
        "first_seen": author.get::<_, Option<DateTime<Utc>>>(3),
        "last_seen": author.get::<_, Option<DateTime<Utc>>>(4),
        "names": names,
        "threads": threads,
        "positions_by_stance": stances,
        "features": features,
    }))
}

pub async fn author_perspective(
    client: &impl GenericClient,
    embedder: Option<&Embedder>,
    args: AuthorPerspectiveArgs,
) -> Result<Value> {
    let limit = args.limit.unwrap_or(8).clamp(1, 20) as usize;
    let id = resolve_author(client, &args.author).await?;
    let name: String = client
        .query_one("SELECT display_name FROM authors WHERE id = $1", &[&id])
        .await?
        .get(0);
    let searcher = Searcher::new(args.mode, &args.topic, embedder).await?;
    let authors = vec![id];
    let ranked = searcher
        .rank_table(
            client,
            "positions",
            "t.author_id = ANY ($3)",
            Some(&authors),
            50,
        )
        .await?;
    let position_ids: Vec<i64> = ranked.into_iter().take(limit).collect();
    let cites = citations(client, "position", &position_ids, 3).await?;
    let mut positions: HashMap<i64, Value> = HashMap::new();
    for row in client
        .query(
            "SELECT p.id, p.stance, p.summary, f.slug, f.name, f.status, d.id, d.title, t.id, t.subject, t.last_at
             FROM positions p JOIN threads t ON t.id = p.thread_id
             LEFT JOIN features f ON f.id = p.feature_id LEFT JOIN decisions d ON d.id = p.decision_id
             WHERE p.id = ANY ($1)",
            &[&position_ids],
        )
        .await?
    {
        let pid: i64 = row.get(0);
        positions.insert(pid, json!({
            "position_id": pid, "stance": row.get::<_, String>(1), "summary": row.get::<_, String>(2),
            "feature": row.get::<_, Option<String>>(3).map(|slug| json!({
                "slug": slug, "name": row.get::<_, Option<String>>(4), "status": row.get::<_, Option<String>>(5) })),
            "decision": row.get::<_, Option<i64>>(6).map(|did| json!({ "id": did, "title": row.get::<_, Option<String>>(7) })),
            "thread": { "thread_id": row.get::<_, i64>(8), "subject": row.get::<_, String>(9),
                        "last_at": row.get::<_, Option<DateTime<Utc>>>(10) },
            "citations": cites.get(&pid).cloned().unwrap_or_default(),
        }));
    }
    let ordered: Vec<Value> = position_ids
        .iter()
        .filter_map(|pid| positions.remove(pid))
        .collect();
    let mut stances: HashMap<String, usize> = HashMap::new();
    for position in &ordered {
        *stances
            .entry(position["stance"].as_str().unwrap_or_default().to_owned())
            .or_default() += 1;
    }
    // The author's own most relevant messages, as further evidence.
    let chunks = searcher
        .rank_chunks(client, Some(&authors), None, None, 40)
        .await?;
    let mut seen_messages = std::collections::HashSet::new();
    let chunk_ids: Vec<i64> = chunks.iter().map(|(chunk, _)| *chunk).collect();
    let rows = client
        .query(
            "SELECT c.id, m.message_id, m.sent_at, t.id, t.subject, c.content
             FROM message_chunks c JOIN messages m ON m.id = c.message_id JOIN threads t ON t.id = m.thread_id
             WHERE c.id = ANY ($1)",
            &[&chunk_ids],
        )
        .await?;
    let by_chunk: HashMap<i64, &tokio_postgres::Row> =
        rows.iter().map(|row| (row.get(0), row)).collect();
    let mut messages = Vec::new();
    for (chunk, _) in &chunks {
        let Some(row) = by_chunk.get(chunk) else {
            continue;
        };
        let message_id: String = row.get(1);
        if !seen_messages.insert(message_id.clone()) {
            continue;
        }
        messages.push(json!({
            "message_id": message_id, "sent_at": row.get::<_, Option<DateTime<Utc>>>(2),
            "thread_id": row.get::<_, i64>(3), "thread": row.get::<_, String>(4),
            "passage": snippet(&row.get::<_, String>(5), searcher.query(), 400),
        }));
        if messages.len() >= 5 {
            break;
        }
    }
    let mut out = json!({
        "author_id": id,
        "author": name,
        "topic": args.topic,
        "mode": searcher.mode.as_str(),
        "positions": ordered,
        "stances": stances,
        "messages": messages,
    });
    if position_ids.is_empty() {
        out["note"] = json!(
            "No extracted positions match this topic yet; the author's own messages below are the evidence."
        );
    }
    if !searcher.notes.is_empty() {
        out["notes"] = json!(searcher.notes);
    }
    Ok(out)
}
