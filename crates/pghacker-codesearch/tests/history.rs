//! The history engine against the snapshot engine: snapshots loaded from a
//! generated repository, and the same repository's history, must give the
//! same output for every tool call at every loaded commit.

mod common;

use std::path::Path;
use std::process::Command;

use pghacker_code::history::{BuildOptions, build};
use pghacker_code::{RepoSource, SnapshotSpec, load_snapshot};
use pghacker_codesearch::engine::{Budget, LineQuery, MAX_CANDIDATES, candidates};
use pghacker_codesearch::grep::{Limits, run};
use pghacker_codesearch::history::attach;
use pghacker_codesearch::{
    ListFilesArgs, ReadFileArgs, SearchArgs, Snapshot, list_files, read_file, resolve_snapshot,
    search,
};
use pghacker_core::testing::TestDb;
use serde_json::{Value, json};
use tokio_postgres::Client;

const SCHEMA: &str = "pghacker_history";

fn git(dir: &Path, tick: u32, args: &[&str]) -> String {
    let date = format!("{} +0000", 1_767_225_600 + tick * 60);
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.org")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.org")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Commits of the snapshot fixture's files, edited, renamed and
/// back-patched; returns (version, ref, sha) for three commits.
fn repository(dir: &Path) -> Vec<(&'static str, &'static str, String)> {
    let write = |path: &str, data: &str| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    git(dir, 0, &["init", "-q", "-b", "master"]);
    for (path, content) in common::files() {
        match content {
            Some(text) => write(path, text),
            None => {
                std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
                std::fs::write(dir.join(path), b"\x00\x01binary").unwrap();
            }
        }
    }
    write("doc/README", "outside the loaded paths\n");
    git(dir, 1, &["add", "-A"]);
    git(dir, 1, &["commit", "-q", "-m", "c0"]);
    let c0 = git(dir, 1, &["rev-parse", "HEAD"]);

    let gram = common::GRAM.replace(
        "stmt: SelectStmt\n",
        "stmt: SelectStmt\n    | JsonTableStmt\n    | AlterStmt\n",
    );
    write("src/backend/parser/gram.y", &gram);
    let json = common::JSON_C.replace(
        "Datum\njson_out",
        "static Node *\njson_node(void)\n{\n\treturn (Node *) makeNode(JsonTable);\n}\n\nDatum\njson_out",
    );
    write("src/backend/utils/adt/json.c", &json);
    write(
        "contrib/pg_trgm/trgm_gist.c",
        "/* trgm_gist.c */\nDatum\ngtrgm_in(PG_FUNCTION_ARGS)\n{\n\treturn 0;\n}\n",
    );
    git(dir, 2, &["add", "-A"]);
    git(dir, 2, &["commit", "-q", "-m", "c1"]);
    let c1 = git(dir, 2, &["rev-parse", "HEAD"]);

    git(dir, 3, &["checkout", "-q", "-b", "REL_18_STABLE", &c1]);
    write(
        "src/backend/utils/adt/json.c",
        &json.replace("json_from_cstring(json)", "json_from_cstring_safe(json)"),
    );
    git(dir, 3, &["add", "-A"]);
    git(dir, 3, &["commit", "-q", "-m", "b1"]);
    let b1 = git(dir, 3, &["rev-parse", "HEAD"]);
    git(dir, 3, &["checkout", "-q", "master"]);

    std::fs::remove_file(dir.join("src/backend/utils/adt/json.c")).unwrap();
    write(
        "src/backend/utils/adt/jsonfuncs.c",
        &json.replace("synthetic JSON functions", "JSON functions"),
    );
    std::fs::remove_file(dir.join("contrib/pg_trgm/expected/empty.out")).unwrap();
    write(
        "src/include/nodes/nodes.h",
        &common::NODES_H.replace("\tT_SelectStmt,\n", "\tT_SelectStmt,\n\tT_JsonExpr,\n"),
    );
    write(
        "src/backend/parser/gram.y",
        &format!(
            "{}alter_stmt: ALTER IDENT\n    {{\n        $$ = (Node *) makeNode(AlterStmt);\n    }}\n    ;\n",
            gram.replace("    | AlterStmt\n", "")
        ),
    );
    git(dir, 4, &["add", "-A"]);
    git(dir, 4, &["commit", "-q", "-m", "c2"]);
    let c2 = git(dir, 4, &["rev-parse", "HEAD"]);
    vec![
        ("19", "master", c2),
        ("18", "REL_18_STABLE", b1),
        ("17", "REL_17_STABLE", c0),
    ]
}

fn without_timing(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("elapsed_ms");
    }
    v
}

async fn same_grep(client: &Client, snap: &Snapshot, hist: &Snapshot, command: &str) {
    let a = run(client, snap, command, Limits::default()).await;
    let b = run(client, hist, command, Limits::default()).await;
    match (a, b) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                (String::from_utf8_lossy(&a.stdout), &a.stderr, a.status),
                (String::from_utf8_lossy(&b.stdout), &b.stderr, b.status),
                "{} {command}",
                snap.git_ref
            );
            assert!(!a.stdout.is_empty() || a.status != 0 || command.contains("-c"));
        }
        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{command}"),
        (a, b) => panic!(
            "{command}: snapshots {:?}, history {:?}",
            a.is_ok(),
            b.is_ok()
        ),
    }
}

async fn same_search(client: &Client, snap: &Snapshot, hist: &Snapshot, args: Value) {
    let parsed: SearchArgs = serde_json::from_value(args.clone()).unwrap();
    let a = search(client, snap, &parsed)
        .await
        .map(|r| without_timing(json!(r)));
    let b = search(client, hist, &parsed)
        .await
        .map(|r| without_timing(json!(r)));
    match (a, b) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{} {args}", snap.git_ref),
        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{args}"),
        (a, b) => panic!("{args}: snapshots {a:?}, history {b:?}"),
    }
}

#[tokio::test]
async fn history_and_snapshots_answer_alike() {
    let dir = tempfile::tempdir().unwrap();
    let commits = repository(dir.path());
    let db = TestDb::create().await.unwrap();
    let paths = vec!["src".to_owned(), "contrib".to_owned()];
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: dir.path().to_owned(),
            master: "master".into(),
            schema: SCHEMA.into(),
            paths: paths.clone(),
        },
    )
    .await
    .unwrap();
    let mut indexer = db.indexer().await.unwrap();
    for (version, git_ref, sha) in &commits {
        load_snapshot(
            &mut indexer,
            &SnapshotSpec {
                repo: "postgres".into(),
                git_ref: (*git_ref).into(),
                sha: sha.clone(),
                version: Some((*version).into()),
                paths: paths.clone(),
                source: RepoSource::Local {
                    path: dir.path().to_owned(),
                },
                source_id: None,
            },
        )
        .await
        .unwrap();
    }
    let reader = db.reader().await.unwrap();
    let greps = [
        "grep -rn makeNode .",
        "grep -rn makeNode",
        "grep -rc json src",
        "grep -rl -i json .",
        "grep -rL json src",
        "grep -n -C2 makeNode src/backend/parser/gram.y",
        "grep -rn -A1 -B2 json_ src/backend",
        "grep -rnb Datum src contrib",
        "grep -n -v T_ src/include/nodes/nodes.h",
        "grep -rnw json .",
        "grep -rnx '}' src",
        "grep -rno 'json_[a-z]*' .",
        "grep -rn --include=*.c PG_ .",
        "grep -rnE 'make(Node|_json)' . | head -3",
        "grep -c '' src/backend/parser/gram.y",
        "grep -n json src/backend/utils/adt/json.c",
        "grep -rn Stmt src/backend/parser | sort | uniq -c",
        "grep -rh T_ src/include | wc -l",
    ];
    let searches = [
        json!({"query": "makeNode"}),
        json!({"query": "json", "match": "identifier"}),
        json!({"query": "make json value", "match": "identifier"}),
        json!({"query": "Datum", "output": "files"}),
        json!({"query": "T_", "output": "counts", "paths": "src/include"}),
        json!({"query": "makeNod"}),
        json!({"query": "jsn_out"}),
        json!({"query": "json_[a-z]+", "match": "regex", "context": 1}),
        json!({"query": ["Stmt", "makeNode"], "order": "coverage", "output": "files"}),
        json!({"query": "return", "also": "json", "not": "cstring"}),
        json!({"query": "n", "paths": "src/backend/parser", "group_by": "match", "output": "counts"}),
        json!({"query": "PG_", "include": "*.c", "limit": 2, "offset": 1}),
    ];
    for (version, _, _) in &commits {
        let snap = resolve_snapshot(&reader, "postgres", Some(version))
            .await
            .unwrap();
        assert!(snap.history.is_none());
        let hist = attach(&reader, snap.clone(), SCHEMA).await.unwrap();
        for command in greps {
            same_grep(&reader, &snap, &hist, command).await;
        }
        for args in &searches {
            same_search(&reader, &snap, &hist, args.clone()).await;
        }
        for args in [
            json!({}),
            json!({"path": "src", "depth": 3}),
            json!({"glob": "*.c"}),
            json!({"path": "contrib/pg_trgm"}),
        ] {
            let parsed: ListFilesArgs = serde_json::from_value(args.clone()).unwrap();
            let a = list_files(&reader, &snap, &parsed).await.unwrap();
            let b = list_files(&reader, &hist, &parsed).await.unwrap();
            assert_eq!(json!(a), json!(b), "{version} {args}");
        }
        for (path, start, end) in [
            ("src/backend/parser/gram.y", None, None),
            ("src/include/nodes/nodes.h", Some(3), Some(9)),
            ("src/backend/utils/adt/json.c", Some(2), None),
            ("src/backend/utils/adt/jsonfuncs.c", None, Some(4)),
            ("contrib/pg_trgm/data/trgm.data", None, None),
        ] {
            let args = ReadFileArgs {
                path: path.into(),
                start_line: start,
                end_line: end,
                version: None,
            };
            let a = read_file(&reader, &snap, &args).await.map(|r| json!(r));
            let b = read_file(&reader, &hist, &args).await.map(|r| json!(r));
            match (a, b) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{version} {path}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{path}"),
                (a, b) => panic!("{version} {path}: {a:?} vs {b:?}"),
            }
        }

        // Both ways of reading candidates find the same lines.
        let compiled =
            pghacker_codesearch::pattern::compile(&pghacker_codesearch::pattern::PatternSpec {
                patterns: &["json".into()],
                dialect: pghacker_codesearch::pattern::Dialect::Fixed,
                ignore_case: false,
                word: false,
                line: false,
                semantics: pghacker_codesearch::pattern::Semantics::LeftmostFirst,
            })
            .unwrap();
        let mut found = Vec::new();
        for scope_first in [true, false] {
            let q = LineQuery {
                snapshot_id: hist.id,
                history: hist.history.as_ref(),
                file_ids: None,
                require: vec![&compiled.filter],
                reject: vec![],
                cap: MAX_CANDIDATES,
                scope_first,
            };
            let mut rows: Vec<(i32, i32, String)> = candidates(&reader, &q, &Budget::new(None))
                .await
                .unwrap()
                .rows
                .into_iter()
                .map(|r| (r.file_id, r.line_no, r.line))
                .collect();
            rows.sort();
            assert!(!rows.is_empty());
            found.push(rows);
        }
        assert_eq!(found[0], found[1], "{version}");
    }
}
