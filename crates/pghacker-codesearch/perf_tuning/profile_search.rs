//! Profiles common loop searches with many unrelated branches' lines
//! sharing the history's trigram index. Builds a synthetic repository into
//! a temporary TestDb, never the application's corpus.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Result, ensure};
use pghacker_code::history::{BuildOptions, build};
use pghacker_codesearch::grep::{self, Limits};
use pghacker_codesearch::search::{GroupBy, MatchMode, OutputMode, Strings};
use pghacker_codesearch::{SearchArgs, search};
use pghacker_core::testing::TestDb;
use serde_json::json;

const SCHEMA: &str = "pghacker_history";

/// Runs git in `dir` as a fixed author, `tick` minutes into 2026.
fn git(dir: &Path, tick: u32, args: &[&str]) -> Result<()> {
    let date = format!("{} +0000", 1_767_225_600 + tick * 60);
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Profile")
        .env("GIT_AUTHOR_EMAIL", "profile@example.org")
        .env("GIT_COMMITTER_NAME", "Profile")
        .env("GIT_COMMITTER_EMAIL", "profile@example.org")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()?;
    ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Writes a tree of 40 files of 7,500 lines each, `line(n)` for line n.
fn write_tree(dir: &Path, line: impl Fn(u32) -> String) -> Result<()> {
    for f in 1..=40 {
        let folder = if f <= 2 {
            "src/backend/access/heap"
        } else if f <= 20 {
            "src/backend/other"
        } else {
            "contrib/example"
        };
        let path = dir.join(folder).join(format!("{f}.c"));
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text: String = (1..=7_500).map(|n| line(n) + "\n").collect();
        std::fs::write(path, text)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Master is 19, one past the newest release line: a loop every 19th
    // line and a while every 37th. REL_18_STABLE to REL_11_STABLE fork from
    // it and turn every line into a loop of their own, which the trigram
    // index shares but master's commit doesn't have.
    let dir = tempfile::tempdir()?;
    let d = dir.path();
    git(d, 0, &["init", "-q", "-b", "master"])?;
    write_tree(d, |n| {
        if n % 19 == 0 {
            "    for (int i = 0; i < 10; i++) {}".into()
        } else if n % 37 == 0 {
            "    while (ready) {}".into()
        } else {
            format!("    benchmark_padding_{n}();")
        }
    })?;
    git(d, 1, &["add", "-A"])?;
    git(d, 1, &["commit", "-q", "-m", "19"])?;
    for v in (11..=18).rev() {
        let tick = 20 - v;
        git(
            d,
            tick,
            &["checkout", "-q", "-b", &format!("REL_{v}_STABLE"), "master"],
        )?;
        write_tree(d, |_| format!("    for (int j = 0; j < {v}; j++) {{}}"))?;
        git(d, tick, &["add", "-A"])?;
        git(d, tick, &["commit", "-q", "-m", &v.to_string()])?;
    }
    let db = TestDb::create().await?;
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: d.to_owned(),
            master: "master".into(),
            url: None,
            schema: SCHEMA.into(),
            paths: Vec::new(),
            install: false,
        },
    )
    .await?;
    let reader = db.reader().await?;
    let snapshot =
        pghacker_codesearch::rev::resolve(&reader, SCHEMA, "postgres", Some("19")).await?;
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
        history: &snapshot.history,
        mounted: false,
        file_ids: None,
        require: vec![&compiled.filter],
        reject: vec![],
        cap: pghacker_codesearch::engine::MAX_CANDIDATES,
        scope_first: true,
    };
    let (sql, params) = pghacker_codesearch::history::line_sql(&q, &snapshot.history, &[])?;
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
            "tree_text",
            Strings::from("for ("),
            MatchMode::Text,
            Strings::default(),
            GroupBy::File,
        ),
        (
            "tree_regex",
            Strings::from(r"^\s*for\s*\("),
            MatchMode::Regex,
            Strings::from(vec!["src".into(), "contrib".into()]),
            GroupBy::File,
        ),
        (
            "tree_match_counts",
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
