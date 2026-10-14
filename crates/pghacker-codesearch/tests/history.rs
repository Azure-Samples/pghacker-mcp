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
    git(dir, 1, &["tag", "REL_17_0", &c0]);

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
            a.map(|o| o.status),
            b.map(|o| o.status)
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
            url: None,
            schema: SCHEMA.into(),
            paths: paths.clone(),
            install: false,
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

#[tokio::test]
async fn revisions_resolve_in_the_history() {
    use pghacker_codesearch::rev::resolve;
    let dir = tempfile::tempdir().unwrap();
    let commits = repository(dir.path());
    let (c2, b1, c0) = (&commits[0].2, &commits[1].2, &commits[2].2);
    let db = TestDb::create().await.unwrap();
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: dir.path().to_owned(),
            master: "master".into(),
            url: None,
            schema: SCHEMA.into(),
            paths: vec!["src".into(), "contrib".into()],
            install: false,
        },
    )
    .await
    .unwrap();
    let reader = db.reader().await.unwrap();
    let at = |v: Option<&str>| {
        let (reader, v) = (&reader, v.map(str::to_owned));
        async move { resolve(reader, SCHEMA, "postgres", v.as_deref()).await }
    };
    // The newest release line by default; 19, one past it, is master.
    let head = at(None).await.unwrap();
    assert_eq!(
        (head.git_ref.as_str(), head.sha.as_str()),
        ("REL_18_STABLE", b1.as_str())
    );
    assert_eq!(head.version.as_deref(), Some("18"));
    assert_eq!(at(Some("19")).await.unwrap().sha, *c2);
    assert_eq!(at(Some("master^^")).await.unwrap().sha, *c0);
    let fork = at(Some("REL_18_STABLE~1")).await.unwrap();
    assert_eq!(fork.git_ref, "master");
    let by_sha = at(Some(&c0[..8])).await.unwrap();
    assert_eq!(
        (by_sha.git_ref.as_str(), by_sha.sha.as_str()),
        ("master", c0.as_str())
    );
    // c1 was committed at 00:02, c2 at 00:04.
    let dated = at(Some("master@2026-01-01T00:03:00Z")).await.unwrap();
    assert_eq!(dated.sha, fork.sha);
    assert!(at(Some("master@2025-12-31")).await.is_err());
    assert!(
        at(Some("REL_99_STABLE"))
            .await
            .unwrap_err()
            .to_string()
            .contains("REL_18_STABLE")
    );
    assert!(at(Some("master~9")).await.is_err());

    // A resolved commit reads like the snapshot of it.
    let mut indexer = db.indexer().await.unwrap();
    load_snapshot(
        &mut indexer,
        &SnapshotSpec {
            repo: "postgres".into(),
            git_ref: "REL_17_STABLE".into(),
            sha: c0.clone(),
            version: Some("17".into()),
            paths: vec!["src".into(), "contrib".into()],
            source: RepoSource::Local {
                path: dir.path().to_owned(),
            },
            source_id: None,
        },
    )
    .await
    .unwrap();
    let snap = resolve_snapshot(&reader, "postgres", Some("17"))
        .await
        .unwrap();
    let hist = at(Some("master~2")).await.unwrap();
    for command in ["grep -rn json .", "grep -c '' src/backend/parser/gram.y"] {
        let a = run(&reader, &snap, command, Limits::default())
            .await
            .unwrap();
        let b = run(&reader, &hist, command, Limits::default())
            .await
            .unwrap();
        assert_eq!(a.stdout, b.stdout, "{command}");
    }
}

#[tokio::test]
async fn virtual_folders_mount_other_commits() {
    let dir = tempfile::tempdir().unwrap();
    let commits = repository(dir.path());
    let db = TestDb::create().await.unwrap();
    let paths = vec!["src".to_owned(), "contrib".to_owned()];
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: dir.path().to_owned(),
            master: "master".into(),
            url: None,
            schema: SCHEMA.into(),
            paths: paths.clone(),
            install: false,
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
    let snap = |v: &'static str| {
        let reader = &reader;
        async move { resolve_snapshot(reader, "postgres", Some(v)).await.unwrap() }
    };
    let (s19, s18, s17) = (snap("19").await, snap("18").await, snap("17").await);
    let hist = attach(&reader, s19.clone(), SCHEMA).await.unwrap();
    let out = |s: &Snapshot, command: &str| {
        let (reader, s, command) = (&reader, s.clone(), command.to_owned());
        async move {
            let o = run(reader, &s, &command, Limits::default()).await.unwrap();
            (String::from_utf8(o.stdout).unwrap(), o.stderr, o.status)
        }
    };
    let prefixed = |text: &str, from: &str, to: &str| -> String {
        text.lines()
            .map(|l| match l.strip_prefix(from) {
                Some(rest) => format!("{to}{rest}\n"),
                None => format!("{l}\n"),
            })
            .collect()
    };

    // A mounted branch reads as that branch's snapshot does.
    let (o18, _, _) = out(&s18, "grep -rn json src").await;
    let (o, e, rc) = out(&hist, "grep -rn json /branch/REL_18_STABLE/src").await;
    assert_eq!(
        (o.as_str(), rc),
        (
            prefixed(&o18, "src/", "/branch/REL_18_STABLE/src/").as_str(),
            0
        ),
        "{e:?}"
    );
    let (o, _, _) = out(&hist, "cd /branch/REL_18_STABLE && grep -rn json src").await;
    assert_eq!(o, o18);
    let (o17, _, _) = out(&s17, "grep -rn json src").await;
    let (o, _, _) = out(&hist, "grep -rn json /tag/REL_17_0/src").await;
    assert_eq!(o, prefixed(&o17, "src/", "/tag/REL_17_0/src/"));
    let sha = &s17.sha[..10];
    let (o, _, _) = out(&hist, &format!("grep -rn json /commit/{sha}/src")).await;
    assert_eq!(o, prefixed(&o17, "src/", &format!("/commit/{sha}/src/")));

    // Two versions in one command; the root stays the commit's own tree.
    let (o19, _, _) = out(&s19, "grep -rn json_from_cstring src").await;
    let (o, _, _) = out(
        &hist,
        "grep -rn json_from_cstring src /branch/REL_18_STABLE/src",
    )
    .await;
    let (b18, _, _) = out(&s18, "grep -rn json_from_cstring src").await;
    assert_eq!(
        o,
        format!(
            "{o19}{}",
            prefixed(&b18, "src/", "/branch/REL_18_STABLE/src/")
        )
    );
    let (o, _, _) = out(&hist, "grep -rln json . /tag/REL_17_0/contrib").await;
    assert!(
        !o.contains("~v") && o.contains("./src/") && o.contains("/tag/REL_17_0/contrib/"),
        "{o}"
    );

    // A virtual root needs a commit; one that names none doesn't exist.
    let (o, e, rc) = out(&hist, "grep -r json /commit").await;
    assert_eq!((o.as_str(), rc), ("", 2));
    assert!(e[0].contains("/commit/<sha>/"), "{e:?}");
    let (_, e, rc) = out(&hist, "grep -r json /branch/REL_99_STABLE/src").await;
    assert_eq!(rc, 2);
    assert_eq!(
        e,
        vec!["grep: /branch/REL_99_STABLE/src: No such file or directory"]
    );

    // read_file, search and list_files.
    let read = |s: &Snapshot, path: &str| {
        let (reader, s, path) = (&reader, s.clone(), path.to_owned());
        async move {
            read_file(
                reader,
                &s,
                &ReadFileArgs {
                    path,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        }
    };
    let a = read(&s17, "src/backend/parser/gram.y").await;
    let b = read(&hist, "/tag/REL_17_0/src/backend/parser/gram.y").await;
    assert_eq!(
        (b.content, b.path.as_str()),
        (a.content, "/tag/REL_17_0/src/backend/parser/gram.y")
    );
    let args: SearchArgs =
        serde_json::from_value(json!({"query": "json", "paths": "src/backend"})).unwrap();
    let a = search(&reader, &s18, &args).await.unwrap();
    let args: SearchArgs = serde_json::from_value(
        json!({"query": "json", "paths": "/branch/REL_18_STABLE/src/backend"}),
    )
    .unwrap();
    let b = search(&reader, &hist, &args).await.unwrap();
    assert_eq!(a.total_lines, b.total_lines);
    let shown: Vec<String> = a
        .lines
        .iter()
        .map(|h| format!("/branch/REL_18_STABLE/{}:{}", h.path, h.line))
        .collect();
    let got: Vec<String> = b
        .lines
        .iter()
        .map(|h| format!("{}:{}", h.path, h.line))
        .collect();
    assert_eq!(got, shown);
    let list = |path: &str, glob: Option<&str>| {
        let reader = &reader;
        let hist = hist.clone();
        let args = ListFilesArgs {
            path: Some(path.to_owned()),
            glob: glob.map(str::to_owned),
            ..Default::default()
        };
        async move {
            list_files(reader, &hist, &args)
                .await
                .unwrap()
                .entries
                .into_iter()
                .map(|e| e.path)
                .collect::<Vec<String>>()
        }
    };
    assert_eq!(
        list("/branch", None).await,
        vec!["/branch/REL_18_STABLE", "/branch/master"]
    );
    assert_eq!(list("/tag", Some("REL_17*")).await, vec!["/tag/REL_17_0"]);
    let newest: Vec<String> = commits
        .iter()
        .map(|c| format!("/commit/{}", &c.2[..12]))
        .collect();
    let listed = list("/commit", None).await;
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0], newest[0]);
    assert_eq!(
        list("/branch/REL_18_STABLE/src/backend", None).await,
        vec![
            "/branch/REL_18_STABLE/src/backend/parser",
            "/branch/REL_18_STABLE/src/backend/utils"
        ]
    );
    assert!(
        !list("/", None)
            .await
            .iter()
            .any(|p| p.contains("~v") || p.starts_with("/branch"))
    );
}

#[tokio::test]
async fn history_search_finds_lines_across_commits() {
    let dir = tempfile::tempdir().unwrap();
    let commits = repository(dir.path());
    let db = TestDb::create().await.unwrap();
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: dir.path().to_owned(),
            master: "master".into(),
            url: None,
            schema: SCHEMA.into(),
            paths: vec!["src".into(), "contrib".into()],
            install: false,
        },
    )
    .await
    .unwrap();
    let reader = db.reader().await.unwrap();
    let head = pghacker_codesearch::rev::resolve(&reader, SCHEMA, "postgres", None)
        .await
        .unwrap();
    let find = |history: &str, paths: Option<&str>| {
        let (reader, head) = (&reader, head.clone());
        let mut args = json!({"query": "json_from_cstring", "history": history});
        if let Some(p) = paths {
            args["paths"] = json!(p);
        }
        async move {
            let args: SearchArgs = serde_json::from_value(args).unwrap();
            search(reader, &head, &args).await.unwrap()
        }
    };
    let all = find("all", None).await;
    assert!(all.complete);
    let texts: Vec<&str> = all.history.iter().map(|h| h.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "\treturn json_from_cstring(json);",
            "\treturn json_from_cstring_safe(json);"
        ]
    );
    let (c2, b1, c0) = (&commits[0].2, &commits[1].2, &commits[2].2);
    let original = &all.history[0];
    assert_eq!(original.path, "src/backend/utils/adt/json.c");
    assert_eq!(
        (original.added.sha.as_str(), original.added.branch.as_str()),
        (c0.as_str(), "master")
    );
    assert_eq!(original.line, 10);
    // The branch replaced it; master still has it, renamed into jsonfuncs.c.
    let removed: Vec<&str> = original.removed.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(removed, vec![b1.as_str()]);
    assert_eq!(original.branches, vec!["master"]);
    let releases = original.releases.as_ref().unwrap();
    assert_eq!((releases.first.as_str(), releases.count), ("REL_17_0", 1));
    let fixed = &all.history[1];
    assert_eq!(
        (fixed.added.sha.as_str(), fixed.branches.clone()),
        (b1.as_str(), vec!["REL_18_STABLE".to_owned()])
    );
    assert!(fixed.releases.is_none());

    let texts = |r: &pghacker_codesearch::SearchResult| -> usize { r.history.len() };
    assert_eq!(texts(&find("REL_17_0", None).await), 1);
    assert_eq!(texts(&find("master", None).await), 1);
    assert_eq!(texts(&find("REL_17_0..REL_18_STABLE", None).await), 2);
    assert_eq!(
        texts(&find(&format!("{}..{}", &c2[..8], &b1[..8]), None).await),
        1
    );
    assert_eq!(texts(&find("all", Some("src/backend/utils")).await), 2);
    assert_eq!(texts(&find("all", Some("src/include")).await), 0);
    // grep over /history: each line once, at the first commit that has it.
    let grep = |command: String| {
        let (reader, head) = (&reader, head.clone());
        async move {
            let o = run(reader, &head, &command, Limits::default())
                .await
                .unwrap();
            (String::from_utf8(o.stdout).unwrap(), o.stderr, o.status)
        }
    };
    let (o, e, rc) = grep("grep -rn json_from_cstring /history".into()).await;
    assert_eq!(
        (o.as_str(), rc),
        (
            format!(
                "/commit/{}/src/backend/utils/adt/json.c:10:\treturn json_from_cstring(json);\n\
                 /commit/{}/src/backend/utils/adt/json.c:10:\treturn json_from_cstring_safe(json);\n",
                &c0[..12],
                &b1[..12]
            )
            .as_str(),
            0
        ),
        "{e:?}"
    );
    let (o, _, _) =
        grep("grep -rn json_from_cstring /history/REL_18_STABLE..master/src".into()).await;
    assert_eq!(
        o,
        format!(
            "/commit/{}/src/backend/utils/adt/jsonfuncs.c:10:\treturn json_from_cstring(json);\n",
            &c2[..12]
        )
    );
    let (o, _, _) = grep("grep -c json_from_cstring /history/REL_17_0".into()).await;
    assert_eq!(
        o,
        format!("/commit/{}/src/backend/utils/adt/json.c:1\n", &c0[..12])
    );
    let (o, _, rc) = grep("grep -rl json_from /history/src/include".into()).await;
    assert_eq!((o.as_str(), rc), ("", 1));
    let error = run(
        &reader,
        &head,
        "grep -rn json src /history",
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("/history"), "{error}");

    let snap_args: SearchArgs =
        serde_json::from_value(json!({"query": "x", "history": "all"})).unwrap();
    let mut indexer = db.indexer().await.unwrap();
    load_snapshot(
        &mut indexer,
        &SnapshotSpec {
            repo: "postgres".into(),
            git_ref: "master".into(),
            sha: c2.clone(),
            version: Some("19".into()),
            paths: vec!["src".into()],
            source: RepoSource::Local {
                path: dir.path().to_owned(),
            },
            source_id: None,
        },
    )
    .await
    .unwrap();
    let snap = resolve_snapshot(&reader, "postgres", Some("19"))
        .await
        .unwrap();
    let error = search(&reader, &snap, &snap_args).await.unwrap_err();
    assert!(error.to_string().contains("history"), "{error}");
}
