// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! find_duplicates' candidates: the duplicates job compares each extract as
//! it arrives, and find_duplicates lists what comparing every pair of live
//! extracts listed, on synthetic embeddings shaped like the corpus's.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, duplicates, extraction};
use pgvector::HalfVector;
use serde_json::{Value, json};
use tokio_postgres::Client;

const DIM: usize = 1536;

async fn indexer(db: &TestDb) -> Indexer {
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let env = Env::from_pairs([("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str())]);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    Indexer::new(settings, &env).unwrap()
}

/// A database with a thread for decisions to come from.
async fn setup() -> (TestDb, Client, Indexer) {
    let db = TestDb::create().await.unwrap();
    let client = db.indexer().await.unwrap();
    client
        .batch_execute(
            "INSERT INTO threads (id, list, root_message_id, subject) VALUES (1, 'pgsql-hackers', 'root@example.org', 'Root')",
        )
        .await
        .unwrap();
    let indexer = indexer(&db).await;
    (db, client, indexer)
}

/// A seeded generator (xorshift64*), as the graph tests use.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn between(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    fn gauss(&mut self) -> f32 {
        let u = self.unit().max(f64::MIN_POSITIVE);
        let v = self.unit();
        ((-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()) as f32
    }

    /// A random unit vector.
    fn direction(&mut self) -> Vec<f32> {
        normalized((0..DIM).map(|_| self.gauss()).collect())
    }

    /// A made-up word of three syllables: 8,000 of them.
    fn word(&mut self) -> String {
        const SYLLABLES: [&str; 20] = [
            "ka", "me", "lo", "ri", "tu", "sa", "va", "ne", "pi", "do", "ru", "ke", "mi", "so",
            "la", "te", "no", "bu", "fa", "gi",
        ];
        (0..3).map(|_| SYLLABLES[self.below(20)]).collect()
    }

    fn name(&mut self) -> String {
        (0..3).map(|_| self.word()).collect::<Vec<_>>().join(" ")
    }
}

fn normalized(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

fn mix(parts: &[(&[f32], f32)]) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    for (part, weight) in parts {
        for (x, y) in v.iter_mut().zip(part.iter()) {
            *x += weight * y;
        }
    }
    normalized(v)
}

/// Embeddings shaped like text embeddings: each leans on a common direction
/// and on one of the topics, so unrelated extracts are about 0.2 alike and
/// extracts of one topic about 0.55.
struct Shape {
    common: Vec<f32>,
    topics: Vec<Vec<f32>>,
}

impl Shape {
    fn new(rng: &mut Rng, topics: usize) -> Self {
        Self {
            common: rng.direction(),
            topics: (0..topics).map(|_| rng.direction()).collect(),
        }
    }

    fn embedding(&self, rng: &mut Rng) -> Vec<f32> {
        let topic = &self.topics[rng.below(self.topics.len())];
        mix(&[(&self.common, 0.45), (topic, 0.6), (&rng.direction(), 0.67)])
    }
}

/// An embedding about `similarity` alike to `of`.
fn near(of: &[f32], similarity: f64, rng: &mut Rng) -> Vec<f32> {
    let noise = (1.0 / (similarity * similarity) - 1.0).sqrt() as f32;
    mix(&[(of, 1.0), (&rng.direction(), noise)])
}

#[derive(Clone)]
struct Extract {
    name: String,
    embedding: Option<Vec<f32>>,
}

/// Stores `extracts` of `kind`; returns their ids, in order.
async fn insert(client: &mut Client, kind: &str, extracts: &[Extract]) -> Vec<i64> {
    let tx = client.transaction().await.unwrap();
    let mut ids = Vec::with_capacity(extracts.len());
    if kind == "feature" {
        let statement = tx
            .prepare(
                "INSERT INTO features (slug, name, description, status, embedding)
                 VALUES ($1, $2, 'A synthetic feature.', 'proposed', $3) RETURNING id",
            )
            .await
            .unwrap();
        for extract in extracts {
            let slug = format!("f-{}", uuid::Uuid::new_v4().simple());
            let embedding = extract.embedding.as_deref().map(HalfVector::from_f32_slice);
            let row = tx
                .query_one(&statement, &[&slug, &extract.name, &embedding])
                .await
                .unwrap();
            ids.push(row.get(0));
        }
    } else {
        let statement = tx
            .prepare(
                "INSERT INTO decisions (thread_id, title, outcome, rationale, embedding)
                 VALUES (1, $1, 'An outcome.', 'A rationale.', $2) RETURNING id",
            )
            .await
            .unwrap();
        for extract in extracts {
            let embedding = extract.embedding.as_deref().map(HalfVector::from_f32_slice);
            let row = tx
                .query_one(&statement, &[&extract.name, &embedding])
                .await
                .unwrap();
            ids.push(row.get(0));
        }
    }
    tx.commit().await.unwrap();
    ids
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

type Pairs = BTreeMap<(i64, i64), (Option<f64>, f64)>;

fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// The pairs that find_duplicates listed before it had candidates: every
/// pair of live extracts, compared as it compared them.
async fn all_pairs(client: &Client, kind: &str, min: f64) -> Pairs {
    let (table, label) = if kind == "feature" {
        ("features", "name")
    } else {
        ("decisions", "title")
    };
    client
        .query(
            &format!(
                "SELECT a.id, b.id,
                        (1 - (a.embedding <=> b.embedding))::float8 AS semantic,
                        similarity(a.{label}, b.{label})::float8 AS name_similarity
                 FROM {table} a JOIN {table} b ON a.id < b.id
                 WHERE a.merged_into IS NULL AND b.merged_into IS NULL
                   AND ((a.embedding IS NOT NULL AND b.embedding IS NOT NULL AND 1 - (a.embedding <=> b.embedding) >= $1::float8)
                        OR similarity(a.{label}, b.{label}) >= $2::float8)
                   AND NOT EXISTS (SELECT FROM distinct_pairs p WHERE p.kind = $3 AND p.low_id = a.id AND p.high_id = b.id)"
            ),
            &[&min, &extraction::NAME_DUPLICATE, &kind],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                (row.get(0), row.get(1)),
                (row.get::<_, Option<f64>>(2).map(round), round(row.get(3))),
            )
        })
        .collect()
}

/// find_duplicates' answer, and its pairs.
async fn listed(indexer: &Indexer, kind: &str, min: f64) -> (Value, Pairs) {
    let found = extraction::find_duplicates(
        indexer,
        args(json!({ "kind": kind, "min_similarity": min, "limit": 100 })),
    )
    .await
    .unwrap();
    let pairs = found["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                (
                    pair["a"]["id"].as_i64().unwrap(),
                    pair["b"]["id"].as_i64().unwrap(),
                ),
                (
                    pair["semantic"].as_f64(),
                    pair["name_similarity"].as_f64().unwrap(),
                ),
            )
        })
        .collect();
    (found, pairs)
}

async fn refresh(indexer: &Indexer) -> duplicates::Report {
    let mut client = indexer.pool.get().await.unwrap();
    duplicates::refresh(&mut client, Duration::from_secs(600))
        .await
        .unwrap()
}

/// Runs the duplicates job until nothing waits; returns what it compared
/// and kept.
async fn refresh_all(indexer: &Indexer) -> (u64, u64) {
    let (mut checked, mut found) = (0, 0);
    loop {
        let report = refresh(indexer).await;
        checked += report.checked;
        found += report.found;
        if !report.more {
            return (checked, found);
        }
    }
}

/// A corpus of `ordinary` extracts with planted look-alikes: pairs alike by
/// embedding above, near and below the thresholds, a cluster of four alike,
/// pairs alike by name only, and extracts without an embedding.
fn corpus(rng: &mut Rng, shape: &Shape, ordinary: usize) -> Vec<Extract> {
    let mut extracts: Vec<Extract> = (0..ordinary)
        .map(|_| Extract {
            name: rng.name(),
            embedding: Some(shape.embedding(rng)),
        })
        .collect();
    let mut plant = |rng: &mut Rng, count: usize, low: f64, high: f64| {
        for _ in 0..count {
            let of = extracts[rng.below(ordinary)].clone();
            let similarity = rng.between(low, high);
            extracts.push(Extract {
                name: rng.name(),
                embedding: Some(near(of.embedding.as_ref().unwrap(), similarity, rng)),
            });
        }
    };
    plant(rng, 20, 0.87, 0.98);
    plant(rng, 10, 0.815, 0.84);
    plant(rng, 6, 0.70, 0.78);
    let center = shape.embedding(rng);
    for _ in 0..4 {
        extracts.push(Extract {
            name: rng.name(),
            embedding: Some(near(&center, 0.96, rng)),
        });
    }
    for _ in 0..15 {
        let of = rng.below(ordinary);
        let name = format!("{} {}", extracts[of].name, rng.word());
        extracts.push(Extract {
            name,
            embedding: Some(shape.embedding(rng)),
        });
    }
    for i in 0..10 {
        let name = if i < 5 {
            format!("{} {}", extracts[rng.below(ordinary)].name, rng.word())
        } else {
            rng.name()
        };
        extracts.push(Extract {
            name,
            embedding: None,
        });
    }
    extracts
}

#[tokio::test]
async fn candidates_are_the_pairs_that_comparing_every_pair_finds() {
    let (_db, mut client, indexer) = setup().await;
    let mut rng = Rng(0x5eed_d0d0_cafe_f00d);
    let shape = Shape::new(&mut rng, 30);
    for (kind, ordinary) in [("feature", 900), ("decision", 600)] {
        let extracts = corpus(&mut rng, &shape, ordinary);
        let ids = insert(&mut client, kind, &extracts).await;
        let table = if kind == "feature" {
            "features"
        } else {
            "decisions"
        };
        // Look-alikes of live extracts, merged into them: left out.
        let merged: Vec<Extract> = (0..3)
            .map(|i| Extract {
                name: extracts[i].name.clone(),
                embedding: Some(near(
                    extracts[i].embedding.as_ref().unwrap(),
                    0.95,
                    &mut rng,
                )),
            })
            .collect();
        let merged_ids = insert(&mut client, kind, &merged).await;
        for (i, id) in merged_ids.iter().enumerate() {
            client
                .execute(
                    &format!("UPDATE {table} SET merged_into = $1 WHERE id = $2"),
                    &[&ids[i], id],
                )
                .await
                .unwrap();
        }
        // Two planted pairs judged different: left out.
        let judged: Vec<(i64, i64)> = all_pairs(&client, kind, 0.85)
            .await
            .into_keys()
            .take(2)
            .collect();
        for (low, high) in &judged {
            client
                .execute(
                    "INSERT INTO distinct_pairs (kind, low_id, high_id, reason, judged_by) VALUES ($1, $2, $3, 'They differ.', 'dedup')",
                    &[&kind, low, high],
                )
                .await
                .unwrap();
        }
    }

    // Merged look-alikes aren't compared: they're no longer extracts of their own.
    let report = refresh(&indexer).await;
    assert_eq!(report.checked, (900 + 65) + (600 + 65), "{report:?}");
    assert!(!report.more);
    for kind in ["feature", "decision"] {
        for min in [0.85, 0.8] {
            let expected = all_pairs(&client, kind, min).await;
            assert!(expected.len() < 100, "{kind} {min}: {}", expected.len());
            let (found, pairs) = listed(&indexer, kind, min).await;
            assert_eq!(pairs, expected, "{kind} at {min}: {found:#}");
            assert_eq!(found["pending"], 0);
            assert_eq!(found["judged_distinct"], 2);
        }
        // Planted: 20 alike from 0.87, 6 in the cluster of four, 15 alike by
        // name, and 5 alike by name without an embedding.
        let (_, pairs) = listed(&indexer, kind, 0.85).await;
        let semantic = pairs
            .values()
            .filter(|(semantic, _)| semantic.is_some_and(|s| s >= 0.85))
            .count();
        let by_name = pairs
            .values()
            .filter(|(semantic, name)| semantic.is_none_or(|s| s < 0.85) && *name >= 0.6)
            .count();
        let unembedded = pairs
            .values()
            .filter(|(semantic, _)| semantic.is_none())
            .count();
        assert!(
            semantic >= 20 + 6 - 2,
            "{kind}: {semantic} alike by embedding"
        );
        assert!(by_name >= 15 + 5 - 2, "{kind}: {by_name} alike by name");
        assert!(unembedded >= 3, "{kind}: {unembedded} without an embedding");
        // The pairs from 0.8 to 0.85 are listed only when asked for.
        let (_, wider) = listed(&indexer, kind, 0.8).await;
        assert!(
            wider.len() >= pairs.len() + 8,
            "{kind}: {} then {}",
            pairs.len(),
            wider.len()
        );
    }
}

#[tokio::test]
async fn new_extracts_late_embeddings_and_undone_merges_are_compared() {
    let (_db, mut client, indexer) = setup().await;
    let mut rng = Rng(0xfeed_beef_1234_5678);
    let shape = Shape::new(&mut rng, 8);
    let base: Vec<Extract> = (0..40)
        .map(|_| Extract {
            name: rng.name(),
            embedding: Some(shape.embedding(&mut rng)),
        })
        .collect();
    let ids = insert(&mut client, "feature", &base).await;
    let report = refresh(&indexer).await;
    assert_eq!(report.checked, 40);
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(pairs.is_empty(), "{found:#}");
    assert_eq!(found["pending"], 0);

    // A new extract alike by name waits until the job compares it.
    let x = Extract {
        name: format!("{} {}", base[0].name, rng.word()),
        embedding: Some(shape.embedding(&mut rng)),
    };
    let x_id = insert(&mut client, "feature", std::slice::from_ref(&x)).await[0];
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(pairs.is_empty(), "{found:#}");
    assert_eq!(found["pending"], 1);
    assert!(
        found["next"]
            .as_str()
            .unwrap()
            .contains("1 feature isn't compared yet"),
        "{found:#}"
    );
    assert_eq!(refresh(&indexer).await.checked, 1);
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert_eq!(
        pairs.keys().collect::<Vec<_>>(),
        [&(ids[0], x_id)],
        "{found:#}"
    );
    assert_eq!(found["pending"], 0);

    // One stored without an embedding is compared by name, then again by
    // its embedding once it has one.
    let y_id = insert(
        &mut client,
        "feature",
        &[Extract {
            name: format!("{} {}", base[1].name, rng.word()),
            embedding: None,
        }],
    )
    .await[0];
    refresh(&indexer).await;
    let (_, pairs) = listed(&indexer, "feature", 0.85).await;
    assert_eq!(pairs[&(ids[1], y_id)].0, None);
    let late =
        HalfVector::from_f32_slice(&near(base[2].embedding.as_ref().unwrap(), 0.92, &mut rng));
    client
        .execute(
            "UPDATE features SET embedding = $1 WHERE id = $2",
            &[&late, &y_id],
        )
        .await
        .unwrap();
    let (found, _) = listed(&indexer, "feature", 0.85).await;
    assert_eq!(found["pending"], 1, "{found:#}");
    assert_eq!(refresh(&indexer).await.checked, 1);
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(pairs[&(ids[2], y_id)].0.unwrap() >= 0.85, "{found:#}");
    assert!(pairs[&(ids[1], y_id)].0.unwrap() < 0.85, "{found:#}");

    // A merged extract drops out. One alike to it that comes meanwhile isn't
    // compared with it; undoing the merge compares them.
    let merged = extraction::merge_extracts(
        &indexer,
        args(json!({ "kind": "feature", "from_id": x_id, "into_id": ids[0], "reason": "The same feature" })),
    )
    .await
    .unwrap();
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(!pairs.contains_key(&(ids[0], x_id)), "{found:#}");
    let z_id = insert(
        &mut client,
        "feature",
        &[Extract {
            name: rng.name(),
            embedding: Some(near(x.embedding.as_ref().unwrap(), 0.95, &mut rng)),
        }],
    )
    .await[0];
    refresh(&indexer).await;
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(!pairs.keys().any(|(_, high)| *high == z_id), "{found:#}");
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    let (found, _) = listed(&indexer, "feature", 0.85).await;
    assert_eq!(found["pending"], 1, "{found:#}");
    refresh(&indexer).await;
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(pairs.contains_key(&(ids[0], x_id)), "{found:#}");
    assert!(pairs[&(x_id, z_id)].0.unwrap() >= 0.85, "{found:#}");

    // A pair judged different isn't listed again.
    extraction::mark_distinct(
        &indexer,
        args(json!({ "kind": "feature", "a_id": x_id, "b_id": ids[0], "reason": "One extends the other." })),
    )
    .await
    .unwrap();
    let (found, pairs) = listed(&indexer, "feature", 0.85).await;
    assert!(!pairs.contains_key(&(ids[0], x_id)), "{found:#}");
    assert_eq!(found["judged_distinct"], 1);

    // Candidates are kept from 0.8: a lower minimum is raised to it.
    let (found, _) = listed(&indexer, "feature", 0.5).await;
    assert_eq!(found["min_similarity"], 0.8);
}

/// Times the duplicates job and find_duplicates on twice today's corpus:
/// 26,000 features and 19,000 decisions, with look-alikes as often as the
/// corpus has them (about 1% by embedding from 0.85, 3% by name).
#[tokio::test]
#[ignore = "production scale, minutes: cargo test -p pghacker-indexer --test duplicates -- --ignored --nocapture"]
async fn production_scale() {
    let (_db, mut client, indexer) = setup().await;
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let shape = Shape::new(&mut rng, 400);
    let started = Instant::now();
    client
        .batch_execute(
            "DROP INDEX features_embedding; DROP INDEX decisions_embedding;
             SET maintenance_work_mem = '1GB';",
        )
        .await
        .unwrap();
    for (kind, count) in [("feature", 26_000), ("decision", 19_000)] {
        let mut extracts: Vec<Extract> = Vec::with_capacity(count);
        for i in 0..count {
            let extract = if i > 100 && rng.below(100) < 1 {
                let of = extracts[rng.below(i)].clone();
                Extract {
                    name: rng.name(),
                    embedding: Some(near(
                        of.embedding.as_ref().unwrap(),
                        rng.between(0.85, 0.98),
                        &mut rng,
                    )),
                }
            } else if i > 100 && rng.below(100) < 3 {
                let of = extracts[rng.below(i)].name.clone();
                Extract {
                    name: format!("{of} {}", rng.word()),
                    embedding: Some(shape.embedding(&mut rng)),
                }
            } else {
                Extract {
                    name: rng.name(),
                    embedding: Some(shape.embedding(&mut rng)),
                }
            };
            extracts.push(extract);
        }
        for chunk in extracts.chunks(2_000) {
            insert(&mut client, kind, chunk).await;
        }
    }
    client
        .batch_execute(
            "CREATE INDEX features_embedding ON features USING hnsw (embedding halfvec_cosine_ops);
             CREATE INDEX decisions_embedding ON decisions USING hnsw (embedding halfvec_cosine_ops);
             ANALYZE features; ANALYZE decisions;",
        )
        .await
        .unwrap();
    println!(
        "loaded 45,000 extracts in {:.0} s",
        started.elapsed().as_secs_f64()
    );

    let started = Instant::now();
    let (checked, found) = refresh_all(&indexer).await;
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "compared {checked} extracts in {seconds:.1} s ({:.2} ms each), {found} pairs kept",
        seconds * 1000.0 / checked as f64,
    );
    assert_eq!(checked, 45_000);

    for kind in ["feature", "decision"] {
        let mut times = Vec::new();
        let mut listed_pairs = 0;
        for _ in 0..20 {
            let started = Instant::now();
            let found =
                extraction::find_duplicates(&indexer, args(json!({ "kind": kind, "limit": 100 })))
                    .await
                    .unwrap();
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            listed_pairs = found["pairs"].as_array().unwrap().len();
        }
        times.sort_by(f64::total_cmp);
        println!(
            "find_duplicates {kind}: p50 {:.1} ms, max {:.1} ms, {listed_pairs} pairs listed",
            times[times.len() / 2],
            times[times.len() - 1]
        );
        assert!(times[times.len() / 2] < 500.0);
    }

    let new: Vec<Extract> = (0..300)
        .map(|_| Extract {
            name: rng.name(),
            embedding: Some(shape.embedding(&mut rng)),
        })
        .collect();
    insert(&mut client, "feature", &new).await;
    let started = Instant::now();
    let report = refresh(&indexer).await;
    println!(
        "compared {} new features in {:.0} ms",
        report.checked,
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(report.checked, 300);

    // What find_duplicates did before, on the first 5,000 features alone.
    client
        .batch_execute("CREATE TABLE five AS SELECT * FROM features ORDER BY id LIMIT 5000")
        .await
        .unwrap();
    let started = Instant::now();
    let row = client
        .query_one(
            "SELECT count(*) FROM five a JOIN five b ON a.id < b.id
             WHERE (1 - (a.embedding <=> b.embedding) >= 0.85 OR similarity(a.name, b.name) >= 0.6)",
            &[],
        )
        .await
        .unwrap();
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "comparing every pair of 5,000 features: {seconds:.1} s, {} pairs; at 13,000 about {:.0} s, at 26,000 about {:.0} s",
        row.get::<_, i64>(0),
        seconds * (13.0f64 / 5.0).powi(2),
        seconds * (26.0f64 / 5.0).powi(2)
    );
}
