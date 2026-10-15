//! The search operation against a synthetic commit history in a test
//! database.

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

async fn resolved(reader: &tokio_postgres::Client, version: Option<&str>) -> Result<String> {
    Ok(resolve_snapshot(reader, "postgres", version).await?.git_ref)
}

#[tokio::test]
async fn versions_resolve_to_their_branch_heads() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let d = dir.path();
    let tick = std::cell::Cell::new(0);
    let commit = |message: &str| {
        tick.set(tick.get() + 1);
        std::fs::write(d.join("src.c"), format!("{message} {}\n", tick.get())).unwrap();
        common::git(d, tick.get(), &["add", "-A"]);
        common::git(d, tick.get(), &["commit", "-q", "-m", message]);
        common::git(d, tick.get(), &["rev-parse", "HEAD"])
    };
    common::git(d, 0, &["init", "-q", "-b", "master"]);
    // Release lines fork from master, with their releases tagged on them.
    let mut shas = std::collections::HashMap::new();
    for (line, releases) in [
        ("REL9_0_STABLE", vec!["REL9_0_23"]),
        ("REL9_6_STABLE", vec!["REL9_6_24", ""]),
        ("REL_18_STABLE", vec!["REL_18_3", "REL_18_6", ""]),
        ("REL_19_STABLE", vec!["REL_19_0", ""]),
    ] {
        commit("master");
        common::git(d, tick.get(), &["checkout", "-q", "-b", line]);
        for release in releases {
            let sha = commit(line);
            if !release.is_empty() {
                common::git(d, tick.get(), &["tag", release, &sha]);
                shas.insert(release, sha);
            }
        }
        common::git(d, tick.get(), &["checkout", "-q", "master"]);
    }
    commit("master");
    let db = TestDb::create().await?;
    common::install(&db, d, &[]).await?;
    let reader = db.reader().await?;

    // A major version is its branch head.
    assert_eq!(resolved(&reader, Some("18")).await?, "REL_18_STABLE");
    assert_eq!(resolved(&reader, Some("PG18")).await?, "REL_18_STABLE");
    assert_eq!(resolved(&reader, Some("9")).await?, "REL9_6_STABLE");
    // No version: the newest numbered release line, not master.
    assert_eq!(resolved(&reader, None).await?, "REL_19_STABLE");
    // Every other commit by release, ref or SHA.
    assert_eq!(resolved(&reader, Some("18.3")).await?, "REL_18_3");
    assert_eq!(resolved(&reader, Some("REL_18_6")).await?, "REL_18_6");
    assert_eq!(resolved(&reader, Some("rel_19_0")).await?, "REL_19_0");
    // A SHA names its commit on its branch.
    let by_sha = resolve_snapshot(&reader, "postgres", Some(&shas["REL_18_6"][..7])).await?;
    assert_eq!(
        (by_sha.git_ref.as_str(), by_sha.sha.as_str()),
        ("REL_18_STABLE", shas["REL_18_6"].as_str())
    );
    assert_eq!(resolved(&reader, Some("master")).await?, "master");
    assert_eq!(resolved(&reader, Some("9.6")).await?, "REL9_6_STABLE");
    assert_eq!(resolved(&reader, Some("9.6.24")).await?, "REL9_6_24");
    assert_eq!(resolved(&reader, Some("9.0")).await?, "REL9_0_STABLE");

    let error = resolved(&reader, Some("18.5"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("REL9_0_STABLE, REL9_6_STABLE, REL_18_STABLE, REL_19_STABLE, master"),
        "{error}"
    );
    Ok(())
}

#[tokio::test]
async fn text_and_regex_searches() -> Result<()> {
    let db = TestDb::create().await?;
    common::load(&db, "postgres", "19", &common::files()).await?;
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
    let snapshot = common::load(&db, "postgres", "19", &common::files()).await?;
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
    let snapshot = common::load(&db, "postgres", "19", &common::files()).await?;
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
    // Enough lines that an index is worth it.
    let filler: String = (1..=60_000)
        .map(|g| format!("filler line {g} of synthetic text\n"))
        .collect();
    let mut files: Vec<(&str, Option<&str>)> = common::files();
    files.push(("src/backend/filler.c", Some(&filler)));
    let snapshot = common::load(&db, "postgres", "19", &files).await?;
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
    let h = &snapshot.history;
    let mut lineages: Vec<i32> = pghacker_codesearch::history::lineage_map(&reader, h)
        .await?
        .keys()
        .copied()
        .collect();
    lineages.sort_unstable();
    let q = pghacker_codesearch::engine::LineQuery {
        history: h,
        mounted: false,
        file_ids: None,
        require: vec![&compiled.filter],
        reject: vec![],
        cap: 1000,
        scope_first: false,
    };
    let (sql, params) = pghacker_codesearch::history::line_sql(&q, h, &lineages)?;
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        params.iter().map(|p| p.as_ref() as _).collect();
    let plan: serde_json::Value = reader
        .query_one(format!("EXPLAIN (FORMAT JSON) {sql}").as_str(), &refs)
        .await?
        .get(0);
    let plan = plan.to_string();
    assert!(plan.contains("code_line_instances_trgm"), "{plan}");
    let result = search(&reader, &snapshot, &SearchArgs::new("json_lex_string")).await?;
    assert_eq!(result.total_lines, 1);
    Ok(())
}

#[tokio::test]
async fn scoped_loop_searches_avoid_the_global_trigram_index() -> Result<()> {
    use pghacker_codesearch::engine::{Budget, LineQuery, candidates};
    use pghacker_codesearch::history::line_sql;
    use pghacker_codesearch::pattern::{Dialect, PatternSpec, Semantics, compile};

    let db = TestDb::create().await?;
    // Another version's loops fill the trigram index.
    let other = "for (int i = 0; i < 10; i++) {}\n".repeat(50_000);
    let snapshot = common::load_versions(
        &db,
        "postgres",
        &[
            (
                "19",
                &[(
                    "src/backend/access/heap/loops.c",
                    Some(
                        "for (a) {}\n\tfor\t(b) {}\nwhile (c) {}\n// for (d) for (e)\nformat();\n",
                    ),
                )][..],
            ),
            (
                "18",
                &[("src/backend/access/heap/other.c", Some(other.as_str()))][..],
            ),
        ],
    )
    .await?;
    let reader = db.reader().await?;
    let compiled = compile(&PatternSpec {
        patterns: &[r"\bfor\s*\(".into()],
        dialect: Dialect::Rust,
        ignore_case: false,
        word: false,
        line: false,
        semantics: Semantics::LeftmostFirst,
    })
    .map_err(anyhow::Error::msg)?;
    let q = LineQuery {
        history: &snapshot.history,
        mounted: false,
        file_ids: None,
        require: vec![&compiled.filter],
        reject: vec![],
        cap: 100,
        scope_first: true,
    };
    let (sql, params) = line_sql(&q, &snapshot.history, &[])?;
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        params.iter().map(|p| p.as_ref() as _).collect();
    let plan: serde_json::Value = reader
        .query_one(format!("EXPLAIN (FORMAT JSON) {sql}").as_str(), &refs)
        .await?
        .get(0);
    let plan = plan.to_string();
    assert!(!plan.contains("code_line_instances_trgm"), "{plan}");
    let rows = candidates(&reader, &q, &Budget::new(None)).await?;
    assert_eq!(rows.rows.len(), 3);
    assert!(!rows.capped && !rows.timed_out);

    let mut args = SearchArgs::new("for (");
    args.paths = "src/backend/access/heap".into();
    args.output = OutputMode::Counts;
    args.group_by = GroupBy::Match;
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!((result.total_lines, result.total_matches), (2, Some(3)));
    assert_eq!(
        (result.matches[0].text.as_str(), result.matches[0].count),
        ("for (", 3)
    );
    assert!(result.complete && !result.budget_exceeded);
    args.query = vec!["for (".into(), "while (".into()].into();
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!((result.total_lines, result.total_matches), (3, Some(4)));
    assert!(result.complete && !result.budget_exceeded);
    args.query = r"^\s*for\s*\(".into();
    args.match_mode = MatchMode::Regex;
    let result = search(&reader, &snapshot, &args).await?;
    assert_eq!((result.total_lines, result.total_matches), (2, Some(2)));
    assert!(result.complete);
    Ok(())
}

#[tokio::test]
async fn commit_and_history_searches_use_the_shared_budget() -> Result<()> {
    let db = TestDb::create().await?;
    let snapshot = common::load(
        &db,
        "postgres",
        "19",
        &[("src/probe.c", Some("budget_probe\n"))],
    )
    .await?;
    let admin = db.admin().await?;
    // Return the candidate query's actual statement timeout as matching text.
    admin
        .batch_execute(&format!(
            "ALTER TABLE {s}.code_line_instances RENAME TO budget_probe_lines;
             CREATE VIEW {s}.code_line_instances AS
             SELECT weave, lo, hi, id, lineage, bytes, alive, replaces,
                    (extract(epoch FROM current_setting('statement_timeout')::interval) * 1000)::bigint::text
                        || ' budget_probe' AS line
             FROM {s}.budget_probe_lines;
             GRANT SELECT ON {s}.code_line_instances TO pgh_reader;",
            s = snapshot.history.schema,
        ))
        .await?;
    let reader = db.reader().await?;
    reader
        .batch_execute("SET statement_timeout = '1739ms'")
        .await?;
    for history in [None, Some("all")] {
        for (budget_ms, maximum) in [
            (None, 10_000u64),
            (Some(12_000), 12_000),
            (Some(u32::MAX), 15_000),
        ] {
            let args = SearchArgs {
                query: "budget_probe".into(),
                budget_ms,
                history: history.map(str::to_owned),
                ..SearchArgs::default()
            };
            let result = search(&reader, &snapshot, &args).await?;
            assert_eq!(result.total_lines, 1);
            assert!(result.complete && !result.budget_exceeded);
            let text = if history.is_some() {
                &result.history[0].text
            } else {
                &result.lines[0].text
            };
            let remaining_ms: u64 = text.split_whitespace().next().unwrap().parse()?;
            assert!(
                remaining_ms > maximum - 2_000 && remaining_ms <= maximum,
                "history={history:?}, budget_ms={budget_ms:?}: {remaining_ms}ms remaining, expected near {maximum}ms"
            );
            let restored: String = reader
                .query_one("SHOW statement_timeout", &[])
                .await?
                .get(0);
            assert_eq!(restored, "1739ms");
        }
    }
    Ok(())
}

#[tokio::test]
async fn timeouts_restore_settings_and_do_not_invert_unknown_results() -> Result<()> {
    use pghacker_codesearch::grep::{Limits, run};
    use pghacker_codesearch::snapshot::FileTable;

    let db = TestDb::create().await?;
    let snapshot = common::load(&db, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;
    FileTable::load(&reader, &snapshot).await?;
    reader
        .batch_execute("SET statement_timeout = '1739ms'")
        .await?;
    let blocker = db.admin().await?;
    blocker
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {}.code_line_instances IN ACCESS EXCLUSIVE MODE",
            snapshot.history.schema
        ))
        .await?;
    let budget = pghacker_codesearch::engine::Budget::new(Some(500));
    let old = pghacker_codesearch::engine::set_timeout(&reader, budget.remaining_ms()).await?;
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let q = pghacker_codesearch::engine::LineQuery {
        history: &snapshot.history,
        mounted: false,
        file_ids: None,
        require: vec![],
        reject: vec![],
        cap: 1,
        scope_first: true,
    };
    let candidates = pghacker_codesearch::engine::candidates(&reader, &q, &budget).await?;
    assert!(candidates.timed_out);
    assert!(budget.elapsed_ms() < 750, "{}ms", budget.elapsed_ms());
    pghacker_codesearch::engine::restore_timeout(&reader, &old).await;
    let mut args = SearchArgs::new("makeNode");
    args.budget_ms = Some(80);
    let result = search(&reader, &snapshot, &args).await?;
    assert!(result.budget_exceeded && !result.complete);
    assert_eq!(result.total_lines, 0);
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.contains("lower bounds"))
    );
    args.query = "".into();
    args.not = "makeNode".into();
    let error = search(&reader, &snapshot, &args).await.unwrap_err();
    assert!(error.to_string().contains("inverted results"), "{error:#}");
    let output = run(
        &reader,
        &snapshot,
        "grep -r makeNode src 2>/dev/null | wc -l || echo none",
        Limits {
            budget_ms: 80,
            ..Limits::default()
        },
    )
    .await?;
    assert!(output.incomplete);
    assert_eq!(output.status, 2);
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    let error = pghacker_codesearch::grep(
        &reader,
        &snapshot,
        &pghacker_codesearch::GrepArgs {
            command: "grep -r makeNode src | wc -l".into(),
            budget_ms: Some(80),
            version: None,
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("search stopped early"),
        "{error:#}"
    );
    let timeout: String = reader
        .query_one("SHOW statement_timeout", &[])
        .await?
        .get(0);
    assert_eq!(timeout, "1739ms");
    let one: i32 = reader.query_one("SELECT 1", &[]).await?.get(0);
    assert_eq!(one, 1);
    blocker.batch_execute("ROLLBACK").await?;
    Ok(())
}
