//! The search operation against a synthetic snapshot in a test database.

mod common;

use anyhow::Result;
use pghacker_codesearch::search::{GroupBy, MatchMode, Order, OutputMode, Strings};
use pghacker_codesearch::{SearchArgs, resolve_snapshot, search};
use pghacker_core::testing::TestDb;

fn hits(result: &pghacker_codesearch::SearchResult) -> Vec<(String, u32)> {
    result
        .lines
        .iter()
        .map(|h| (h.path.clone(), h.line))
        .collect()
}

fn at(path: &str, line: u32) -> (String, u32) {
    (path.to_owned(), line)
}

#[tokio::test]
async fn text_and_regex_searches() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    common::load(&indexer, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;
    let snapshot = resolve_snapshot(&reader, "postgres", Some("PG19")).await?;
    assert_eq!(snapshot.version.as_deref(), Some("19"));

    let result = search(&reader, &snapshot, &SearchArgs::new("makeNode(")).await?;
    assert_eq!(
        hits(&result),
        vec![
            at("src/backend/parser/gram.y", 8),
            at("src/backend/parser/gram.y", 15),
            at("src/include/nodes/nodes.h", 11),
        ]
    );
    assert_eq!((result.total_lines, result.total_files), (3, 2));
    assert!(result.complete && !result.cut && !result.budget_exceeded);

    // Case, whole words and a folder scope.
    let mut args = SearchArgs::new("JSON");
    args.case_sensitive = Some(false);
    args.whole_word = true;
    args.paths = Strings::One("./src/backend/utils/".into());
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(
        hits(&result),
        vec![
            at("src/backend/utils/adt/json.c", 1),
            at("src/backend/utils/adt/json.c", 9),
            at("src/backend/utils/adt/json.c", 10),
        ]
    );

    // A regex with alternation, as ripgrep reads it.
    let mut args = SearchArgs::new(r"^(Datum|float4)$|\bT_\w+Stmt\b");
    args.match_mode = MatchMode::Regex;
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(
        hits(&result),
        vec![
            at("contrib/pg_trgm/trgm_op.c", 2),
            at("src/backend/utils/adt/json.c", 6),
            at("src/backend/utils/adt/json.c", 13),
            at("src/include/nodes/nodes.h", 8),
        ]
    );

    // A lookahead falls back to fancy-regex and still uses the index.
    let mut args = SearchArgs::new(r"json_\w+(?=\(PG)");
    args.match_mode = MatchMode::Regex;
    args.output = OutputMode::Lines;
    args.group_by = GroupBy::Match;
    let result = search(&reader, &snapshot, &args).await?;
    let parts: Vec<&str> = result.lines.iter().map(|h| h.text.as_str()).collect();
    assert_eq!(parts, vec!["json_in", "json_out", "json_to_cstring"]);
    Ok(())
}

#[tokio::test]
async fn scope_filters_and_paging() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;

    let mut args = SearchArgs::new("T_");
    args.include = Strings::One("*.h".into());
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(result.total_lines, 5);
    assert!(result.lines.iter().all(|h| h.path.ends_with(".h")));

    let mut args = SearchArgs::new("make");
    args.exclude = Strings::Many(vec!["parser".into(), "*.h".into()]);
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(
        hits(&result),
        vec![
            at("contrib/pg_trgm/trgm_op.c", 8),
            at("src/backend/utils/adt/json.c", 16),
        ]
    );

    let mut args = SearchArgs::new("make");
    args.paths = Strings::Many(vec![
        "/home/me/src/postgres/src/include/nodes/nodes.h".into(),
        "contrib/**/*.c".into(),
    ]);
    args.limit = Some(1);
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(hits(&result), vec![at("contrib/pg_trgm/trgm_op.c", 8)]);
    assert!(result.cut);
    assert_eq!(result.total_lines, 2);
    args.offset = 1;
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(hits(&result), vec![at("src/include/nodes/nodes.h", 11)]);
    assert!(!result.cut);

    let mut args = SearchArgs::new("x");
    args.paths = Strings::One("src/nowhere".into());
    let error = search(&reader, &snapshot, &args).await.unwrap_err();
    assert!(error.to_string().contains("no such file or folder"));

    // Context, and filters on the line (| grep, | grep -v).
    let mut args = SearchArgs::new("makeNode");
    args.paths = Strings::One("src/backend".into());
    args.context = 1;
    args.not = Strings::One("Select".into());
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(hits(&result), vec![at("src/backend/parser/gram.y", 15)]);
    assert_eq!(result.lines[0].before, vec!["    {"]);
    assert_eq!(result.lines[0].after, vec!["        $$ = (Node *) n;"]);

    // grep -v: every line in scope that matches none of the patterns.
    let mut args = SearchArgs::new("");
    args.paths = Strings::One("src/include/nodes/nodes.h".into());
    args.not = Strings::Many(vec!["T_".into(), "#".into()]);
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!(result.total_lines, 6);
    Ok(())
}

#[tokio::test]
async fn identifiers_counts_coverage_and_similar_names() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;

    let mut args = SearchArgs::new("make json value");
    args.match_mode = MatchMode::Identifier;
    args.output = OutputMode::Counts;
    args.group_by = GroupBy::Match;
    let result = search(&reader, &snapshot, &args).await?;
    let counts: Vec<(&str, u64)> = result
        .matches
        .iter()
        .map(|m| (m.text.as_str(), m.count))
        .collect();
    assert_eq!(
        counts,
        vec![
            ("make_json_value", 2),
            ("MAKE_JSON_VALUE", 1),
            ("MakeJsonValue", 1)
        ]
    );
    assert_eq!(result.total_matches, Some(4));

    let mut args = SearchArgs::new(Strings::Many(vec![
        "makeNode".into(),
        "JsonTable".into(),
        "SelectStmt".into(),
    ]));
    args.output = OutputMode::Files;
    args.order = Order::Coverage;
    let result = search(&reader, &snapshot, &args).await?;
    let files: Vec<(&str, Option<u32>)> = result
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.terms))
        .collect();
    assert_eq!(
        files,
        vec![
            ("src/backend/parser/gram.y", Some(3)),
            ("src/include/nodes/nodes.h", Some(3)),
        ]
    );

    let result = search(&reader, &snapshot, &SearchArgs::new("make_json_values")).await?;
    assert_eq!(result.total_lines, 0);
    let similar = result.similar.expect("similar names");
    assert!(
        similar.names.iter().any(|n| n.name == "make_json_value"),
        "{similar:?}"
    );
    Ok(())
}

#[tokio::test]
async fn literal_searches_use_the_trigram_index() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &common::files()).await?;
    // Enough lines that an index is worth it.
    indexer
        .batch_execute(&format!(
            "INSERT INTO code_files (snapshot_id, path, blob_sha, size, line_count) \
             VALUES ({}, 'src/backend/filler.c', 'f', 1, 60000);
             INSERT INTO code_lines (file_id, line_no, line)
             SELECT currval('code_files_id_seq'), g, 'filler line ' || g || ' of synthetic text'
             FROM generate_series(1, 60000) g;",
            snapshot.id
        ))
        .await?;
    // As a loader should: move new GIN entries out of the pending list.
    indexer.batch_execute("VACUUM ANALYZE code_lines").await?;
    indexer.batch_execute("ANALYZE code_files").await?;
    let reader = db.reader().await?;
    let compiled =
        pghacker_codesearch::pattern::compile(&pghacker_codesearch::pattern::PatternSpec {
            patterns: &["json_lex_string".to_owned()],
            dialect: pghacker_codesearch::pattern::Dialect::Fixed,
            ignore_case: false,
            word: false,
            line: false,
            semantics: pghacker_codesearch::pattern::Semantics::LeftmostFirst,
        })
        .map_err(anyhow::Error::msg)?;
    let q = pghacker_codesearch::engine::LineQuery {
        snapshot_id: snapshot.id,
        file_ids: None,
        require: vec![&compiled.filter],
        reject: vec![],
        cap: 1000,
    };
    let (sql, params) = pghacker_codesearch::engine::line_sql(&q);
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        params.iter().map(|p| p.as_ref() as _).collect();
    let plan: serde_json::Value = reader
        .query_one(format!("EXPLAIN (FORMAT JSON) {sql}").as_str(), &refs)
        .await?
        .get(0);
    let plan = plan.to_string();
    assert!(plan.contains("code_lines_trgm"), "{plan}");
    let result = search(&reader, &snapshot, &SearchArgs::new("json_lex_string")).await?;
    assert_eq!(result.total_lines, 1);
    Ok(())
}
