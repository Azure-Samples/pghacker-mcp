//! A synthetic snapshot for the integration tests.

#![allow(dead_code)]

use anyhow::Result;
use pghacker_codesearch::Snapshot;
use tokio_postgres::Client;

/// Lines as the loader splits them: on `\n`, without a final empty line.
pub fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Inserts a ready snapshot of `repo` with these (path, content) files; a
/// content of None is a binary file.
pub async fn load(
    client: &Client,
    repo: &str,
    version: &str,
    files: &[(&str, Option<&str>)],
) -> Result<Snapshot> {
    let row = client
        .query_one(
            "INSERT INTO code_snapshots (repo, ref, sha, version, paths, state, loaded_at) \
             VALUES ($1, 'REL_' || $2 || '_STABLE', md5($1 || $2), $2, ARRAY['src', 'contrib'], \
                     'ready', now()) RETURNING id, sha",
            &[&repo, &version],
        )
        .await?;
    let id: i32 = row.get(0);
    let sha: String = row.get(1);
    for (path, content) in files {
        let lines = content.map(split_lines).unwrap_or_default();
        let file: i32 = client
            .query_one(
                "INSERT INTO code_files (snapshot_id, path, blob_sha, size, line_count, \
                 is_binary, final_newline) VALUES ($1, $2, md5($2), $3, $4, $5, $6) RETURNING id",
                &[
                    &id,
                    path,
                    &(content.map_or(16, str::len) as i32),
                    &(lines.len() as i32),
                    &content.is_none(),
                    &content.is_none_or(|c| c.is_empty() || c.ends_with('\n')),
                ],
            )
            .await?
            .get(0);
        if !lines.is_empty() {
            let texts: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
            client
                .execute(
                    "INSERT INTO code_lines (file_id, line_no, line) \
                     SELECT $1, n, t FROM unnest($2::text[]) WITH ORDINALITY AS u(t, n)",
                    &[&file, &texts],
                )
                .await?;
        }
    }
    client
        .batch_execute("ANALYZE code_files, code_lines")
        .await?;
    Ok(Snapshot {
        id,
        repo: repo.to_owned(),
        version: Some(version.to_owned()),
        git_ref: format!("REL_{version}_STABLE"),
        sha,
    })
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
