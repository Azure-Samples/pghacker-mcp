//! Search modes: full text ranks by BM25 (pg_textsearch), semantic by
//! pgvector cosine distance, and hybrid fuses both rankings with reciprocal
//! rank fusion.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// BM25 over the words.
    Fulltext,
    /// Vector similarity to the query's embedding.
    Semantic,
    /// Both rankings fused: the default.
    #[default]
    Hybrid,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fulltext => "fulltext",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
        }
    }
}

/// The constant in reciprocal rank fusion's 1 / (k + rank).
pub const RRF_K: f64 = 60.0;

/// Fuses rankings, each best first, into one: score = sum of 1 / (k + rank),
/// with ranks from 1. Ties keep the order of first appearance.
pub fn rrf<T: Clone + Eq + std::hash::Hash>(rankings: &[Vec<T>], k: f64) -> Vec<(T, f64)> {
    let mut scores: HashMap<T, (f64, usize)> = HashMap::new();
    let mut order = 0;
    for ranking in rankings {
        for (rank, item) in ranking.iter().enumerate() {
            let entry = scores.entry(item.clone()).or_insert_with(|| {
                order += 1;
                (0.0, order)
            });
            entry.0 += 1.0 / (k + (rank + 1) as f64);
        }
    }
    let mut fused: Vec<(T, f64, usize)> = scores
        .into_iter()
        .map(|(item, (score, first))| (item, score, first))
        .collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    fused
        .into_iter()
        .map(|(item, score, _)| (item, score))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuses_rankings() {
        let fused = rrf(&[vec![1, 2, 3], vec![3, 1, 4]], RRF_K);
        let order: Vec<i32> = fused.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![1, 3, 2, 4]);
        assert!((fused[0].1 - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
    }

    #[test]
    fn modes_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&SearchMode::Fulltext).unwrap(),
            "\"fulltext\""
        );
        assert_eq!(SearchMode::default(), SearchMode::Hybrid);
    }
}
