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
    // Text that isn't UTF-8: binary to the tools, though the history keeps
    // its lines.
    std::fs::write(
        dir.join("src/backend/utils/adt/latin1.c"),
        b"/* json caf\xe9 */\nint json_latin1 = makeNode(Datum);\n",
    )
    .unwrap();
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
        // No match in the files named: alternative spellings and names.
        json!({"query": "jsn_out", "paths": "src/backend"}),
        json!({"query": "no_such_name", "paths": ["src/include", "contrib"]}),
        // Its only near spelling is in a binary file (Latin-1 text).
        json!({"query": "json_latin2"}),
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
                mounted: false,
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

        // So do trigram reads of a scope: one commit's files, or files that
        // mount another commit's.
        let own = |s: Snapshot| {
            let reader = &reader;
            async move {
                let table = pghacker_codesearch::snapshot::FileTable::load(reader, &s)
                    .await
                    .unwrap();
                // Callers leave binary files out of the scopes they read.
                let ids: Vec<i32> = table.files[..table.own]
                    .iter()
                    .filter(|f| !f.is_binary)
                    .map(|f| f.id)
                    .collect();
                ids
            }
        };
        let half: Vec<i32> = own(hist.clone()).await.into_iter().step_by(2).collect();
        let old = resolve_snapshot(&reader, "postgres", Some("17"))
            .await
            .unwrap();
        let mut mixed = own(attach(&reader, old, SCHEMA).await.unwrap()).await;
        mixed.extend(&half);
        mixed.sort_unstable();
        mixed.dedup();
        for (ids, mounted) in [(&half, false), (&mixed, true)] {
            let mut found = Vec::new();
            for scope_first in [true, false] {
                let q = LineQuery {
                    snapshot_id: hist.id,
                    history: hist.history.as_ref(),
                    mounted,
                    file_ids: Some(ids.as_slice()),
                    require: vec![&compiled.filter],
                    reject: vec![],
                    cap: MAX_CANDIDATES,
                    scope_first,
                };
                let c = candidates(&reader, &q, &Budget::new(None)).await.unwrap();
                assert!(!c.capped && !c.timed_out);
                let mut rows: Vec<(i32, i32, String)> = c
                    .rows
                    .into_iter()
                    .map(|r| (r.file_id, r.line_no, r.line))
                    .collect();
                rows.sort();
                assert!(!rows.is_empty(), "{version} mounted {mounted}");
                found.push(rows);
            }
            assert_eq!(found[0], found[1], "{version} mounted {mounted}");
        }

        // Raw candidates of the whole commit: none from binary files.
        let make_node =
            pghacker_codesearch::pattern::compile(&pghacker_codesearch::pattern::PatternSpec {
                patterns: &["makeNode".into()],
                dialect: pghacker_codesearch::pattern::Dialect::Fixed,
                ignore_case: false,
                word: false,
                line: false,
                semantics: pghacker_codesearch::pattern::Semantics::LeftmostFirst,
            })
            .unwrap();
        for scope_first in [true, false] {
            let mut both = Vec::new();
            for s in [&snap, &hist] {
                let table = pghacker_codesearch::snapshot::FileTable::load(&reader, s)
                    .await
                    .unwrap();
                let path_of: std::collections::HashMap<i32, String> =
                    table.files.iter().map(|f| (f.id, f.path.clone())).collect();
                let q = LineQuery {
                    snapshot_id: s.id,
                    history: s.history.as_ref(),
                    mounted: false,
                    file_ids: None,
                    require: vec![&make_node.filter],
                    reject: vec![],
                    cap: MAX_CANDIDATES,
                    scope_first,
                };
                let mut rows: Vec<(String, i32)> = candidates(&reader, &q, &Budget::new(None))
                    .await
                    .unwrap()
                    .rows
                    .iter()
                    .map(|r| (path_of[&r.file_id].clone(), r.line_no))
                    .collect();
                rows.sort();
                both.push(rows);
            }
            assert!(!both[0].is_empty());
            assert_eq!(both[0], both[1], "{version} scope_first {scope_first}");
        }

        // Capped, both engines give the first lines in tree order.
        for (scope_first, whole) in [(true, true), (false, true), (true, false), (false, false)] {
            let mut capped = Vec::new();
            for s in [&snap, &hist] {
                let table = pghacker_codesearch::snapshot::FileTable::load(&reader, s)
                    .await
                    .unwrap();
                let ids: Vec<i32> = table.files[..table.own].iter().map(|f| f.id).collect();
                let path_of: std::collections::HashMap<i32, String> =
                    table.files.iter().map(|f| (f.id, f.path.clone())).collect();
                let q = LineQuery {
                    snapshot_id: s.id,
                    history: s.history.as_ref(),
                    mounted: false,
                    file_ids: (!whole).then_some(ids.as_slice()),
                    require: vec![&compiled.filter],
                    reject: vec![],
                    cap: 7,
                    scope_first,
                };
                let c = candidates(&reader, &q, &Budget::new(None)).await.unwrap();
                assert!(c.capped);
                let rows: Vec<(String, i32)> = c
                    .rows
                    .iter()
                    .map(|r| (path_of[&r.file_id].clone(), r.line_no))
                    .collect();
                capped.push(rows);
            }
            assert_eq!(
                capped[0], capped[1],
                "{version} scope_first {scope_first} whole {whole}"
            );
        }
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

/// The commit per line that `git blame` gives, with extra options.
fn git_blame(dir: &Path, rev: &str, path: &str, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["blame", "--porcelain"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&[rev, "--", path]);
    let out = git(dir, 99, &args);
    out.lines()
        .filter(|l| {
            l.len() > 41
                && l.as_bytes()[40] == b' '
                && l[..40].bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(|l| l[..12].to_owned())
        .collect()
}

#[tokio::test]
async fn blame_agrees_with_git_blame() {
    use pghacker_codesearch::{BlameArgs, blame};
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let write = |path: &str, data: &str| {
        let path = d.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    let commit = |tick: u32, message: &str| {
        git(d, tick, &["add", "-A"]);
        git(d, tick, &["commit", "-q", "-m", message]);
        git(d, tick, &["rev-parse", "HEAD"])
    };
    git(d, 0, &["init", "-q", "-b", "master"]);
    write("src/a.c", "int\nmain(void)\n{\nint x = 1;\nreturn x;\n}\n");
    commit(1, "c0");
    write(
        "src/a.c",
        "int\nmain(int argc)\n{\nint x = 1;\nint y = 2;\nreturn x;\n}\n",
    );
    let c1 = commit(2, "c1");
    git(d, 3, &["checkout", "-q", "-b", "REL_1_STABLE", &c1]);
    write(
        "src/a.c",
        "int\nmain(int argc)\n{\nint x = 1;\nint y = 3;\nreturn x;\n}\n",
    );
    let b1 = commit(3, "b1");
    write(
        "src/a.c",
        "int\nmain(int argc)\n{\nint x = 10;\nint y = 3;\nreturn x;\n}\n",
    );
    let b2 = commit(4, "b2");
    git(d, 4, &["checkout", "-q", "master"]);
    std::fs::remove_file(d.join("src/a.c")).unwrap();
    write(
        "src/b.c",
        "int\nmain(int argc)\n{\nint x = 1;\nint y = 2;\nreturn x + y;\n}\n",
    );
    commit(5, "c2: rename");
    // A reformatting commit, then the file that lists it.
    write(
        "src/b.c",
        "int\nmain(int argc)\n{\n\tint x = 1;\n\tint y = 2;\n\treturn x + y;\n}\n",
    );
    let indent = commit(6, "pgindent run");
    write(".git-blame-ignore-revs", &format!("# pgindent\n{indent}\n"));
    let head = commit(7, "list the reformatting");

    let db = TestDb::create().await.unwrap();
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
    .await
    .unwrap();
    let reader = db.reader().await.unwrap();
    let at = |rev: &str| {
        let (reader, rev) = (&reader, rev.to_owned());
        async move {
            pghacker_codesearch::rev::resolve(reader, SCHEMA, "postgres", Some(&rev))
                .await
                .unwrap()
        }
    };
    let ours = |snap: Snapshot, path: &str, ignore: bool, reverse: bool| {
        let (reader, path) = (&reader, path.to_owned());
        async move {
            let args = BlameArgs {
                path,
                ignore_revs: Some(ignore),
                reverse,
                ..Default::default()
            };
            blame(reader, &snap, &args).await.unwrap()
        }
    };
    let commits = |r: &pghacker_codesearch::BlameResult| -> Vec<String> {
        r.lines.iter().map(|l| l.commit.clone()).collect()
    };
    let master = at("master").await;
    let plain = ours(master.clone(), "src/b.c", false, false).await;
    assert_eq!(commits(&plain), git_blame(d, &head, "src/b.c", &[]));
    assert_eq!(plain.lines[3].text, "\tint x = 1;");
    let ignoring = ours(master.clone(), "src/b.c", true, false).await;
    assert_eq!(
        commits(&ignoring),
        git_blame(
            d,
            &head,
            "src/b.c",
            &["--ignore-revs-file", ".git-blame-ignore-revs"]
        )
    );
    assert_eq!(ignoring.ignored_revs, 1);
    assert!(!commits(&ignoring).contains(&indent[..12].to_owned()));
    // A mounted commit reads its own list: c1's tree has none.
    let mounted_head = ours(
        at(&c1[..10]).await,
        &format!("/commit/{}/src/b.c", &head[..10]),
        true,
        false,
    )
    .await;
    assert_eq!(
        (commits(&mounted_head), mounted_head.ignored_revs),
        (commits(&ignoring), 1)
    );
    let branch = ours(at("REL_1_STABLE").await, "src/a.c", true, false).await;
    assert_eq!(commits(&branch), git_blame(d, &b2, "src/a.c", &[]));
    let mounted = ours(master.clone(), "/branch/REL_1_STABLE/src/a.c", true, false).await;
    assert_eq!(
        (commits(&mounted), mounted.path.as_str()),
        (commits(&branch), "/branch/REL_1_STABLE/src/a.c")
    );

    // Reverse: which commit, on the version's lane, removed each line.
    let gone = |r: &pghacker_codesearch::BlameResult| -> Vec<(u32, String)> {
        r.lines
            .iter()
            .filter(|l| !l.commit.is_empty())
            .map(|l| (l.line, r.commits[&l.commit].subject.clone()))
            .collect()
    };
    // On master the rename keeps the lineage; b1's change is on its branch.
    let reverse = ours(at(&c1[..10]).await, "src/a.c", false, true).await;
    assert_eq!(
        gone(&reverse),
        vec![
            (4, "pgindent run".to_owned()),
            (5, "pgindent run".to_owned()),
            (6, "c2: rename".to_owned())
        ]
    );
    let on_branch = ours(
        master.clone(),
        &format!("/commit/{}/src/a.c", &b1[..10]),
        false,
        true,
    )
    .await;
    assert_eq!(gone(&on_branch), vec![(4, "b2".to_owned())]);
    let at_head = ours(at("REL_1_STABLE").await, "src/a.c", false, true).await;
    assert!(gone(&at_head).is_empty());
}

/// A C file of functions f0..f5, with `edit` applied to its lines.
fn c_file(edit: impl Fn(&mut Vec<String>)) -> String {
    let mut lines = vec!["/* a.c */".to_owned()];
    for f in 0..6 {
        lines.push(String::new());
        lines.push("static int".into());
        lines.push(format!("f{f}(void)"));
        lines.push("{".into());
        for i in 0..8 {
            lines.push(format!("\tint v{i} = {f}{i};"));
        }
        lines.push("\treturn 0;".into());
        lines.push("}".into());
    }
    edit(&mut lines);
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// A diff without the lines the history doesn't keep: file modes, blob
/// ids and rename similarity.
fn comparable(diff: &str) -> Vec<String> {
    diff.lines()
        .filter(|l| {
            ![
                "index ",
                "new file",
                "deleted file",
                "similarity index",
                "old mode",
                "new mode",
            ]
            .iter()
            .any(|p| l.starts_with(p))
        })
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn get_commit_matches_git_show() {
    use pghacker_codesearch::{GetCommitArgs, get_commit};
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let write = |path: &str, data: &[u8]| {
        let path = d.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    let commit = |tick: u32, message: &str| {
        git(d, tick, &["add", "-A"]);
        git(d, tick, &["commit", "-q", "-m", message]);
        git(d, tick, &["rev-parse", "HEAD"])
    };
    git(d, 0, &["init", "-q", "-b", "master"]);
    write("src/a.c", c_file(|_| {}).as_bytes());
    write("doc/readme.txt", b"one\ntwo\nthree");
    write("bin.dat", b"\x00\x01\x02");
    write("src/empty.c", b"");
    let c0 = commit(1, "c0");
    // Far-apart changes, close ones, a line gaining its newline.
    write(
        "src/a.c",
        c_file(|l| {
            l[8] = "\tint v3 = 3;".into();
            l.insert(25, "\t/* new in f1 */".into());
            l[37] = "\tint v5 = 25 + 1;".into();
            l[39] = "\tint v7 = 27 + 1;".into();
            l[61] = "\tint v5 = 4;".into();
        })
        .as_bytes(),
    );
    write("doc/readme.txt", b"one\ntwo\nthree\nfour\n");
    write("bin.dat", b"\x00\x09");
    let c1 = commit(2, "c1");
    git(d, 3, &["checkout", "-q", "-b", "REL_1_STABLE", &c1]);
    write(
        "src/a.c",
        c_file(|l| {
            l[8] = "\tint v3 = 3;".into();
            l.insert(25, "\t/* new in f1 */".into());
            l[37] = "\tint v5 = 25 + 1;".into();
            l[39] = "\tint v7 = 27 + 1;".into();
            l[61] = "\tint v5 = 4;".into();
            l[46] = "\tint v1 = 31 + 1;".into();
        })
        .as_bytes(),
    );
    let b1 = commit(3, "b1");
    git(d, 3, &["tag", "REL_1_0", &b1]);
    git(d, 4, &["checkout", "-q", "master"]);
    // Rename with a change, a deletion, a file without a final newline.
    std::fs::remove_file(d.join("src/a.c")).unwrap();
    write(
        "src/b.c",
        c_file(|l| {
            l[8] = "\tint v3 = 3;".into();
            l.insert(25, "\t/* new in f1 */".into());
            l[37] = "\tint v5 = 25 + 1;".into();
            l[39] = "\tint v7 = 27 + 1;".into();
            l[61] = "\tint v5 = 4;".into();
            l[75] = "\tint v7 = 57 + 1;".into();
        })
        .as_bytes(),
    );
    std::fs::remove_file(d.join("doc/readme.txt")).unwrap();
    write("src/new.c", b"int\nnew_function(void)\n{\n}");
    let c2 = commit(5, "c2");
    // Changes at the top, a binary deletion, a final newline added.
    let b = std::fs::read_to_string(d.join("src/b.c")).unwrap();
    write(
        "src/b.c",
        b.lines()
            .skip(2)
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .as_bytes(),
    );
    std::fs::remove_file(d.join("bin.dat")).unwrap();
    write("src/new.c", b"int\nnew_function(void)\n{\n}\n");
    let c3 = commit(6, "c3");

    let db = TestDb::create().await.unwrap();
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
    .await
    .unwrap();
    let reader = db.reader().await.unwrap();
    let show = |sha: &str, args: GetCommitArgs| {
        let reader = &reader;
        let sha = sha.to_owned();
        async move {
            let args = GetCommitArgs {
                commit: sha[..12].to_owned(),
                ..args
            };
            get_commit(reader, SCHEMA, "postgres", &args).await.unwrap()
        }
    };
    for sha in [&c0, &c1, &b1, &c2, &c3] {
        let ours = show(sha, GetCommitArgs::default()).await;
        assert_eq!(&ours.sha, sha);
        let git_show = git(d, 0, &["show", "--format=", "-M50%", sha]);
        assert_eq!(
            comparable(ours.diff.trim()),
            comparable(&git_show),
            "commit {sha}"
        );
        for u in ["0", "1"] {
            let ours = show(
                sha,
                GetCommitArgs {
                    context: Some(u.parse().unwrap()),
                    ..Default::default()
                },
            )
            .await;
            let git_show = git(
                d,
                0,
                &["show", "--format=", "-M50%", &format!("-U{u}"), sha],
            );
            assert_eq!(
                comparable(ours.diff.trim()),
                comparable(&git_show),
                "commit {sha} -U{u}"
            );
        }
    }
    let c2_shown = show(&c2, GetCommitArgs::default()).await;
    let files: Vec<(&str, Option<&str>, &str)> = c2_shown
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.old_path.as_deref(), f.status.as_str()))
        .collect();
    assert_eq!(
        files,
        [
            ("doc/readme.txt", None, "D"),
            ("src/b.c", Some("src/a.c"), "R"),
            ("src/new.c", None, "A")
        ]
    );
    assert_eq!(c2_shown.parent.as_deref(), Some(c1.as_str()));
    assert_eq!(c2_shown.subject, "c2");
    // Paths and no diff.
    let only = show(
        &c2,
        GetCommitArgs {
            paths: vec!["src/a.c".into(), "*.txt".into()],
            ..Default::default()
        },
    )
    .await;
    let headers: Vec<&str> = only
        .diff
        .lines()
        .filter(|l| l.starts_with("diff --git"))
        .collect();
    assert_eq!(
        headers,
        [
            "diff --git a/doc/readme.txt b/doc/readme.txt",
            "diff --git a/src/a.c b/src/b.c"
        ]
    );
    let bare = show(
        &c2,
        GetCommitArgs {
            diff: Some(false),
            ..Default::default()
        },
    )
    .await;
    assert!(bare.diff.is_empty() && bare.files.len() == 3);
    // A master commit's first release is on the branch forked from it.
    let releases: Vec<(String, String)> = show(&c1, GetCommitArgs::default())
        .await
        .releases
        .into_iter()
        .map(|r| (r.branch, r.tag))
        .collect();
    assert_eq!(
        releases,
        [("REL_1_STABLE".to_owned(), "REL_1_0".to_owned())]
    );
}

#[tokio::test]
async fn search_commits_agrees_with_git_log() {
    use pghacker_codesearch::{SearchCommitsArgs, search_commits};
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let write = |path: &str, data: &str| {
        let path = d.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    let commit = |tick: u32, author: &str, message: &str| {
        git(d, tick, &["add", "-A"]);
        git(
            d,
            tick,
            &[
                "commit",
                "-q",
                "--author",
                &format!("{author} <a@example.org>"),
                "-m",
                message,
            ],
        );
        git(d, tick, &["rev-parse", "HEAD"])
    };
    git(d, 0, &["init", "-q", "-b", "master"]);
    write("a.c", "int\nx(void)\n{\n\treturn foo(1);\n}\n");
    write("doc/b.txt", "bee\n");
    commit(1, "Ann Lee", "Add a.c and b.txt");
    // foo's count stays: -G finds it, -S doesn't.
    write("a.c", "int\nx(void)\n{\n\treturn foo(2);\n}\n");
    commit(
        2,
        "Bob Ray",
        "Call foo with 2\n\nAuthor: Cy Doe <cy@example.org>",
    );
    write("a.c", "int\nx(void)\n{\n\tfoo(3);\n\treturn foo(2);\n}\n");
    let c2 = commit(3, "Ann Lee", "Call foo once more");
    git(d, 4, &["checkout", "-q", "-b", "REL_1_STABLE", &c2]);
    write("doc/b.txt", "bee\nbranch\n");
    commit(4, "Bob Ray", "Fix the docs on the branch");
    git(d, 5, &["checkout", "-q", "master"]);
    std::fs::remove_file(d.join("a.c")).unwrap();
    write(
        "src/a.c",
        "int\nx(void)\n{\n\tfoo(3);\n\treturn foo(2);\n}\n",
    );
    commit(5, "Ann Lee", "Move a.c to src");
    std::fs::remove_file(d.join("doc/b.txt")).unwrap();
    write("src/a.c", "int\nx(void)\n{\n\treturn foo(2);\n}\n");
    commit(6, "Bob Ray", "Drop b.txt and a foo call");

    let db = TestDb::create().await.unwrap();
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
    .await
    .unwrap();
    let reader = db.reader().await.unwrap();
    let ours = |args: Value| {
        let reader = &reader;
        async move {
            let args: SearchCommitsArgs = serde_json::from_value(args).unwrap();
            let mut shas: Vec<String> = search_commits(reader, SCHEMA, "postgres", &args)
                .await
                .unwrap()
                .commits
                .into_iter()
                .map(|c| c.sha)
                .collect();
            shas.sort();
            shas
        }
    };
    let theirs = |args: &[&str]| {
        let mut all = vec!["log", "--format=%H"];
        all.extend_from_slice(args);
        let mut shas: Vec<String> = git(d, 0, &all).lines().map(str::to_owned).collect();
        shas.sort();
        shas
    };
    let not_c2 = format!("^{c2}");
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({}), vec!["--all"]),
        (json!({"range": "master"}), vec!["master"]),
        (json!({"range": "REL_1_STABLE"}), vec!["REL_1_STABLE"]),
        (
            json!({"range": format!("{}..master", &c2[..10])}),
            vec!["master", &not_c2],
        ),
        (
            json!({"paths": ["src/a.c"]}),
            vec!["--all", "--", "src/a.c"],
        ),
        (json!({"paths": ["a.c"]}), vec!["--all", "--", "a.c"]),
        (json!({"paths": ["doc"]}), vec!["--all", "--", "doc"]),
        (json!({"paths": ["*.txt"]}), vec!["--all", "--", "*.txt"]),
        (
            json!({"changed_lines": "foo\\("}),
            vec!["--all", "-Gfoo\\("],
        ),
        (
            json!({"changed_lines": "foo\\(3"}),
            vec!["--all", "-Gfoo\\(3"],
        ),
        (json!({"occurrences": "foo("}), vec!["--all", "-Sfoo("]),
        (
            json!({"occurrences": "foo\\([23]", "occurrences_regex": true}),
            vec!["--all", "--pickaxe-regex", "-Sfoo\\([23]"],
        ),
        (json!({"occurrences": "bee"}), vec!["--all", "-Sbee"]),
        // Diff filters in paths (a.c's rename into src is left out: the
        // history follows a file's lineage, git log the paths named).
        (
            json!({"occurrences": "bee", "paths": ["doc"]}),
            vec!["--all", "-Sbee", "--", "doc"],
        ),
        (
            json!({"changed_lines": "bee", "paths": ["*.c"]}),
            vec!["--all", "-Gbee", "--", "*.c"],
        ),
        (
            json!({"author": "ann lee"}),
            vec!["--all", "-i", "--author=ann lee"],
        ),
        (json!({"message": "FOO"}), vec!["--all", "-i", "--grep=FOO"]),
        (
            json!({"range": "master", "changed_lines": "foo"}),
            vec!["master", "-Gfoo"],
        ),
    ];
    for (args, git_args) in cases {
        assert_eq!(ours(args.clone()).await, theirs(&git_args), "{args}");
    }
    // The Author trailer counts as authorship.
    let cy = ours(json!({"author": "Cy Doe"})).await;
    assert_eq!(cy.len(), 1);
    // Lines shown for diff filters, with their files.
    let found = search_commits(
        &reader,
        SCHEMA,
        "postgres",
        &serde_json::from_value(json!({"occurrences": "foo(3)"})).unwrap(),
    )
    .await
    .unwrap();
    let lines: Vec<(String, String, String)> = found
        .commits
        .iter()
        .flat_map(|c| {
            c.lines
                .iter()
                .map(|l| (l.path.clone(), l.sign.clone(), l.text.clone()))
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("src/a.c".to_owned(), "-".to_owned(), "\tfoo(3);".to_owned()),
            ("a.c".to_owned(), "+".to_owned(), "\tfoo(3);".to_owned()),
        ]
    );
}

#[tokio::test]
async fn search_patches_finds_lines_by_thread() {
    use pghacker_codesearch::{SearchPatchesArgs, search_patches};
    let diff = |path: &str, lines: &str| {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,2 +1,{} @@\n int\n x(void)\n{lines}",
            2 + lines.lines().filter(|l| l.starts_with('+')).count()
                - lines.lines().filter(|l| l.starts_with('-')).count()
        )
    };
    let v1 = diff("src/a.c", "+\tfoo_bar(1);\n");
    let v2 = diff("src/a.c", "+\tfoo_bar(1);\n+\tfoo_bar(2);\n");
    let removal = "diff --git a/src/b.c b/src/b.c\n--- a/src/b.c\n+++ b/src/b.c\n@@ -1,2 +1,1 @@\n int\n-\tfoo_bar(1);\n";

    // The commit that landed thread 1's patch names its first message.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, 0, &["init", "-q", "-b", "master"]);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/a.c"), "int\nx(void)\n").unwrap();
    git(d, 1, &["add", "-A"]);
    git(d, 1, &["commit", "-q", "-m", "base"]);
    std::fs::write(
        d.join("src/a.c"),
        "int\nx(void)\n\tfoo_bar(1);\n\tfoo_bar(2);\n",
    )
    .unwrap();
    git(d, 2, &["add", "-A"]);
    git(
        d,
        2,
        &[
            "commit",
            "-q",
            "-m",
            "Add foo_bar calls\n\nDiscussion: https://postgr.es/m/v1@example.org",
        ],
    );
    let landed_sha = git(d, 2, &["rev-parse", "HEAD"]);

    let db = TestDb::create().await.unwrap();
    let mut admin = db.admin().await.unwrap();
    let insert_patch = |id: i64, message: i64, content: &str, path: &str| {
        format!(
            "INSERT INTO pghacker.patches (id, message_id, filename, series_version, paths, content) \
             VALUES ({id}, {message}, 'v{id}-0001-x.patch', {id}, ARRAY['{path}'], $${content}$$);"
        )
    };
    admin
        .batch_execute(&format!(
            "INSERT INTO pghacker.threads (id, list, root_message_id, subject) VALUES
                 (1, 'pgsql-hackers', 'v1@example.org', 'Add foo_bar'),
                 (2, 'pgsql-hackers', 'rm@example.org', 'Remove foo_bar');
             INSERT INTO pghacker.messages (id, message_id, list, thread_id, sent_at, from_name) VALUES
                 (10, 'v1@example.org', 'pgsql-hackers', 1, '2025-01-01', 'Ann Lee'),
                 (11, 'v2@example.org', 'pgsql-hackers', 1, '2025-02-01', 'Ann Lee'),
                 (20, 'rm@example.org', 'pgsql-hackers', 2, '2025-03-01', 'Bob Ray');
             {}{}{}",
            insert_patch(1, 10, &v1, "src/a.c"),
            insert_patch(2, 11, &v2, "src/a.c"),
            insert_patch(3, 20, removal, "src/b.c"),
        ))
        .await
        .unwrap();
    let indexed = pghacker_mail::patch_lines::index(&mut admin, 100)
        .await
        .unwrap();
    assert_eq!((indexed.patches, indexed.more), (3, false));
    assert_eq!(
        pghacker_mail::patch_lines::index(&mut admin, 100)
            .await
            .unwrap()
            .patches,
        0
    );
    // "\tfoo_bar(1);" is one text for all its '+' lines.
    assert_eq!(
        admin
            .query_one(
                "SELECT count(*) FROM pghacker.patch_texts WHERE line = E'\\tfoo_bar(1);'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
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
    .await
    .unwrap();

    let reader = db.reader().await.unwrap();
    let run = |args: Value, history: bool| {
        let reader = &reader;
        async move {
            let args: SearchPatchesArgs = serde_json::from_value(args).unwrap();
            search_patches(reader, history.then_some(SCHEMA), &args)
                .await
                .unwrap()
        }
    };
    let found = run(json!({"query": "foo_bar(1)"}), true).await;
    type Summary = (
        i64,
        usize,
        String,
        String,
        Vec<(Option<String>, String, String)>,
    );
    let summary: Vec<Summary> = found
        .threads
        .iter()
        .map(|t| {
            (
                t.thread_id,
                t.patches,
                t.first.message_id.clone(),
                t.latest.message_id.clone(),
                t.lines
                    .iter()
                    .map(|l| (l.path.clone(), l.sign.clone(), l.text.clone()))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                2,
                1,
                "rm@example.org".to_owned(),
                "rm@example.org".to_owned(),
                vec![(
                    Some("src/b.c".to_owned()),
                    "-".to_owned(),
                    "\tfoo_bar(1);".to_owned()
                )]
            ),
            (
                1,
                2,
                "v1@example.org".to_owned(),
                "v2@example.org".to_owned(),
                vec![(
                    Some("src/a.c".to_owned()),
                    "+".to_owned(),
                    "\tfoo_bar(1);".to_owned()
                )]
            ),
        ]
    );
    assert_eq!(found.threads[1].lines[0].line, 7);
    let landed: Vec<&str> = found.threads[1]
        .landed
        .iter()
        .map(|c| c.sha.as_str())
        .collect();
    assert_eq!(landed, [landed_sha.as_str()]);
    assert!(found.threads[0].landed.is_empty());
    // Filters.
    let added = run(json!({"query": "foo_bar", "lines": "added"}), false).await;
    assert_eq!(
        added
            .threads
            .iter()
            .map(|t| t.thread_id)
            .collect::<Vec<_>>(),
        [1]
    );
    assert!(added.threads[0].landed.is_empty());
    let paths = run(json!({"query": "foo_bar", "paths": ["src/b.c"]}), false).await;
    assert_eq!(
        paths
            .threads
            .iter()
            .map(|t| t.thread_id)
            .collect::<Vec<_>>(),
        [2]
    );
    let by = run(
        json!({"query": "FOO_BAR", "match": "identifier", "author": "ann"}),
        false,
    )
    .await;
    assert_eq!(
        by.threads.iter().map(|t| t.thread_id).collect::<Vec<_>>(),
        [1]
    );
    assert!(!by.incomplete);

    // The commit's added lines are all in v2, posted in its linked thread.
    let shown = pghacker_codesearch::get_commit(
        &reader,
        SCHEMA,
        "postgres",
        &pghacker_codesearch::GetCommitArgs {
            commit: landed_sha.clone(),
            diff: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Where each line was first proposed.
    let snapshot = pghacker_codesearch::rev::resolve(&reader, SCHEMA, "postgres", Some("master"))
        .await
        .unwrap();
    let blamed = pghacker_codesearch::blame(
        &reader,
        &snapshot,
        &pghacker_codesearch::BlameArgs {
            path: "src/a.c".into(),
            proposals: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let proposed: Vec<(String, Option<String>)> = blamed
        .lines
        .iter()
        .map(|l| {
            (
                l.text.clone(),
                l.proposed.as_ref().map(|p| p.message_id.clone()),
            )
        })
        .collect();
    assert_eq!(
        proposed,
        [
            ("int".to_owned(), None),
            ("x(void)".to_owned(), None),
            (
                "\tfoo_bar(1);".to_owned(),
                Some("v1@example.org".to_owned())
            ),
            (
                "\tfoo_bar(2);".to_owned(),
                Some("v2@example.org".to_owned())
            ),
        ]
    );
    let committed = shown.committed_patch.expect("a committed patch");
    assert_eq!(
        (
            committed.message_id.as_str(),
            committed.thread_id,
            committed.share
        ),
        ("v2@example.org", 1, 1.0)
    );
}
