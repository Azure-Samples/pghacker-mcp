// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! What the tools share: search modes over chunk and extract rankings,
//! resolving authors, and snippets.

use anyhow::{Result, anyhow};
use pgvector::HalfVector;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::embed::Embedder;
use pghacker_core::search::{RRF_K, SearchMode, rrf};
use pghacker_core::text::truncate_chars;

/// The search mode argument: `fulltext`, `semantic` or `hybrid` (default).
pub type Mode = SearchMode;

/// Ranks by mode. Holds the query's embedding once computed.
pub struct Searcher<'a> {
    pub mode: Mode,
    embedder: Option<&'a Embedder>,
    query: String,
    embedding: Option<HalfVector>,
    /// What the caller should know about the mode, such as a fallback.
    pub notes: Vec<String>,
}

impl<'a> Searcher<'a> {
    pub async fn new(mode: Mode, query: &str, embedder: Option<&'a Embedder>) -> Result<Self> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow!("query is empty"));
        }
        let mut searcher = Self {
            mode,
            embedder,
            query: query.to_owned(),
            embedding: None,
            notes: Vec::new(),
        };
        if mode != Mode::Fulltext {
            match searcher.embedder {
                Some(embedder) => searcher.embedding = Some(embedder.embed_one(query).await?),
                None if mode == Mode::Semantic => {
                    return Err(anyhow!(
                        "semantic search isn't configured on this server; use mode fulltext"
                    ));
                }
                None => {
                    searcher.mode = Mode::Fulltext;
                    searcher.notes.push(
                        "semantic search isn't configured; hybrid fell back to fulltext".into(),
                    );
                }
            }
        }
        Ok(searcher)
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn embedding(&self) -> Option<&HalfVector> {
        self.embedding.as_ref()
    }

    /// Ranks rows of `table` (which has `search_text`, `embedding` and a BM25
    /// index `pghacker.<table>_bm25`) restricted by `filter` (a SQL condition on
    /// alias `t` that may use `$3`, bound to `filter_ids`). Returns ids, best first.
    pub async fn rank_table(
        &self,
        client: &impl GenericClient,
        table: &str,
        filter: &str,
        filter_ids: Option<&Vec<i64>>,
        depth: i64,
    ) -> Result<Vec<i64>> {
        let index = format!("pghacker.{table}_bm25");
        let mut rankings = Vec::new();
        if self.mode != Mode::Semantic {
            let rows = client
                .query(
                    &format!(
                        "SELECT t.id, (t.search_text <@> to_bm25query($1, '{index}'))::float8 FROM {table} t
                         WHERE {filter} ORDER BY t.search_text <@> to_bm25query($1, '{index}') LIMIT $2"
                    ),
                    &[&self.query, &depth, &filter_ids],
                )
                .await?;
            rankings.push(
                rows.iter()
                    .filter(|row| row.get::<_, f64>(1) < 0.0)
                    .map(|row| row.get(0))
                    .collect::<Vec<i64>>(),
            );
        }
        if self.mode != Mode::Fulltext
            && let Some(embedding) = &self.embedding
        {
            let rows = client
                .query(
                    &format!(
                        "SELECT t.id FROM {table} t WHERE t.embedding IS NOT NULL AND {filter}
                         ORDER BY t.embedding <=> $1 LIMIT $2"
                    ),
                    &[embedding, &depth, &filter_ids],
                )
                .await?;
            rankings.push(rows.iter().map(|row| row.get(0)).collect());
        }
        Ok(fuse(rankings))
    }

    /// Ranks message chunks restricted by optional authors and dates; returns
    /// (chunk id, thread id) pairs, best first.
    pub async fn rank_chunks(
        &self,
        client: &impl GenericClient,
        authors: Option<&Vec<i64>>,
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
        depth: i64,
    ) -> Result<Vec<(i64, i64)>> {
        let filter = "($3::bigint[] IS NULL OR m.author_id = ANY ($3))
                      AND ($4::timestamptz IS NULL OR m.sent_at >= $4)
                      AND ($5::timestamptz IS NULL OR m.sent_at < $5)";
        let mut rankings: Vec<Vec<(i64, i64)>> = Vec::new();
        if self.mode != Mode::Semantic {
            let rows = client
                .query(
                    &format!(
                        "SELECT c.id, c.thread_id,
                                (c.content <@> to_bm25query($1, 'pghacker.message_chunks_bm25'))::float8
                         FROM message_chunks c JOIN messages m ON m.id = c.message_id
                         WHERE {filter}
                         ORDER BY c.content <@> to_bm25query($1, 'pghacker.message_chunks_bm25') LIMIT $2"
                    ),
                    &[&self.query, &depth, &authors, &since, &until],
                )
                .await?;
            rankings.push(
                rows.iter()
                    .filter(|row| row.get::<_, f64>(2) < 0.0)
                    .map(|row| (row.get(0), row.get(1)))
                    .collect(),
            );
        }
        if self.mode != Mode::Fulltext
            && let Some(embedding) = &self.embedding
        {
            let rows = client
                .query(
                    &format!(
                        "SELECT c.id, c.thread_id FROM message_chunks c JOIN messages m ON m.id = c.message_id
                         WHERE c.embedding IS NOT NULL AND {filter}
                         ORDER BY c.embedding <=> $1 LIMIT $2"
                    ),
                    &[embedding, &depth, &authors, &since, &until],
                )
                .await?;
            rankings.push(rows.iter().map(|row| (row.get(0), row.get(1))).collect());
        }
        Ok(fuse(rankings))
    }
}

/// One ranking as is, or several fused.
fn fuse<T: Clone + Eq + std::hash::Hash>(mut rankings: Vec<Vec<T>>) -> Vec<T> {
    if rankings.len() == 1 {
        return rankings.pop().unwrap();
    }
    rrf(&rankings, RRF_K)
        .into_iter()
        .map(|(item, _)| item)
        .collect()
}

/// An author, by id or by name.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AuthorRef {
    Id(i64),
    Name(String),
}

/// The author's id: an id as given, or the best name match. A name that
/// matches several authors about equally is an error listing them.
pub async fn resolve_author(client: &impl GenericClient, author: &AuthorRef) -> Result<i64> {
    let name = match author {
        AuthorRef::Id(id) => return final_author(client, *id).await,
        AuthorRef::Name(name) => name.trim(),
    };
    if let Ok(id) = name.parse::<i64>() {
        return final_author(client, id).await;
    }
    let rows = client
        .query(
            "SELECT a.id, a.display_name, a.message_count, max(similarity(n.name, $1))::float8 AS score
             FROM author_names n JOIN authors a ON a.id = n.author_id
             WHERE a.merged_into IS NULL AND (n.name % $1 OR n.name ILIKE '%' || $1 || '%')
             GROUP BY a.id ORDER BY score DESC, a.message_count DESC LIMIT 5",
            &[&name],
        )
        .await?;
    let Some(best) = rows.first() else {
        return Err(anyhow!("no author matches {name}; try search_authors"));
    };
    let exact = rows
        .iter()
        .find(|row| row.get::<_, String>(1).eq_ignore_ascii_case(name));
    if let Some(row) = exact {
        return Ok(row.get(0));
    }
    let best_score: f64 = best.get(3);
    let close: Vec<&tokio_postgres::Row> = rows
        .iter()
        .filter(|row| best_score - row.get::<_, f64>(3) < 0.05)
        .collect();
    if close.len() > 1 {
        let names: Vec<String> = close
            .iter()
            .map(|row| {
                format!(
                    "{} (id {}, {} messages)",
                    row.get::<_, String>(1),
                    row.get::<_, i64>(0),
                    row.get::<_, i32>(2)
                )
            })
            .collect();
        return Err(anyhow!(
            "{name} matches several authors: {}; pass an id",
            names.join(", ")
        ));
    }
    Ok(best.get(0))
}

/// Follows merges to the surviving author.
async fn final_author(client: &impl GenericClient, id: i64) -> Result<i64> {
    let row = client
        .query_opt(
            "WITH RECURSIVE chain AS (
                 SELECT id, merged_into, 0 AS depth FROM authors WHERE id = $1
                 UNION ALL SELECT a.id, a.merged_into, c.depth + 1 FROM authors a JOIN chain c ON a.id = c.merged_into
                 WHERE c.depth < 10)
             SELECT id FROM chain WHERE merged_into IS NULL LIMIT 1",
            &[&id],
        )
        .await?;
    row.map(|row| row.get(0))
        .ok_or_else(|| anyhow!("there's no author {id}"))
}

/// A passage of `text` around the first query word it contains.
pub fn snippet(text: &str, query: &str, width: usize) -> String {
    let lower = text.to_lowercase();
    let start = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| word.len() >= 3)
        .filter_map(|word| lower.find(&word.to_lowercase()))
        .min()
        .unwrap_or(0);
    let mut begin = start.saturating_sub(width / 3);
    while !text.is_char_boundary(begin) {
        begin -= 1;
    }
    let passage = truncate_chars(&text[begin..], width);
    let mut out = String::new();
    if begin > 0 {
        out.push('…');
    }
    out.push_str(&passage.split_whitespace().collect::<Vec<_>>().join(" "));
    if passage.len() < text.len() - begin {
        out.push('…');
    }
    out
}

/// Citations of extracts, by (kind, id): Message-ID, author, date, quote, thread.
pub async fn citations(
    client: &impl GenericClient,
    kind: &str,
    ids: &[i64],
    per_extract: usize,
) -> Result<std::collections::HashMap<i64, Vec<Value>>> {
    let rows = client
        .query(
            "SELECT c.extract_id, m.message_id, coalesce(a.display_name, m.from_name), m.sent_at, c.quote, m.thread_id, t.subject
             FROM citations c JOIN messages m ON m.id = c.message_id
             LEFT JOIN authors a ON a.id = m.author_id JOIN threads t ON t.id = m.thread_id
             WHERE c.extract_kind = $1 AND c.extract_id = ANY ($2)
             ORDER BY c.extract_id, m.sent_at, c.id",
            &[&kind, &ids],
        )
        .await?;
    let mut out: std::collections::HashMap<i64, Vec<Value>> = std::collections::HashMap::new();
    for row in rows {
        let list = out.entry(row.get(0)).or_default();
        if list.len() < per_extract {
            list.push(json!({
                "message_id": row.get::<_, String>(1),
                "author": row.get::<_, String>(2),
                "sent_at": row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(3),
                "quote": row.get::<_, String>(4),
                "thread_id": row.get::<_, i64>(5),
                "thread": row.get::<_, String>(6),
            }));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_center_on_query_words() {
        let text = "Lots of preamble here. Then the JSON_TABLE plan clause is discussed at length, with details.";
        let s = snippet(text, "json_table plan", 40);
        assert!(s.contains("JSON_TABLE"), "{s}");
        assert!(s.starts_with('…'));
        assert_eq!(snippet("short", "nothing", 40), "short");
    }
}
