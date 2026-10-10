//! Checks wide loop searches and pipeline counts on the full PG19 tree
//! against GNU grep, using a temporary database.

use anyhow::{Context, Result, ensure};
use pghacker_code::{RepoSource, SnapshotSpec};
use pghacker_codesearch::grep::{self, Limits};
use pghacker_codesearch::search::{GroupBy, MatchMode, OutputMode, Strings};
use pghacker_codesearch::{SearchArgs, resolve_snapshot, search};
use pghacker_core::config::repo_root;
use pghacker_core::testing::TestDb;
use serde_json::json;
use std::time::Instant;

#[path = "../src/gnu.rs"]
mod gnu;

#[tokio::main]
async fn main() -> Result<()> {
    let root = repo_root().context("cannot find the repository root")?;
    let db = TestDb::create().await?;
    let mut indexer = db.indexer().await?;
    let spec = SnapshotSpec {
        repo: "postgres".into(),
        git_ref: "REL_19_STABLE".into(),
        sha: "2359968b449a0fd53cfc04d4a644ae02370adec6".into(),
        version: Some("19".into()),
        paths: vec![],
        source: RepoSource::Remote {
            url: "https://github.com/postgres/postgres.git".into(),
            cache_dir: root.join(".runtime/git"),
        },
        source_id: None,
    };
    let loaded = pghacker_code::load_snapshot(&mut indexer, &spec).await?;
    let tree = root.join(".runtime/replay/profile-full/tree");
    pghacker_code::materialize(&spec, &tree)?;
    let reader = db.reader().await?;
    let snapshot = resolve_snapshot(&reader, "postgres", Some("19")).await?;
    let cases = [
        (
            "full_text",
            Strings::from("for ("),
            MatchMode::Text,
            GroupBy::File,
            "grep -rhF 'for (' src contrib | wc -l",
        ),
        (
            "full_regex",
            Strings::from(r"\bfor\s*\("),
            MatchMode::Regex,
            GroupBy::File,
            r"grep -rhE '\bfor[[:space:]]*\(' src contrib | wc -l",
        ),
        (
            "full_anchored_regex",
            Strings::from(r"^\s*for\s*\("),
            MatchMode::Regex,
            GroupBy::File,
            r"grep -rhE '^[[:space:]]*for[[:space:]]*\(' src contrib | wc -l",
        ),
        (
            "full_multi_match_counts",
            Strings::from(vec!["for (".into(), "while (".into()]),
            MatchMode::Text,
            GroupBy::Match,
            "grep -rhoF -e 'for (' -e 'while (' src contrib | wc -l",
        ),
    ];
    let mut jobs: Vec<gnu::Job> = cases
        .iter()
        .map(|(_, _, _, _, command)| gnu::Job {
            tree: "full".into(),
            cwd: "".into(),
            command: (*command).into(),
        })
        .collect();
    let grep_command = r"grep -rhE '^[[:space:]]*(for|while)[[:space:]]*\(' src contrib | wc -l";
    jobs.push(gnu::Job {
        tree: "full".into(),
        cwd: "".into(),
        command: grep_command.into(),
    });
    let native = gnu::run(
        &jobs,
        &[("full".into(), tree)],
        &root.join(".runtime/replay/profile-full/gnu"),
        1,
        "",
    )?;
    ensure!(
        native
            .iter()
            .all(|result| result.status == 0 && result.stderr.is_empty()),
        "GNU grep failed"
    );
    let mut results =
        vec![json!({ "case": "snapshot", "files": loaded.files, "lines": loaded.lines })];
    for (i, (label, query, match_mode, group_by, _)) in cases.into_iter().enumerate() {
        let args = SearchArgs {
            query,
            match_mode,
            group_by,
            output: OutputMode::Counts,
            paths: vec!["src".into(), "contrib".into()].into(),
            budget_ms: Some(10_000),
            ..SearchArgs::default()
        };
        let result = search(&reader, &snapshot, &args).await?;
        ensure!(
            result.complete && !result.budget_exceeded,
            "{label}: incomplete at {}ms",
            result.elapsed_ms
        );
        let expected: u64 = String::from_utf8(native[i].stdout.clone())?
            .trim()
            .parse()?;
        let actual = result.total_matches.unwrap_or(result.total_lines);
        ensure!(actual == expected, "{label}: {actual} != GNU's {expected}");
        results.push(json!({ "case": label, "elapsed_ms": result.elapsed_ms, "count": actual, "gnu_count": expected, "complete": result.complete }));
    }
    for budget_ms in [4_000, 10_000] {
        let start = Instant::now();
        let result = grep::run(
            &reader,
            &snapshot,
            grep_command,
            Limits {
                budget_ms,
                ..Limits::default()
            },
        )
        .await?;
        ensure!(
            !result.incomplete && result.status == 0,
            "grep at {budget_ms}ms: incomplete"
        );
        ensure!(
            result.stdout == native[4].stdout,
            "grep: output differs from GNU"
        );
        results.push(json!({ "case": "full_grep_wc", "budget_ms": budget_ms, "elapsed_ms": start.elapsed().as_millis(), "stdout": String::from_utf8(result.stdout)? }));
    }
    println!("{}", serde_json::to_string_pretty(&results)?);
    Ok(())
}
