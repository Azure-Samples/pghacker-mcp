// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! search_extracts: features, decisions and positions with their citations.

use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

use pghacker_core::embed::Embedder;
use pghacker_core::search::{RRF_K, rrf};

use crate::common::{AuthorRef, Mode, Searcher, citations, resolve_author};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ExtractKind {
    Feature,
    Decision,
    Position,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchExtractsArgs {
    pub query: String,
    /// fulltext, semantic or hybrid (default).
    #[serde(default)]
    pub mode: Mode,
    /// Only this kind; default all three.
    #[serde(default)]
    pub kind: Option<ExtractKind>,
    /// Only positions held by this author (id or name).
    #[serde(default)]
    pub author: Option<AuthorRef>,
    /// Results, 1 to 30. Default 10.
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn search_extracts(
    client: &impl GenericClient,
    embedder: Option<&Embedder>,
    args: SearchExtractsArgs,
) -> Result<Value> {
    let limit = args.limit.unwrap_or(10).clamp(1, 30) as usize;
    let searcher = Searcher::new(args.mode, &args.query, embedder).await?;
    let author = match &args.author {
        Some(author) => Some(resolve_author(client, author).await?),
        None => None,
    };
    let kinds: Vec<ExtractKind> = match (args.kind, author) {
        (Some(kind), _) => vec![kind],
        (None, Some(_)) => vec![ExtractKind::Position],
        (None, None) => vec![
            ExtractKind::Feature,
            ExtractKind::Decision,
            ExtractKind::Position,
        ],
    };
    let authors = author.map(|id| vec![id]);
    let mut rankings: Vec<Vec<(ExtractKind, i64)>> = Vec::new();
    for kind in &kinds {
        let (table, filter) = match kind {
            ExtractKind::Feature => (
                "features",
                "t.merged_into IS NULL AND cardinality(coalesce($3::bigint[], '{}')) >= 0",
            ),
            ExtractKind::Decision => (
                "decisions",
                "t.merged_into IS NULL AND cardinality(coalesce($3::bigint[], '{}')) >= 0",
            ),
            ExtractKind::Position => (
                "positions",
                "($3::bigint[] IS NULL OR t.author_id = ANY ($3))",
            ),
        };
        let ids = searcher
            .rank_table(client, table, filter, authors.as_ref(), 60)
            .await?;
        rankings.push(ids.into_iter().map(|id| (*kind, id)).collect());
    }
    let fused: Vec<(ExtractKind, i64)> = rrf(&rankings, RRF_K)
        .into_iter()
        .map(|(item, _)| item)
        .take(limit)
        .collect();
    let ids_of = |kind: ExtractKind| -> Vec<i64> {
        fused
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, id)| *id)
            .collect()
    };
    let mut items: std::collections::HashMap<(ExtractKind, i64), Value> =
        std::collections::HashMap::new();

    let feature_ids = ids_of(ExtractKind::Feature);
    let feature_cites = citations(client, "feature", &feature_ids, 3).await?;
    for row in client
        .query(
            "SELECT f.id, f.slug, f.name, f.description, f.status, f.release,
                    (SELECT count(*) FROM feature_threads ft WHERE ft.feature_id = f.id),
                    (SELECT array_agg(path ORDER BY path) FROM feature_files ff WHERE ff.feature_id = f.id)
             FROM features f WHERE f.id = ANY ($1)",
            &[&feature_ids],
        )
        .await?
    {
        let id: i64 = row.get(0);
        items.insert((ExtractKind::Feature, id), json!({
            "kind": "feature", "id": id, "slug": row.get::<_, String>(1), "name": row.get::<_, String>(2),
            "description": row.get::<_, String>(3), "status": row.get::<_, String>(4),
            "release": row.get::<_, Option<String>>(5), "threads": row.get::<_, i64>(6),
            "files": row.get::<_, Option<Vec<String>>>(7).unwrap_or_default(),
            "citations": feature_cites.get(&id).cloned().unwrap_or_default(),
        }));
    }
    let decision_ids = ids_of(ExtractKind::Decision);
    let decision_cites = citations(client, "decision", &decision_ids, 3).await?;
    for row in client
        .query(
            "SELECT d.id, d.title, d.outcome, d.alternatives, d.rationale, d.decided_at, f.slug, f.name, d.thread_id
             FROM decisions d LEFT JOIN features f ON f.id = d.feature_id WHERE d.id = ANY ($1)",
            &[&decision_ids],
        )
        .await?
    {
        let id: i64 = row.get(0);
        items.insert((ExtractKind::Decision, id), json!({
            "kind": "decision", "id": id, "title": row.get::<_, String>(1), "outcome": row.get::<_, String>(2),
            "alternatives": row.get::<_, String>(3), "rationale": row.get::<_, String>(4),
            "decided_at": row.get::<_, Option<DateTime<Utc>>>(5),
            "feature": row.get::<_, Option<String>>(6).map(|slug| json!({ "slug": slug, "name": row.get::<_, Option<String>>(7) })),
            "thread_id": row.get::<_, i64>(8),
            "citations": decision_cites.get(&id).cloned().unwrap_or_default(),
        }));
    }
    let position_ids = ids_of(ExtractKind::Position);
    let position_cites = citations(client, "position", &position_ids, 3).await?;
    for row in client
        .query(
            "SELECT p.id, a.display_name, p.author_id, p.stance, p.summary, f.slug, f.name, d.id, d.title, p.thread_id
             FROM positions p JOIN authors a ON a.id = p.author_id
             LEFT JOIN features f ON f.id = p.feature_id LEFT JOIN decisions d ON d.id = p.decision_id
             WHERE p.id = ANY ($1)",
            &[&position_ids],
        )
        .await?
    {
        let id: i64 = row.get(0);
        items.insert((ExtractKind::Position, id), json!({
            "kind": "position", "id": id, "author": row.get::<_, String>(1), "author_id": row.get::<_, i64>(2),
            "stance": row.get::<_, String>(3), "summary": row.get::<_, String>(4),
            "feature": row.get::<_, Option<String>>(5).map(|slug| json!({ "slug": slug, "name": row.get::<_, Option<String>>(6) })),
            "decision": row.get::<_, Option<i64>>(7).map(|id| json!({ "id": id, "title": row.get::<_, Option<String>>(8) })),
            "thread_id": row.get::<_, i64>(9),
            "citations": position_cites.get(&id).cloned().unwrap_or_default(),
        }));
    }
    let results: Vec<Value> = fused.iter().filter_map(|key| items.remove(key)).collect();
    let mut out = json!({ "mode": searcher.mode.as_str(), "results": results });
    if !searcher.notes.is_empty() {
        out["notes"] = json!(searcher.notes);
    }
    Ok(out)
}
