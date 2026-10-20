// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! search_authors, get_author and author_perspective.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::search::{RRF_K, rrf};

use crate::common::{AuthorRef, Mode, Searcher, citations, resolve_author, snippet};

/// A topic narrows an author's record instead of ranking all of it: with an
/// embedding, rows count only within this cosine distance of the topic...
const TOPIC_CEILING: f64 = 0.7;
/// ...and within this margin of the closest row.
const TOPIC_MARGIN: f64 = 0.25;

/// The largest distance from the topic that counts, given the closest row's.
pub fn topic_cutoff(closest: f64) -> f64 {
    (closest + TOPIC_MARGIN).min(TOPIC_CEILING)
}

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
    /// Files or folders from the source root, such as the files a patch
    /// changes (src/backend/optimizer/plan/planner.c, src/include/nodes):
    /// the author's positions on features implemented in them count too,
    /// besides those matching the topic.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only evidence from before this date (YYYY-MM-DD): positions whose
    /// cited messages were all sent before it, and messages sent before it.
    /// For what the author had said by then.
    #[serde(default)]
    pub until: Option<NaiveDate>,
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

/// `searcher` is [`Searcher::new`] of `args`' topic and mode.
pub async fn author_perspective(
    client: &impl GenericClient,
    searcher: &Searcher,
    args: AuthorPerspectiveArgs,
) -> Result<Value> {
    let limit = args.limit.unwrap_or(8).clamp(1, 20) as usize;
    let id = resolve_author(client, &args.author).await?;
    let name: String = client
        .query_one("SELECT display_name FROM authors WHERE id = $1", &[&id])
        .await?
        .get(0);
    let authors = vec![id];
    let until = args
        .until
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|day| day.and_utc());
    let paths = source_paths(&args.paths);
    // The topic's rankings fused into one, then fused with the paths' at
    // equal weight, so that neither crowds the other out.
    let topic_ranked: Vec<i64> = rrf(&topic_positions(client, searcher, id).await?, RRF_K)
        .into_iter()
        .map(|(position, _)| position)
        .collect();
    let path_ranked = if paths.is_empty() {
        Vec::new()
    } else {
        path_positions(client, id, &paths).await?
    };
    let mut rankings = vec![topic_ranked.clone(), path_ranked.clone()];
    if let Some(until) = until {
        let before = positions_before(client, id, until).await?;
        for ranking in &mut rankings {
            ranking.retain(|position| before.contains(position));
        }
    }
    rankings.retain(|ranking| !ranking.is_empty());
    let position_ids: Vec<i64> = rrf(&rankings, RRF_K)
        .into_iter()
        .map(|(position, _)| position)
        .take(limit)
        .collect();
    let on_topic: HashSet<i64> = topic_ranked.into_iter().collect();
    let on_paths: HashSet<i64> = path_ranked.into_iter().collect();
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
        let mut position = json!({
            "position_id": pid, "stance": row.get::<_, String>(1), "summary": row.get::<_, String>(2),
            "feature": row.get::<_, Option<String>>(3).map(|slug| json!({
                "slug": slug, "name": row.get::<_, Option<String>>(4), "status": row.get::<_, Option<String>>(5) })),
            "decision": row.get::<_, Option<i64>>(6).map(|did| json!({ "id": did, "title": row.get::<_, Option<String>>(7) })),
            "thread": { "thread_id": row.get::<_, i64>(8), "subject": row.get::<_, String>(9),
                        "last_at": row.get::<_, Option<DateTime<Utc>>>(10) },
            "citations": cites.get(&pid).cloned().unwrap_or_default(),
        });
        if !paths.is_empty() {
            let matched: Vec<&str> = [("topic", &on_topic), ("paths", &on_paths)]
                .into_iter()
                .filter(|(_, ids)| ids.contains(&pid))
                .map(|(how, _)| how)
                .collect();
            position["matched"] = json!(matched);
        }
        positions.insert(pid, position);
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
    let mut chunks = searcher
        .rank_chunks(client, Some(&authors), None, until, 40)
        .await?;
    if let Some(embedding) = topic_embedding(searcher) {
        let ids: Vec<i64> = chunks.iter().map(|(chunk, _)| *chunk).collect();
        let distances: HashMap<i64, Option<f64>> = client
            .query(
                "SELECT id, (embedding <=> $1)::float8 FROM message_chunks WHERE id = ANY ($2)",
                &[embedding, &ids],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let closest = distances
            .values()
            .flatten()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let cutoff = topic_cutoff(closest);
        // Chunks not embedded yet still count when their text matched.
        chunks.retain(|(chunk, _)| {
            distances
                .get(chunk)
                .copied()
                .flatten()
                .is_none_or(|distance| distance <= cutoff)
        });
    }
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
    if !paths.is_empty() {
        out["paths"] = json!(paths);
    }
    if let Some(until) = args.until {
        out["until"] = json!(until);
    }
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

/// `paths` as `feature_files` has them: from the source root, without a
/// leading `./` or `/` or a trailing `/`.
fn source_paths(paths: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    paths
        .iter()
        .map(|path| {
            let path = path.trim();
            let path = path.strip_prefix("./").unwrap_or(path);
            path.trim_matches('/').to_owned()
        })
        .filter(|path| !path.is_empty() && seen.insert(path.clone()))
        .collect()
}

/// The author's positions on features implemented in `paths` (files, or
/// folders and what's below them), directly or through a decision about the
/// feature: those touching the most of the paths first, then the latest.
async fn path_positions(
    client: &impl GenericClient,
    author: i64,
    paths: &[String],
) -> Result<Vec<i64>> {
    let below: Vec<String> = paths
        .iter()
        .map(|path| {
            let escaped = path
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("{escaped}/%")
        })
        .collect();
    let rows = client
        .query(
            "SELECT p.id
             FROM positions p LEFT JOIN decisions d ON d.id = p.decision_id
             JOIN feature_files ff ON ff.feature_id = coalesce(p.feature_id, d.feature_id)
             JOIN threads t ON t.id = p.thread_id
             WHERE p.author_id = $1 AND (ff.path = ANY ($2) OR ff.path LIKE ANY ($3))
             GROUP BY p.id, t.last_at
             ORDER BY count(DISTINCT ff.path) DESC, t.last_at DESC NULLS LAST, p.id
             LIMIT 200",
            &[&author, &paths, &below],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

/// The author's positions whose cited messages were all sent before `until`.
async fn positions_before(
    client: &impl GenericClient,
    author: i64,
    until: DateTime<Utc>,
) -> Result<HashSet<i64>> {
    let rows = client
        .query(
            "SELECT c.extract_id
             FROM citations c JOIN positions p ON p.id = c.extract_id
             JOIN messages m ON m.id = c.message_id
             WHERE c.extract_kind = 'position' AND p.author_id = $1
             GROUP BY c.extract_id HAVING max(m.sent_at) < $2",
            &[&author, &until],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

/// The topic's embedding when the mode uses semantic search.
fn topic_embedding(searcher: &Searcher) -> Option<&pgvector::HalfVector> {
    searcher
        .embedding()
        .filter(|_| searcher.mode != Mode::Fulltext)
}

/// The rankings of the author's positions on the topic, best first, for RRF
/// to fuse. A position matches through its summary or through the feature or
/// decision it is about, since a summary often leaves its subject implicit.
/// With an embedding, a position counts only within the topic cutoff, so
/// loose text matches drop out.
async fn topic_positions(
    client: &impl GenericClient,
    searcher: &Searcher,
    author: i64,
) -> Result<Vec<Vec<i64>>> {
    let mut rankings: Vec<Vec<i64>> = Vec::new();
    let mut counted: Option<HashSet<i64>> = None;
    if let Some(embedding) = topic_embedding(searcher) {
        let rows = client
            .query(
                "SELECT p.id, LEAST(p.embedding <=> $1, f.embedding <=> $1, d.embedding <=> $1)::float8
                 FROM positions p LEFT JOIN features f ON f.id = p.feature_id
                 LEFT JOIN decisions d ON d.id = p.decision_id
                 WHERE p.author_id = $2",
                &[embedding, &author],
            )
            .await?;
        let mut near: Vec<(i64, f64)> = rows
            .iter()
            .filter_map(|row| row.get::<_, Option<f64>>(1).map(|d| (row.get(0), d)))
            .collect();
        near.sort_by(|a, b| a.1.total_cmp(&b.1));
        if let Some(&(_, closest)) = near.first() {
            let cutoff = topic_cutoff(closest);
            near.retain(|&(_, distance)| distance <= cutoff);
        }
        // Positions not embedded yet still count when their text matches.
        let mut ids: HashSet<i64> = rows
            .iter()
            .filter(|row| row.get::<_, Option<f64>>(1).is_none())
            .map(|row| row.get(0))
            .collect();
        ids.extend(near.iter().map(|&(id, _)| id));
        counted = Some(ids);
        rankings.push(near.into_iter().map(|(id, _)| id).collect());
    }
    if searcher.mode != Mode::Semantic {
        let rows = client
            .query(
                "SELECT p.id,
                        (p.search_text <@> to_bm25query($1, 'pghacker.positions_bm25'))::float8,
                        (f.search_text <@> to_bm25query($1, 'pghacker.features_bm25'))::float8,
                        (d.search_text <@> to_bm25query($1, 'pghacker.decisions_bm25'))::float8
                 FROM positions p LEFT JOIN features f ON f.id = p.feature_id
                 LEFT JOIN decisions d ON d.id = p.decision_id
                 WHERE p.author_id = $2",
                &[&searcher.query(), &author],
            )
            .await?;
        for column in 1..=3 {
            let mut matched: Vec<(i64, f64)> = rows
                .iter()
                .filter_map(|row| {
                    let id: i64 = row.get(0);
                    let score = row
                        .get::<_, Option<f64>>(column)
                        .filter(|score| *score < 0.0)?;
                    counted
                        .as_ref()
                        .is_none_or(|ids| ids.contains(&id))
                        .then_some((id, score))
                })
                .collect();
            matched.sort_by(|a, b| a.1.total_cmp(&b.1));
            rankings.push(matched.into_iter().map(|(id, _)| id).collect());
        }
    }
    rankings.retain(|ranking| !ranking.is_empty());
    Ok(rankings)
}
