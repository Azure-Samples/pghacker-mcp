//! Profiles common loop searches with many unrelated snapshots sharing the
//! trigram index. Uses a temporary TestDb, never the application's corpus.

use anyhow::Result;
use pghacker_codesearch::grep::{self, Limits};
use pghacker_codesearch::search::{GroupBy, MatchMode, OutputMode, Strings};
use pghacker_codesearch::{SearchArgs, resolve_snapshot, search};
use pghacker_core::testing::TestDb;
use serde_json::json;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    indexer
        .batch_execute(
            "INSERT INTO code_snapshots (repo, ref, sha, version, state, file_count, line_count, loaded_at)
             SELECT 'postgres', 'REL_' || v || '_STABLE', md5(v::text), v::text, 'ready', 40, 300000, now()
             FROM generate_series(19, 11, -1) v;
             INSERT INTO code_files (snapshot_id, path, blob_sha, size, line_count)
             SELECT s.id,
                    CASE WHEN f <= 2 THEN 'src/backend/access/heap/'
                         WHEN f <= 20 THEN 'src/backend/other/' ELSE 'contrib/example/' END
                         || f || '.c',
                    md5(f::text), 300000, 7500
             FROM code_snapshots s CROSS JOIN generate_series(1, 40) f;
             INSERT INTO code_lines (file_id, line_no, line)
             SELECT f.id, n,
                    CASE WHEN s.version <> '19' OR n % 19 = 0 THEN '    for (int i = 0; i < 10; i++) {}'
                         WHEN n % 37 = 0 THEN '    while (ready) {}'
                         ELSE '    benchmark_padding_' || n || '();' END
             FROM code_files f JOIN code_snapshots s ON s.id = f.snapshot_id
             CROSS JOIN generate_series(1, 7500) n;",
        )
        .await?;
    indexer.batch_execute("VACUUM ANALYZE code_lines").await?;
    indexer.batch_execute("ANALYZE code_files").await?;
    let reader = db.reader().await?;
    let snapshot = resolve_snapshot(&reader, "postgres", Some("19")).await?;
    let mut cases = Vec::new();
    let compiled =
        pghacker_codesearch::pattern::compile(&pghacker_codesearch::pattern::PatternSpec {
            patterns: &["for (".into()],
            dialect: pghacker_codesearch::pattern::Dialect::Fixed,
            ignore_case: false,
            word: false,
            line: false,
            semantics: pghacker_codesearch::pattern::Semantics::LeftmostFirst,
        })
        .map_err(anyhow::Error::msg)?;
    let q = pghacker_codesearch::engine::LineQuery {
        snapshot_id: snapshot.id,
        history: None,
        mounted: false,
        file_ids: None,
        require: vec![&compiled.filter],
        reject: vec![],
        cap: pghacker_codesearch::engine::MAX_CANDIDATES,
        scope_first: true,
    };
    let (sql, params) = pghacker_codesearch::engine::line_sql(&q);
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        params.iter().map(|p| p.as_ref() as _).collect();
    let plan: serde_json::Value = reader
        .query_one(
            format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}").as_str(),
            &refs,
        )
        .await?
        .get(0);
    cases.push(json!({
        "case": "scoped_plan", "jit_ms": plan[0]["JIT"]["Timing"]["Total"],
        "execution_ms": plan[0]["Execution Time"], "cost": plan[0]["Plan"]["Total Cost"]
    }));
    for (label, query, mode, paths, group_by) in [
        (
            "heap_text",
            Strings::from("for ("),
            MatchMode::Text,
            Strings::from("src/backend/access/heap"),
            GroupBy::File,
        ),
        (
            "heap_regex",
            Strings::from(r"\bfor\s*\("),
            MatchMode::Regex,
            Strings::from("src/backend/access/heap"),
            GroupBy::File,
        ),
        (
            "heap_multi_counts",
            Strings::from(vec!["for (".into(), "while (".into()]),
            MatchMode::Text,
            Strings::from("src/backend/access/heap"),
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
    ] {
        let args = SearchArgs {
            query,
            match_mode: mode,
            paths,
            output: OutputMode::Counts,
            group_by,
            budget_ms: Some(10_000),
            ..SearchArgs::default()
        };
        let mut times = Vec::new();
        let mut result = search(&reader, &snapshot, &args).await?;
        for _ in 0..3 {
            result = search(&reader, &snapshot, &args).await?;
            times.push(result.elapsed_ms);
        }
        times.sort_unstable();
        cases.push(json!({
            "case": label, "median_ms": times[1], "total_lines": result.total_lines,
            "total_matches": result.total_matches, "complete": result.complete,
            "budget_exceeded": result.budget_exceeded
        }));
    }
    let started = Instant::now();
    let output = grep::run(
        &reader,
        &snapshot,
        "grep -rF 'for (' src/backend/access/heap | wc -l",
        Limits {
            budget_ms: 10_000,
            ..Limits::default()
        },
    )
    .await?;
    cases.push(json!({
        "case": "heap_grep_wc", "elapsed_ms": started.elapsed().as_millis(),
        "stdout": String::from_utf8(output.stdout)?, "status": output.status,
        "incomplete": output.incomplete
    }));
    println!("{}", serde_json::to_string_pretty(&cases)?);
    Ok(())
}
