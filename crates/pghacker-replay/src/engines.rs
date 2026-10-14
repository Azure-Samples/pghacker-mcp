//! The gate for replacing the snapshots with the commit history: the same
//! code-tool calls, at the commits production keeps snapshots of, through
//! the snapshot engine and the history engine. Their output must be the
//! same, and their latencies close.
//!
//! Database `pghacker_bench` holds both: the 15 snapshots, loaded from a
//! local clone, and that clone's whole history in schema pghacker_history.
//! `--database` and `--schema` point the run elsewhere, such as a
//! deployment's database with its history installed in pghacker.
//! The workloads are the generated PG19 searches (as grep command lines and
//! as searches), the profiling program's searches, and the grep command
//! lines of the last study replay. Per-call records go to
//! `.runtime/replay/engines-*.jsonl`; the console shows aggregates.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use pghacker_code::history::BuildOptions;
use pghacker_code::{RepoSource, SnapshotSpec};
use pghacker_codesearch::grep::{self as grep_tool, Limits};
use pghacker_codesearch::search::{GroupBy, MatchMode, OutputMode, Strings};
use pghacker_codesearch::{SearchArgs, Snapshot, search};
use pghacker_core::config::with_database;
use pghacker_core::db;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Ctx, percentile};

pub const DATABASE: &str = "pghacker_bench";
pub const SCHEMA: &str = "pghacker_history";

/// Production's snapshots on 2026-10-14: (version, ref, commit).
pub const HEADS: [(&str, &str, &str); 15] = [
    (
        "19",
        "REL_19_STABLE",
        "28f836dacafb3a0e4073bf42a8d2a7529565534b",
    ),
    (
        "18",
        "REL_18_STABLE",
        "3c6135f839d03ef96a7313bca09ba4c1ec8dd161",
    ),
    (
        "master",
        "master",
        "f58ab18a90460cd297249bc80b13d84b2671bf0c",
    ),
    (
        "17",
        "REL_17_STABLE",
        "35c508af520963bf1245b86f437c21f834cc2be0",
    ),
    (
        "14",
        "REL_14_STABLE",
        "76a6c5da407e461b337ff48dbdbefd144f392187",
    ),
    (
        "16",
        "REL_16_STABLE",
        "b87cc7a3eeb0561eff8842189142dba5a90ad01c",
    ),
    (
        "15",
        "REL_15_STABLE",
        "7401a1ec1c8b4ce2f0e6cbfaab2fe82e0e667d42",
    ),
    (
        "13",
        "REL_13_STABLE",
        "05f6c9ec2c475fae8fdd56ae40bd01afcf70d1f2",
    ),
    (
        "10",
        "REL_10_STABLE",
        "f4e8f137bfb08a664c8288824c1e36b5143ac875",
    ),
    (
        "12",
        "REL_12_STABLE",
        "3f302f0ed06f69c5ebd33d4df95895323a3cbef6",
    ),
    (
        "11",
        "REL_11_STABLE",
        "170e416034ec0231d9e9238f2577eeb76ca8d181",
    ),
    (
        "9",
        "REL9_6_STABLE",
        "70cf253d18da103864037011bead1683a96502dc",
    ),
    (
        "8",
        "REL8_4_STABLE",
        "8dd387500abe58a684c559f20c26ac08884b0c5d",
    ),
    (
        "7",
        "REL7_4_STABLE",
        "fd7fdbc88be20f5fc8789254b18902116f88b5fe",
    ),
    (
        "6",
        "REL6_5_PATCHES",
        "21f0db98ea83025bb77928741af05f15ab4b0cba",
    ),
];

fn url(base: Result<String>) -> Result<String> {
    with_database(&base?, DATABASE)
}

/// Creates the database, loads the snapshots and builds the history, each
/// only when missing.
pub async fn prepare(ctx: &Ctx, git_dir: &Path) -> Result<()> {
    crate::prepare_database(ctx, DATABASE).await?;
    let admin_url = url(ctx.env.admin_database_url())?;
    let indexer_url = url(ctx.env.indexer_database_url())?;
    let mut indexer = db::connect(&indexer_url, "pghacker-replay").await?;
    let loaded: Vec<String> = indexer
        .query("SELECT sha FROM code_snapshots WHERE state = 'ready'", &[])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let missing: Vec<_> = HEADS
        .iter()
        .filter(|h| !loaded.iter().any(|s| s == h.2))
        .collect();
    if !missing.is_empty() {
        // Many lines load faster without the trigram index; it is rebuilt below.
        indexer
            .batch_execute("DROP INDEX IF EXISTS code_lines_trgm")
            .await?;
        for (version, git_ref, sha) in missing {
            let t = Instant::now();
            let stats = pghacker_code::load_snapshot(
                &mut indexer,
                &SnapshotSpec {
                    repo: "postgres".into(),
                    git_ref: (*git_ref).into(),
                    sha: (*sha).into(),
                    version: Some((*version).into()),
                    paths: Vec::new(),
                    source: RepoSource::Local {
                        path: git_dir.to_owned(),
                    },
                    source_id: None,
                },
            )
            .await?;
            println!(
                "loaded {git_ref} {}: {} files, {} lines in {:.0}s",
                &sha[..12],
                stats.files,
                stats.lines,
                t.elapsed().as_secs_f64()
            );
        }
    }
    let indexed = indexer
        .query_opt(
            "SELECT 1 FROM pg_indexes WHERE indexname = 'code_lines_trgm'",
            &[],
        )
        .await?
        .is_some();
    if !indexed {
        let t = Instant::now();
        indexer
            .batch_execute(
                "SET maintenance_work_mem = '1GB';
                 CREATE INDEX code_lines_trgm ON code_lines USING gin (line gin_trgm_ops)",
            )
            .await
            .context("cannot index the lines")?;
        indexer.batch_execute("VACUUM ANALYZE code_lines").await?;
        indexer.batch_execute("ANALYZE code_files").await?;
        println!("indexed the lines in {:.0}s", t.elapsed().as_secs_f64());
    }

    let admin = db::connect(&admin_url, "pghacker-replay").await?;
    let built = admin
        .query_opt(
            "SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = 'code_line_instances'",
            &[&SCHEMA],
        )
        .await?
        .is_some();
    if !built {
        let t = Instant::now();
        let stats = pghacker_code::history::build(
            &admin_url,
            &BuildOptions {
                git_dir: git_dir.to_owned(),
                master: "master".into(),
                url: None,
                schema: SCHEMA.into(),
                paths: Vec::new(),
                install: false,
            },
        )
        .await?;
        println!(
            "built the history: {} commits, {} line instances in {:.0}s",
            stats.stored.commits,
            stats.stored.instances,
            t.elapsed().as_secs_f64()
        );
    }
    println!("{DATABASE} is ready");
    Ok(())
}

/// What to run.
pub struct RunOptions {
    pub pg19: usize,
    pub seed: u64,
    pub jobs: usize,
    /// Only the first N heads, newest first.
    pub heads: Option<usize>,
    pub passes: usize,
    pub study: bool,
    /// The database to read, as pgh_reader.
    pub database: String,
    /// The history's schema.
    pub schema: String,
}

#[derive(Clone)]
enum Call {
    Grep(String),
    Search(Box<SearchArgs>),
}

struct Work {
    workload: &'static str,
    family: String,
    call: Call,
}

/// One call's result through one engine.
struct Outcome {
    out: Value,
    ms: f64,
    /// It answered: no error, complete.
    answered: bool,
    incomplete: bool,
}

#[derive(Serialize)]
struct Record {
    pass: usize,
    workload: &'static str,
    family: String,
    head: String,
    call: String,
    same: bool,
    snapshot_ms: f64,
    history_ms: f64,
    answered: bool,
    snapshot_incomplete: bool,
    history_incomplete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn profile_searches() -> Vec<(String, SearchArgs)> {
    let heap = || Strings::from("src/backend/access/heap");
    [
        (
            "heap_text",
            Strings::from("for ("),
            MatchMode::Text,
            heap(),
            GroupBy::File,
        ),
        (
            "heap_regex",
            Strings::from(r"\bfor\s*\("),
            MatchMode::Regex,
            heap(),
            GroupBy::File,
        ),
        (
            "heap_multi_counts",
            Strings::from(vec!["for (".into(), "while (".into()]),
            MatchMode::Text,
            heap(),
            GroupBy::Match,
        ),
        (
            "snapshot_text",
            Strings::from("for ("),
            MatchMode::Text,
            Strings::default(),
            GroupBy::File,
        ),
        (
            "snapshot_regex",
            Strings::from(r"^\s*for\s*\("),
            MatchMode::Regex,
            Strings::from(vec!["src".into(), "contrib".into()]),
            GroupBy::File,
        ),
        (
            "snapshot_match_counts",
            Strings::from("for ("),
            MatchMode::Text,
            Strings::from("src"),
            GroupBy::Match,
        ),
    ]
    .into_iter()
    .map(|(label, query, mode, paths, group_by)| {
        (
            label.to_owned(),
            SearchArgs {
                query,
                match_mode: mode,
                paths,
                output: OutputMode::Counts,
                group_by,
                budget_ms: Some(10_000),
                ..SearchArgs::default()
            },
        )
    })
    .collect()
}

/// The grep command lines of the newest study replay.
fn study_commands(ctx: &Ctx) -> Result<Vec<String>> {
    let mut runs: Vec<_> = std::fs::read_dir(&ctx.runtime)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("study-") && n.ends_with(".jsonl"))
        })
        .collect();
    runs.sort();
    let Some(newest) = runs.last() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in std::fs::read_to_string(newest)?.lines() {
        let v: Value = serde_json::from_str(line)?;
        if let Some(command) = v["grep"]["command"].as_str()
            && !command.is_empty()
        {
            out.push(command.to_owned());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

async fn call(client: &tokio_postgres::Client, snapshot: &Snapshot, call: &Call) -> Outcome {
    let t = Instant::now();
    match call {
        Call::Grep(command) => {
            let limits = Limits {
                budget_ms: 10_000,
                max_output: usize::MAX,
            };
            match grep_tool::run(client, snapshot, command, limits).await {
                Ok(o) => Outcome {
                    ms: t.elapsed().as_secs_f64() * 1e3,
                    answered: o.status != 2 && !o.incomplete,
                    incomplete: o.incomplete,
                    out: json!({
                        "stdout": String::from_utf8_lossy(&o.stdout),
                        "stderr": o.stderr,
                        "status": o.status,
                        "incomplete": o.incomplete,
                    }),
                },
                Err(e) => Outcome {
                    ms: t.elapsed().as_secs_f64() * 1e3,
                    answered: false,
                    incomplete: false,
                    out: json!({ "error": format!("{e:#}") }),
                },
            }
        }
        Call::Search(args) => match search(client, snapshot, args).await {
            Ok(r) => {
                let ms = t.elapsed().as_secs_f64() * 1e3;
                let (complete, exceeded) = (r.complete, r.budget_exceeded);
                let mut out = json!(r);
                if let Some(o) = out.as_object_mut() {
                    o.remove("elapsed_ms");
                }
                Outcome {
                    ms,
                    answered: complete && !exceeded,
                    incomplete: !complete || exceeded,
                    out,
                }
            }
            Err(e) => Outcome {
                ms: t.elapsed().as_secs_f64() * 1e3,
                answered: false,
                incomplete: false,
                out: json!({ "error": format!("{e:#}") }),
            },
        },
    }
}

/// Where two outputs first differ, briefly.
fn difference(a: &Value, b: &Value) -> String {
    let (a, b) = (a.to_string(), b.to_string());
    let at = a
        .bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let from = at.saturating_sub(80);
    let snip = |s: &str| {
        s.get(from..(at + 120).min(s.len()))
            .unwrap_or("")
            .to_owned()
    };
    format!(
        "snapshot {} bytes, history {} bytes, first difference at {at}: {:?} vs {:?}",
        a.len(),
        b.len(),
        snip(&a),
        snip(&b)
    )
}

fn stats(ms: &mut [f64]) -> Value {
    ms.sort_by(f64::total_cmp);
    let mean = if ms.is_empty() {
        0.0
    } else {
        ms.iter().sum::<f64>() / ms.len() as f64
    };
    json!({
        "n": ms.len(),
        "p50": (percentile(ms, 0.50) * 10.0).round() / 10.0,
        "p95": (percentile(ms, 0.95) * 10.0).round() / 10.0,
        "p99": (percentile(ms, 0.99) * 10.0).round() / 10.0,
        "mean": (mean * 10.0).round() / 10.0,
    })
}

fn summarize(records: &[Record]) -> Value {
    let group = |key: &dyn Fn(&Record) -> String| -> BTreeMap<String, Value> {
        let mut groups: BTreeMap<String, Vec<&Record>> = BTreeMap::new();
        for r in records {
            groups.entry(key(r)).or_default().push(r);
        }
        groups
            .into_iter()
            .map(|(k, rs)| {
                let same = rs.iter().filter(|r| r.same).count();
                let mut a: Vec<f64> = rs
                    .iter()
                    .filter(|r| r.answered)
                    .map(|r| r.snapshot_ms)
                    .collect();
                let mut b: Vec<f64> = rs
                    .iter()
                    .filter(|r| r.answered)
                    .map(|r| r.history_ms)
                    .collect();
                let (sa, sb) = (stats(&mut a), stats(&mut b));
                let ratio = |q: &str| {
                    let (x, y) = (sa[q].as_f64().unwrap_or(0.0), sb[q].as_f64().unwrap_or(0.0));
                    if x > 0.0 {
                        ((y / x) * 100.0).round() / 100.0
                    } else {
                        0.0
                    }
                };
                let v = json!({
                    "calls": rs.len(),
                    "same": same,
                    "snapshot_ms": sa,
                    "history_ms": sb,
                    "ratio_p50": ratio("p50"),
                    "ratio_p95": ratio("p95"),
                    "snapshot_incomplete": rs.iter().filter(|r| r.snapshot_incomplete).count(),
                    "history_incomplete": rs.iter().filter(|r| r.history_incomplete).count(),
                });
                (k, v)
            })
            .collect()
    };
    json!({
        "all": group(&|_| "all".into())["all"].clone(),
        "by_workload": group(&|r| r.workload.to_owned()),
        "by_head": group(&|r| r.head.clone()),
    })
}

/// Runs every workload at every head through both engines.
pub async fn run(ctx: &Ctx, opts: &RunOptions) -> Result<()> {
    let reader_url = with_database(&ctx.env.reader_database_url()?, &opts.database)?;
    let reader = db::connect(&reader_url, "pghacker-replay").await?;
    let rows = reader
        .query(
            "SELECT id, repo, version, ref, sha FROM code_snapshots \
             WHERE state = 'ready' AND repo = 'postgres' ORDER BY committed_at DESC",
            &[],
        )
        .await?;
    let mut heads = Vec::new();
    for row in rows {
        let snap = Snapshot {
            id: row.get(0),
            repo: row.get(1),
            version: row.get(2),
            git_ref: row.get(3),
            sha: row.get(4),
            history: None,
        };
        let hist =
            pghacker_codesearch::history::attach(&reader, snap.clone(), &opts.schema).await?;
        heads.push((snap, hist));
    }
    if let Some(n) = opts.heads {
        heads.truncate(n);
    }

    let mut works = Vec::new();
    crate::pg19::ensure_tree(ctx).await?;
    for (family, command, args) in crate::pg19::workload(ctx, opts.pg19, opts.seed)? {
        works.push(Work {
            workload: "pg19-grep",
            family: family.clone(),
            call: Call::Grep(command),
        });
        if let Some(args) = args {
            works.push(Work {
                workload: "pg19-search",
                family,
                call: Call::Search(Box::new(args)),
            });
        }
    }
    for (label, args) in profile_searches() {
        works.push(Work {
            workload: "profile",
            family: label,
            call: Call::Search(Box::new(args)),
        });
    }
    works.push(Work {
        workload: "profile",
        family: "heap_grep_wc".into(),
        call: Call::Grep("grep -rF 'for (' src/backend/access/heap | wc -l".into()),
    });
    if opts.study {
        for command in study_commands(ctx)? {
            works.push(Work {
                workload: "study-grep",
                family: "study".into(),
                call: Call::Grep(command),
            });
        }
    }
    println!(
        "{} calls at {} heads through both engines, {} at once",
        works.len(),
        heads.len(),
        opts.jobs
    );

    let works = Arc::new(works);
    let heads = Arc::new(heads);
    let pool = db::pool(&reader_url, opts.jobs, "pghacker-replay")?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let log_path = ctx.runtime.join(format!("engines-{stamp}.jsonl"));
    let mut log = String::new();
    let mut summaries = Vec::new();
    for pass in 0..opts.passes.max(1) {
        let started = Instant::now();
        // Head by head, as production mostly reads one version at a time:
        // each engine caches a version's file table.
        let pairs: Vec<(usize, usize)> = (0..heads.len())
            .flat_map(|h| (0..works.len()).map(move |w| (w, h)))
            .collect();
        let records: Vec<Record> =
            stream::iter(pairs.into_iter().enumerate().map(|(i, (w, h))| {
                let (pool, works, heads) = (pool.clone(), works.clone(), heads.clone());
                async move {
                    let client = pool.get().await?;
                    let work = &works[w];
                    let (snap, hist) = &heads[h];
                    // Alternate which engine goes first, so neither always
                    // finds the other's pages in the cache.
                    let (a, b) = if i % 2 == 0 {
                        let a = call(&client, snap, &work.call).await;
                        (a, call(&client, hist, &work.call).await)
                    } else {
                        let b = call(&client, hist, &work.call).await;
                        (call(&client, snap, &work.call).await, b)
                    };
                    let same = a.out == b.out;
                    anyhow::Ok(Record {
                        pass,
                        workload: work.workload,
                        family: work.family.clone(),
                        head: snap.git_ref.clone(),
                        call: match &work.call {
                            Call::Grep(c) => c.clone(),
                            Call::Search(args) => format!("{args:?}"),
                        },
                        same,
                        snapshot_ms: a.ms,
                        history_ms: b.ms,
                        answered: a.answered && b.answered,
                        snapshot_incomplete: a.incomplete,
                        history_incomplete: b.incomplete,
                        detail: (!same).then(|| difference(&a.out, &b.out)),
                    })
                }
            }))
            .buffer_unordered(opts.jobs.max(1))
            .collect::<Vec<Result<Record>>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        for r in &records {
            log.push_str(&serde_json::to_string(r)?);
            log.push('\n');
        }
        std::fs::write(&log_path, &log)?;
        let mut summary = summarize(&records);
        summary["pass"] = json!(pass);
        summary["seconds"] = json!(started.elapsed().as_secs());
        let different: Vec<&Record> = records.iter().filter(|r| !r.same).take(5).collect();
        summary["first_differences"] = json!(
            different
                .iter()
                .map(|r| json!({"head": r.head, "call": r.call, "detail": r.detail}))
                .collect::<Vec<_>>()
        );
        println!("{}", serde_json::to_string_pretty(&summary)?);
        summaries.push(summary);
        std::fs::write(
            ctx.runtime.join("engines-summary.json"),
            serde_json::to_string_pretty(&summaries)?,
        )?;
    }
    println!("records: {}", log_path.display());
    Ok(())
}
