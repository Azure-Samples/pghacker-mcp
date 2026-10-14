//! A synthetic commit history for the integration tests: git repositories
//! of PG-like files, built into the test database's commit history where
//! the code tools read it.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use pghacker_code::history::{BuildOptions, build};
use pghacker_codesearch::{Snapshot, resolve_snapshot};
use pghacker_core::code::{DEFAULT_HISTORY_SCHEMA, history_schema};
use pghacker_core::testing::TestDb;

/// The path prefixes the fixture histories keep.
pub const PATHS: [&str; 2] = ["src", "contrib"];

/// (path, content) files; a content of None is a binary file.
pub type Files<'a> = [(&'a str, Option<&'a str>)];

/// Runs git in `dir` as a fixed author, `tick` minutes into 2026, and
/// returns its output, trimmed.
pub fn git(dir: &Path, tick: u32, args: &[&str]) -> String {
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

/// Writes these files into the work tree at `dir`, a binary file with NUL
/// bytes.
pub fn write_files(dir: &Path, files: &Files<'_>) {
    for (path, content) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match content {
            Some(text) => std::fs::write(path, text).unwrap(),
            None => std::fs::write(path, b"\x00\x01binary").unwrap(),
        }
    }
}

/// Builds the commit history of the repository at `git_dir`, keeping
/// `paths`, where the code tools read it: installed in `pghacker`, or in
/// the schema PGHACKER_CODE_HISTORY names.
pub async fn install(db: &TestDb, git_dir: &Path, paths: &[&str]) -> Result<()> {
    let schema = history_schema();
    let install = schema == DEFAULT_HISTORY_SCHEMA;
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: git_dir.to_owned(),
            master: "master".into(),
            url: None,
            schema: if install {
                "pghacker_fixture".into()
            } else {
                schema.into()
            },
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            install,
        },
    )
    .await?;
    Ok(())
}

/// A history of `repo` with these files committed on the branch
/// REL_<version>_STABLE: version `version`'s commit, as the code tools read
/// it.
pub async fn load(db: &TestDb, repo: &str, version: &str, files: &Files<'_>) -> Result<Snapshot> {
    load_versions(db, repo, &[(version, files)]).await
}

/// A history of `repo` with a branch REL_<version>_STABLE per version, each
/// forked from a master commit outside the kept paths and holding just its
/// files in one commit, so the versions share no lines. Returns the first
/// version's commit, as the code tools read it.
pub async fn load_versions(
    db: &TestDb,
    repo: &str,
    versions: &[(&str, &Files<'_>)],
) -> Result<Snapshot> {
    let dir = tempfile::tempdir()?;
    let d = dir.path();
    git(d, 0, &["init", "-q", "-b", "master"]);
    std::fs::write(d.join("README"), "outside the kept paths\n")?;
    git(d, 0, &["add", "-A"]);
    git(d, 0, &["commit", "-q", "-m", "base"]);
    let base = git(d, 0, &["rev-parse", "HEAD"]);
    for (k, (version, files)) in versions.iter().enumerate() {
        let tick = k as u32 + 1;
        let branch = format!("REL_{version}_STABLE");
        git(d, tick, &["checkout", "-q", "-b", &branch, &base]);
        write_files(d, files);
        git(d, tick, &["add", "-A"]);
        git(d, tick, &["commit", "-q", "--allow-empty", "-m", version]);
    }
    install(db, d, &PATHS).await?;
    let reader = db.reader().await?;
    resolve_snapshot(&reader, repo, Some(versions[0].0)).await
}

pub const GRAM: &str = "\
%token <str> IDENT FCONST
%%
stmt: SelectStmt
    | CreateStmt
    ;
SelectStmt: SELECT target_list
    {
        SelectStmt *n = makeNode(SelectStmt);
        n->targetList = $2;
        $$ = (Node *) n;
    }
    ;
json_table: JSON_TABLE '(' json_value_expr ')'
    {
        JsonTable *n = makeNode(JsonTable);
        $$ = (Node *) n;
    }
    ;
";

pub const JSON_C: &str = "\
/* json.c: synthetic JSON functions */
#include \"postgres.h\"

static void json_lex_string(JsonLexContext *lex);

Datum
json_in(PG_FUNCTION_ARGS)
{
\tchar\t   *json = PG_GETARG_CSTRING(0);
\treturn json_from_cstring(json);
}

Datum
json_out(PG_FUNCTION_ARGS)
{
\t/* make_json_value is not called here */
\treturn json_to_cstring(PG_GETARG_DATUM(0));
}
";

pub const NODES_H: &str = "\
#ifndef NODES_H
#define NODES_H

typedef enum NodeTag
{
\tT_Invalid = 0,
\tT_JsonTable,
\tT_SelectStmt,
} NodeTag;

#define makeNode(_type_) ((_type_ *) newNode(sizeof(_type_), T_##_type_))
#define IsA(nodeptr,_type_) (nodeTag(nodeptr) == T_##_type_)

#endif\t\t\t\t\t\t\t/* NODES_H */";

pub const TRGM_C: &str = "\
/* trgm_op.c: synthetic trigram operators */
float4
cnt_sml(TRGM *trg1, TRGM *trg2, bool inexact)
{
\treturn similarity_threshold;
}

/* make_json_value(x) and MakeJsonValue and MAKE_JSON_VALUE */
";

/// The fixture: PG-like paths with synthetic content.
pub fn files() -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("src/backend/parser/gram.y", Some(GRAM)),
        ("src/backend/utils/adt/json.c", Some(JSON_C)),
        ("src/include/nodes/nodes.h", Some(NODES_H)),
        ("contrib/pg_trgm/trgm_op.c", Some(TRGM_C)),
        ("contrib/pg_trgm/expected/empty.out", Some("")),
        ("contrib/pg_trgm/data/trgm.data", None),
    ]
}
