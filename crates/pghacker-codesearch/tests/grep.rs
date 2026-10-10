//! The grep, read_file and list_files tools against a synthetic snapshot.
//! The expected outputs are GNU grep 3.11's on the same files (LC_ALL=C.UTF-8).

mod common;

use anyhow::Result;
use pghacker_codesearch::grep::{Limits, run};
use pghacker_codesearch::{
    GrepArgs, ListFilesArgs, ReadFileArgs, Snapshot, grep, list_files, read_file,
};
use pghacker_core::testing::TestDb;
use tokio_postgres::Client;

async fn out(client: &Client, snapshot: &Snapshot, command: &str) -> (String, Vec<String>, i32) {
    let o = run(client, snapshot, command, Limits::default())
        .await
        .unwrap_or_else(|e| panic!("{command}: {e:#}"));
    (String::from_utf8(o.stdout).unwrap(), o.stderr, o.status)
}

const GRAM_8: &str = "        SelectStmt *n = makeNode(SelectStmt);";
const GRAM_15: &str = "        JsonTable *n = makeNode(JsonTable);";
const NODES_11: &str = "#define makeNode(_type_) ((_type_ *) newNode(sizeof(_type_), T_##_type_))";

#[tokio::test]
async fn incomplete_searches_cannot_turn_into_pipeline_counts() -> Result<()> {
    use pghacker_codesearch::engine::MAX_CANDIDATES;

    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &[("src/loops.c", Some(""))]).await?;
    let count = MAX_CANDIDATES as i32 + 2;
    indexer
        .execute(
            "UPDATE code_files SET line_count = $1 WHERE snapshot_id = $2",
            &[&count, &snapshot.id],
        )
        .await?;
    indexer
        .execute(
            "INSERT INTO code_lines (file_id, line_no, line)
             SELECT currval('code_files_id_seq'), n, 'for (int i = 0; i < 10; i++) {}'
             FROM generate_series(1, $1::int) n",
            &[&count],
        )
        .await?;
    indexer.batch_execute("VACUUM ANALYZE code_lines").await?;
    let reader = db.reader().await?;
    for command in [
        "grep -rF 'for (' src | wc -l",
        "grep -rF 'for (' src | sort | uniq -c",
        "grep -rvc 'for (' src | wc -l || echo none",
    ] {
        let output = run(&reader, &snapshot, command, Limits::default()).await?;
        assert!(output.incomplete, "{command}");
        assert_eq!(output.status, 2, "{command}");
        assert!(output.stdout.is_empty(), "{command}");
        assert!(output.stderr[0].contains("no result or count"), "{command}");
    }
    let error = grep(
        &reader,
        &snapshot,
        &GrepArgs {
            command: "grep -rF 'for (' src | wc -l".into(),
            version: None,
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("search stopped early"),
        "{error:#}"
    );
    let (selected, incomplete) =
        pghacker_codesearch::grep::select(&reader, &snapshot, "grep -rv 'for (' src", 10_000)
            .await?;
    assert!(incomplete);
    assert!(selected.iter().all(|file| file.lines.is_empty()));
    Ok(())
}

#[tokio::test]
async fn grep_prints_like_gnu_grep() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;
    let s = &snapshot;

    let (o, e, rc) = out(&reader, s, "grep -rn makeNode .").await;
    assert_eq!(
        o,
        format!(
            "./src/backend/parser/gram.y:8:{GRAM_8}\n./src/backend/parser/gram.y:15:{GRAM_15}\n\
             ./src/include/nodes/nodes.h:11:{NODES_11}\n"
        )
    );
    assert!(e.is_empty() && rc == 0);
    let (o, _, _) = out(&reader, s, "grep -rn makeNode").await;
    assert!(o.starts_with(&format!("src/backend/parser/gram.y:8:{GRAM_8}\n")));
    let (o, _, _) = out(&reader, s, "grep -rn 'makeNode(' src/backend").await;
    assert_eq!(
        o,
        format!("src/backend/parser/gram.y:8:{GRAM_8}\nsrc/backend/parser/gram.y:15:{GRAM_15}\n")
    );
    let (o, _, _) = out(
        &reader,
        s,
        "grep -c json src/backend/utils/adt/json.c contrib/pg_trgm/trgm_op.c",
    )
    .await;
    assert_eq!(
        o,
        "src/backend/utils/adt/json.c:8\ncontrib/pg_trgm/trgm_op.c:1\n"
    );
    let (o, _, _) = out(&reader, s, "grep -rc T_ src/include").await;
    assert_eq!(o, "src/include/nodes/nodes.h:5\n");
    let (o, _, _) = out(&reader, s, "grep -rl -i json .").await;
    assert_eq!(
        o,
        "./contrib/pg_trgm/trgm_op.c\n./src/backend/parser/gram.y\n\
         ./src/backend/utils/adt/json.c\n./src/include/nodes/nodes.h\n"
    );
    let (o, _, _) = out(&reader, s, "grep -rL json src").await;
    assert_eq!(o, "src/include/nodes/nodes.h\n");
    let (o, _, _) = out(&reader, s, "grep -n -C1 makeNode src/backend/parser/gram.y").await;
    assert_eq!(
        o,
        format!(
            "7-    {{\n8:{GRAM_8}\n9-        n->targetList = $2;\n--\n14-    {{\n15:{GRAM_15}\n\
             16-        $$ = (Node *) n;\n"
        )
    );
    let (o, _, _) = out(
        &reader,
        s,
        "grep -rn -A1 -e JsonTable -e SelectStmt src/include src/backend/parser",
    )
    .await;
    assert_eq!(
        o,
        format!(
            "src/include/nodes/nodes.h:7:\tT_JsonTable,\nsrc/include/nodes/nodes.h:8:\tT_SelectStmt,\n\
             src/include/nodes/nodes.h-9-}} NodeTag;\n--\nsrc/backend/parser/gram.y:3:stmt: SelectStmt\n\
             src/backend/parser/gram.y-4-    | CreateStmt\n--\n\
             src/backend/parser/gram.y:6:SelectStmt: SELECT target_list\n\
             src/backend/parser/gram.y-7-    {{\nsrc/backend/parser/gram.y:8:{GRAM_8}\n\
             src/backend/parser/gram.y-9-        n->targetList = $2;\n--\n\
             src/backend/parser/gram.y:15:{GRAM_15}\n\
             src/backend/parser/gram.y-16-        $$ = (Node *) n;\n"
        )
    );
    let (o, _, _) = out(
        &reader,
        s,
        "grep -o -E 'json_[a-z]+' src/backend/utils/adt/json.c",
    )
    .await;
    assert_eq!(
        o,
        "json_lex\njson_in\njson_from\njson_out\njson_value\njson_to\n"
    );
    let (o, _, _) = out(
        &reader,
        s,
        "grep -rhow 'T_[A-Za-z]*' src/include | sort | uniq -c",
    )
    .await;
    assert_eq!(
        o,
        "      2 T_\n      1 T_Invalid\n      1 T_JsonTable\n      1 T_SelectStmt\n"
    );
    let (o, _, _) = out(&reader, s, "grep -rn json --include='*.c' . | head -3").await;
    assert_eq!(
        o,
        "./contrib/pg_trgm/trgm_op.c:8:/* make_json_value(x) and MakeJsonValue and MAKE_JSON_VALUE */\n\
         ./src/backend/utils/adt/json.c:1:/* json.c: synthetic JSON functions */\n\
         ./src/backend/utils/adt/json.c:4:static void json_lex_string(JsonLexContext *lex);\n"
    );
    let (o, _, _) = out(&reader, s, "grep -rn make --exclude-dir=parser src | wc -l").await;
    assert_eq!(o, "2\n");
    let (o, _, _) = out(&reader, s, "grep -n 'NODES_H$' src/include/nodes/nodes.h").await;
    assert_eq!(o, "1:#ifndef NODES_H\n2:#define NODES_H\n");
    let (o, e, rc) = out(&reader, s, "grep 'x' src").await;
    assert_eq!(
        (o.as_str(), e, rc),
        ("", vec!["grep: src: Is a directory".to_owned()], 2)
    );
    let (_, e, rc) = out(&reader, s, "grep -r json src/nope src/include").await;
    assert_eq!(
        e,
        vec!["grep: src/nope: No such file or directory".to_owned()]
    );
    assert_eq!(rc, 2);
    let (o, _, _) = out(
        &reader,
        s,
        "grep -rnw 'n' src/backend/parser/gram.y | cut -c1-30",
    )
    .await;
    assert_eq!(
        o,
        "8:        SelectStmt *n = make\n9:        n->targetList = $2;\n\
         10:        $$ = (Node *) n;\n15:        JsonTable *n = make\n16:        $$ = (Node *) n;\n"
    );
    let (o, _, rc) = out(&reader, s, "grep -v -c '' contrib/pg_trgm/trgm_op.c").await;
    assert_eq!((o.as_str(), rc), ("", 1));
    let (o, _, _) = out(&reader, s, "grep -rZl json .").await;
    assert!(o.starts_with("./contrib/pg_trgm/trgm_op.c\0./src/backend/parser/gram.y\0"));
    let (o, _, _) = out(
        &reader,
        s,
        "grep -m1 -A2 -n Datum src/backend/utils/adt/json.c",
    )
    .await;
    assert_eq!(o, "6:Datum\n7-json_in(PG_FUNCTION_ARGS)\n8-{\n");
    Ok(())
}

#[tokio::test]
async fn grep_paths_pipelines_and_errors() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;
    let s = &snapshot;

    // Absolute and home paths into a checkout, and a cd prefix.
    let (o, _, _) = out(
        &reader,
        s,
        "grep -c makeNode /home/me/postgres/src/backend/parser/gram.y",
    )
    .await;
    assert_eq!(o, "2\n");
    let (o, _, _) = out(&reader, s, "cd ~/postgres/src && grep -rl makeNode backend").await;
    assert_eq!(o, "backend/parser/gram.y\n");
    // Wildcards expand like bash; the pattern stays when nothing matches.
    let (o, _, _) = out(
        &reader,
        s,
        "grep -l json contrib/pg_trgm/*.c src/*/*/adt/*.c",
    )
    .await;
    assert_eq!(
        o,
        "contrib/pg_trgm/trgm_op.c\nsrc/backend/utils/adt/json.c\n"
    );
    let (_, e, _) = out(&reader, s, "grep -l json src/*.c").await;
    assert_eq!(
        e,
        vec!["grep: src/*.c: No such file or directory".to_owned()]
    );
    // Stages after the grep, and || fallbacks.
    let (o, _, _) = out(
        &reader,
        s,
        "grep -rn json src | grep -v include | tail -n 2",
    )
    .await;
    assert_eq!(
        o,
        "src/backend/utils/adt/json.c:16:\t/* make_json_value is not called here */\n\
         src/backend/utils/adt/json.c:17:\treturn json_to_cstring(PG_GETARG_DATUM(0));\n"
    );
    let (o, _, _) = out(
        &reader,
        s,
        "grep -rho 'T_[A-Z][a-z]*' src | sort -r | head -2",
    )
    .await;
    assert_eq!(o, "T_Select\nT_Json\n");
    let (o, _, rc) = out(&reader, s, "grep -r nothing_here src || echo none").await;
    assert_eq!((o.as_str(), rc), ("none\n", 0));
    let (o, _, _) = out(
        &reader,
        s,
        "grep -rn json src 2>/dev/null | cut -d: -f1 | uniq -c",
    )
    .await;
    assert_eq!(
        o,
        "      1 src/backend/parser/gram.y\n      8 src/backend/utils/adt/json.c\n"
    );
    // The tool's text: output, then messages.
    let text = grep(
        &reader,
        s,
        &GrepArgs {
            command: "grep -c json src/backend/utils/adt/json.c src/x.c".into(),
            version: None,
        },
    )
    .await?;
    assert_eq!(
        text,
        "src/backend/utils/adt/json.c:8\ngrep: src/x.c: No such file or directory\n"
    );
    for (command, message) in [
        ("egrep x src", "grep -E"),
        ("rg x src", "search"),
        ("grep -rn x src | awk '{print $1}'", "not supported"),
        ("grep -rn x src; ls", "one grep pipeline"),
        ("grep -P '(' src", "unclosed group"),
        ("grep -E 'a{2,1}' -r src", "Invalid content"),
    ] {
        let error = run(&reader, s, command, Limits::default())
            .await
            .expect_err(command);
        assert!(
            format!("{error:#}").contains(message),
            "{command}: {error:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn read_file_and_list_files() -> Result<()> {
    let db = TestDb::create().await?;
    let indexer = db.indexer().await?;
    let snapshot = common::load(&indexer, "postgres", "19", &common::files()).await?;
    let reader = db.reader().await?;

    let r = read_file(
        &reader,
        &snapshot,
        &ReadFileArgs {
            path: "./src/include/nodes/nodes.h".into(),
            start_line: Some(12),
            end_line: Some(99),
            version: None,
        },
    )
    .await?;
    assert_eq!(
        (r.start_line, r.end_line, r.total_lines, r.cut),
        (12, 14, 14, false)
    );
    assert_eq!(
        r.content,
        concat!(
            "    12\t#define IsA(nodeptr,_type_) (nodeTag(nodeptr) == T_##_type_)\n",
            "    13\t\n",
            "    14\t#endif\t\t\t\t\t\t\t/* NODES_H */\n",
        )
    );
    let error = read_file(
        &reader,
        &snapshot,
        &ReadFileArgs {
            path: "src/backend/parser/json.c".into(),
            ..ReadFileArgs::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("src/backend/utils/adt/json.c"),
        "{error}"
    );
    let error = read_file(
        &reader,
        &snapshot,
        &ReadFileArgs {
            path: "src/backend".into(),
            ..ReadFileArgs::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("list_files"));

    let l = list_files(&reader, &snapshot, &ListFilesArgs::default()).await?;
    let entries: Vec<(&str, &str, Option<u32>)> = l
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.kind, e.files))
        .collect();
    assert_eq!(
        entries,
        vec![("contrib", "dir", Some(3)), ("src", "dir", Some(3))]
    );
    assert_eq!(l.loaded_paths, vec!["src", "contrib"]);
    let l = list_files(
        &reader,
        &snapshot,
        &ListFilesArgs {
            path: Some("contrib/pg_trgm".into()),
            depth: Some(2),
            ..ListFilesArgs::default()
        },
    )
    .await?;
    let paths: Vec<(&str, bool)> = l
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.binary))
        .collect();
    assert_eq!(
        paths,
        vec![
            ("contrib/pg_trgm/data", false),
            ("contrib/pg_trgm/data/trgm.data", true),
            ("contrib/pg_trgm/expected", false),
            ("contrib/pg_trgm/expected/empty.out", false),
            ("contrib/pg_trgm/trgm_op.c", false),
        ]
    );
    let l = list_files(
        &reader,
        &snapshot,
        &ListFilesArgs {
            glob: Some("*.[ch]".into()),
            limit: Some(2),
            ..ListFilesArgs::default()
        },
    )
    .await?;
    assert_eq!((l.total, l.cut, l.entries.len()), (3, true, 2));
    Ok(())
}
