// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The public MCP server's `query` tool: one read-only SQL statement, AGE's
//! `cypher()` included, run as pgh_reader in a read-only transaction that is
//! always rolled back, with a 10-second statement timeout, answered as CSV.
//! Connection checkout is bounded at 5 seconds and the whole call at 20.
//!
//! The check and the tool ([`check`], [`QueryTool`]) are in `query`, and the
//! transaction, its text-format reads and the CSV in `execute`, whose
//! `run_readonly_query` binds its portal with the vendored tokio-postgres
//! fork's `bind_text`.

mod execute;
mod query;

use schemars::JsonSchema;
use serde::Deserialize;

pub use execute::{MAX_CSV_BYTES, NULL_TEXT};
pub use query::{
    CALL_TIMEOUT, DEFAULT_MAX_ROWS, MAX_ROWS, MAX_SQL_BYTES, QueryTool, STATEMENT_TIMEOUT,
    TIMEOUT_MESSAGE, check,
};

/// The tool's name.
pub const NAME: &str = "query";

/// The tool's description for agents.
pub const DESCRIPTION: &str = "Runs one read-only SQL statement on pghacker's PostgreSQL 17 \
    database and returns CSV: a header row, up to max_rows rows and 1 MiB, and the row count, with \
    a banner above when rows are cut. Tables, in schema pghacker: threads, messages, authors, \
    author_names, patches, patch_texts, anchors, message_chunks, features, decisions, positions, \
    citations, feature_threads, feature_files, sources, and the commit history's below. Text keeps \
    the email addresses written in it; the table of senders' addresses (author_emails) is not \
    readable. Query the AGE graph pghacker_graph with SELECT * FROM cypher('pghacker_graph', $$ MATCH \
    (a:Author)-[:AUTHORED]->(m:Message) RETURN a.name, count(m) $$) AS (name agtype, n agtype). \
    Call get_ontology for the graph's labels and edges and the tables' columns. Takes one \
    SELECT, WITH, VALUES, TABLE, EXPLAIN or SHOW statement of at most 64 KiB, run read-only with \
    a 10-second timeout. NULL reads as None, an empty string as \"\".";

/// The commit history's tables, which pghacker-mcp adds to the description.
pub const HISTORY_NOTE: &str = "The commit history's tables: code_commits, code_commit_files, \
    code_commit_threads, code_refs, code_lanes, code_lineages, code_file_versions, \
    code_line_instances and code_history; get_ontology shows how to read a commit's files and \
    lines.";

/// The tool's arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryArgs {
    /// One SQL statement that reads, such as a SELECT or a cypher() call, of at most 64 KiB.
    pub sql: String,
    /// Rows to return: 100 by default, at most 1,000. More are cut, with a banner.
    #[serde(default)]
    #[schemars(range(min = 1, max = 1000))]
    pub max_rows: Option<u32>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn arguments_take_sql_and_an_optional_max_rows() {
        let args: QueryArgs = serde_json::from_value(json!({"sql": "SELECT 1"})).unwrap();
        assert_eq!((args.sql.as_str(), args.max_rows), ("SELECT 1", None));
        let args: QueryArgs =
            serde_json::from_value(json!({"sql": "SELECT 1", "max_rows": 5})).unwrap();
        assert_eq!(args.max_rows, Some(5));
        assert!(
            serde_json::from_value::<QueryArgs>(json!({"sql": "SELECT 1", "limit": 5})).is_err()
        );
        assert!(serde_json::from_value::<QueryArgs>(json!({"max_rows": 5})).is_err());
        let schema = pghacker_core::mcp::schema::<QueryArgs>();
        assert_eq!(schema["required"], json!(["sql"]));
        assert_eq!(schema["properties"]["max_rows"]["minimum"], json!(1));
        assert_eq!(schema["properties"]["max_rows"]["maximum"], json!(MAX_ROWS));
    }

    #[test]
    fn the_description_says_what_agents_need() {
        for needed in [
            "schema pghacker",
            "message_chunks",
            "patch_texts",
            "feature_files",
            "cypher('pghacker_graph', $$",
            "AS (name agtype, n agtype)",
            "get_ontology",
            "one SELECT",
            "10-second",
            "banner",
            "Text keeps the email addresses written in it",
            "NULL reads as None",
        ] {
            assert!(DESCRIPTION.contains(needed), "{needed}");
        }
        assert!(
            !DESCRIPTION.contains("code_lines"),
            "the snapshots are retired"
        );
        assert!(HISTORY_NOTE.contains("code_line_instances"));
    }
}
