// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Stored hostile corpus through the public tool implementation and both MCP
//! protocols. Nothing here renders markup or contacts a payload's URLs.

use pghacker_core::mcp::{McpServer, router};
use pghacker_core::skills::EXTENSION;
use pghacker_core::testing::TestDb;
use serde_json::{Value, json};

use super::{listen, server};
use crate::tools;

const RAW: &str = concat!(
    "needle\t<script>sample</script> ![image](https://example.invalid/pixel)",
    "\u{1b}[2J\u{9b}1G\u{202e}.patch\u{202c}\u{2066}x\u{2069}\r",
);
const CLEAN: &str = concat!(
    "needle\t<script>sample</script> ![image](https://example.invalid/pixel)",
    "[2J1G.patchx",
);
const PATH: &str = "src/\u{202e}fixture.c";

async fn corpus() -> TestDb {
    let db = TestDb::create().await.unwrap();
    let indexer = db.indexer().await.unwrap();
    indexer
        .batch_execute(
            "INSERT INTO authors (id, display_name, message_count) VALUES (1, 'Stored', 1);
             INSERT INTO threads (id, list, root_message_id, subject, message_count, participant_count)
                 VALUES (1, 'pgsql-hackers', 'message@example.org', 'Stored', 1, 1);
             INSERT INTO messages (id, message_id, list, thread_id, author_id, from_name,
                                   subject, body, body_clean, has_patch)
                 VALUES (1, 'message@example.org', 'pgsql-hackers', 1, 1, 'Stored',
                         'Stored', 'Stored', 'Stored', true);
             INSERT INTO patches (message_id, filename, paths, added, removed, content)
                 VALUES (1, 'fixture.patch', '{src/fixture.c}', 1, 0, 'Stored');
             INSERT INTO features (id, slug, name, description, status)
                 VALUES (1, 'fixture', 'Stored', 'Stored', 'proposed');
             INSERT INTO feature_threads (feature_id, thread_id) VALUES (1, 1);
             INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary)
                 VALUES (1, 1, 1, 1, 'concerned', 'Stored');
             INSERT INTO citations (extract_kind, extract_id, message_id, quote)
                 VALUES ('position', 1, 1, 'Stored');
             INSERT INTO code_history (master, paths, head)
                 VALUES ('master', '{src}', repeat('a', 40));
             INSERT INTO code_lanes (id, name, lo, hi, head_addr)
                 VALUES (0, 'master', 0, 4294967296, 0);
             INSERT INTO code_commits (addr, sha, lane, committed_at, authored_at,
                                       author_name, committer_name, subject, message)
                 VALUES (0, repeat('a', 40), 0, '2026-01-01', '2026-01-01',
                         'Stored', 'Stored', 'Stored', 'Stored');
             INSERT INTO code_refs (kind, name, addr)
                 VALUES ('branch', 'master', 0), ('branch', 'REL_19_STABLE', 0);
             INSERT INTO code_lineages (id, first_path, born) VALUES (1, 'src/fixture.c', 0);
             INSERT INTO code_file_versions (id, lineage, path, blob_sha, lo, hi, size,
                                             line_count, is_binary, final_newline, bom)
                 VALUES (1, 1, 'src/fixture.c', repeat('b', 40), 0, 4294967296, 7,
                         1, false, true, false);
             INSERT INTO code_line_instances (weave, lo, hi, id, lineage, bytes, line)
                 VALUES (4294967296, 0, 4294967296, 1, 1, 6, 'Stored');
             INSERT INTO code_commit_files (addr, path, status, added, removed, is_binary, version)
                 VALUES (0, 'src/fixture.c', 'A', 1, 0, false, 1);",
        )
        .await
        .unwrap();
    for sql in [
        "UPDATE authors SET display_name = $1",
        "UPDATE threads SET subject = $1",
        "UPDATE messages SET from_name = $1, subject = $1",
        "UPDATE patches SET filename = $1, paths = ARRAY[$1], content = $1",
        "UPDATE features SET name = $1, description = $1",
        "UPDATE positions SET summary = $1",
        "UPDATE citations SET quote = $1",
        "UPDATE code_commits SET author_name = $1, committer_name = $1, subject = $1,
                                message = $1, trailers = jsonb_build_object('Reviewed-by', ARRAY[$1])",
        "UPDATE code_line_instances SET line = $1, bytes = octet_length($1)",
    ] {
        indexer.execute(sql, &[&RAW]).await.unwrap();
    }
    let body = format!("{RAW}\n\tcaf\u{e9}");
    indexer
        .execute("UPDATE messages SET body = $1, body_clean = $1", &[&body])
        .await
        .unwrap();
    for sql in [
        "UPDATE code_lineages SET first_path = $1",
        "UPDATE code_file_versions SET path = $1",
        "UPDATE code_commit_files SET path = $1",
    ] {
        indexer.execute(sql, &[&PATH]).await.unwrap();
    }
    indexer
        .execute(
            "UPDATE code_file_versions SET size = $1",
            &[&i32::try_from(RAW.len() + 1).unwrap()],
        )
        .await
        .unwrap();
    db
}

async fn rpc(
    client: &reqwest::Client,
    base: &str,
    current: bool,
    capabilities: &Value,
    method: &str,
    mut params: Value,
) -> Value {
    let mut request = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    if current {
        request = request
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", method);
        if let Some(name) = params
            .get("name")
            .or_else(|| params.get("uri"))
            .and_then(Value::as_str)
        {
            request = request.header("mcp-name", name);
        }
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": capabilities,
        });
    }
    let response = request
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    let body = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map_or(text.as_str(), str::trim);
    serde_json::from_str(body).unwrap()
}

fn structured(response: &Value) -> &Value {
    let result = &response["result"];
    assert_ne!(result["isError"], true, "{response}");
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    assert_eq!(result["content"][0]["type"], "text");
    let decoded: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, result["structuredContent"], "{response}");
    &result["structuredContent"]
}

#[tokio::test]
async fn outbound_corpus_filter_covers_stored_data_without_rewriting_it() {
    let db = corpus().await;
    let (tools, _) = server(&db, None);
    let skills = tools::skills().unwrap();
    let skill = skills
        .get(tools::REVIEW_SKILL)
        .unwrap()
        .instructions()
        .to_owned();
    let server = McpServer::new(tools, "pghacker-mcp", "1", tools::INSTRUCTIONS)
        .with_skills(skills, tools::EXTENSION_INSTRUCTIONS)
        .unwrap();
    let base = listen(router(server, None)).await;
    let client = reqwest::Client::new();
    let body = format!("{CLEAN}\n\tcaf\u{e9}");

    for current in [false, true] {
        for declares_skills in [false, true] {
            let capabilities = if declares_skills {
                json!({ "extensions": { EXTENSION: {} } })
            } else {
                json!({})
            };
            let (method, params) = if current {
                ("server/discover", json!({}))
            } else {
                (
                    "initialize",
                    json!({
                        "protocolVersion": "2025-11-25", "capabilities": capabilities,
                        "clientInfo": { "name": "corpus-test", "version": "1" },
                    }),
                )
            };
            let info = rpc(&client, &base, current, &capabilities, method, params).await;
            let instructions = if declares_skills {
                tools::EXTENSION_INSTRUCTIONS
            } else {
                tools::INSTRUCTIONS
            };
            assert_eq!(info["result"]["instructions"], instructions, "{info}");

            for full_text in [false, true] {
                let response = rpc(&client, &base, current, &capabilities, "tools/call", json!({
                    "name": "get_thread", "arguments": { "thread_id": 1, "full_text": full_text },
                })).await;
                let result = structured(&response);
                assert_eq!(result["subject"], CLEAN);
                assert_eq!(result["messages"][0]["author"], CLEAN);
                assert_eq!(result["messages"][0]["text"], body);
                assert_eq!(result["messages"][0]["patches"][0]["filename"], CLEAN);
                assert_eq!(result["messages"][0]["patches"][0]["files"][0], CLEAN);
                assert_eq!(result["extracts"]["features"][0]["name"], CLEAN);
                assert_eq!(result["extracts"]["positions"][0]["summary"], CLEAN);
                assert_eq!(result["message_count"], 1);
            }

            let response = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "read_file", "arguments": { "path": PATH },
                }),
            )
            .await;
            let result = structured(&response);
            assert_eq!(result["path"], "src/fixture.c");
            assert_eq!(result["content"], format!("     1\t{CLEAN}\n"));
            assert_eq!(result["start_line"], 1);
            assert_eq!(result["end_line"], 1);
            assert_eq!(result["total_lines"], 1);
            assert_eq!(result["cut"], false);

            let response = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "get_commit", "arguments": { "commit": "master", "diff": false },
                }),
            )
            .await;
            let result = structured(&response);
            assert_eq!(result["subject"], CLEAN);
            assert_eq!(result["message"], CLEAN);
            assert_eq!(result["author"], CLEAN);
            assert_eq!(result["trailers"]["Reviewed-by"][0], CLEAN);
            assert_eq!(result["files"][0]["path"], "src/fixture.c");

            let response = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "grep", "arguments": { "command": "grep -r -n -F needle src" },
                }),
            )
            .await;
            assert_eq!(response["result"]["content"][0]["type"], "text");
            assert_eq!(
                response["result"]["content"][0]["text"],
                format!("src/fixture.c:1:{CLEAN}\n")
            );
            assert!(response["result"].get("structuredContent").is_none());

            let special = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "query", "arguments": {
                        "sql": "SELECT chr(27) AS \"\u{1b}\", 'No' || chr(8238) || 'ne' AS null_text, NULL::text AS null_value",
                    },
                }),
            )
            .await;
            assert_ne!(special["result"]["isError"], true, "{special}");
            assert_eq!(
                special["result"]["content"][0]["text"],
                "\"\",null_text,null_value\n\"\",\"None\",None\n\n(1 row)"
            );

            let limited = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "query", "arguments": {
                        "sql": "SELECT body FROM messages UNION ALL SELECT body FROM messages",
                        "max_rows": 1,
                    },
                }),
            )
            .await;
            assert_ne!(limited["result"]["isError"], true, "{limited}");
            assert_eq!(
                limited["result"]["content"][0]["text"],
                format!(
                    "Result has more than 1 rows, and is too large to return. \
                     Only returning the first 1 results.\n\nbody\n\"{body}\"\n\n(1 row)"
                )
            );
            let failed_query = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "query", "arguments": { "sql": "SELECT \"missing\u{1b}\u{202e}\"" },
                }),
            )
            .await;
            assert_eq!(failed_query["result"]["isError"], true, "{failed_query}");
            let error = failed_query["result"]["content"][0]["text"]
                .as_str()
                .unwrap();
            assert!(
                error.contains("column \"missing\" does not exist"),
                "{error:?}"
            );
            assert!(!error.contains(['\u{1b}', '\u{202e}']), "{error:?}");

            let response = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "query", "arguments": {
                        "sql": "SELECT body AS \"pay\u{202e}load\u{202c}\u{1b}\" FROM messages",
                    },
                }),
            )
            .await;
            assert_ne!(response["result"]["isError"], true, "{response}");
            assert_eq!(
                response["result"]["content"][0]["text"],
                format!("payload\n\"{body}\"\n\n(1 row)")
            );
            assert!(response["result"].get("structuredContent").is_none());

            let missing = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "get_thread", "arguments": { "message_id": "missing\u{1b}\u{202e}" },
                }),
            )
            .await;
            assert_eq!(missing["result"]["isError"], true, "{missing}");
            assert_eq!(
                missing["result"]["content"][0]["text"],
                "there's no message missing"
            );
            let rejected = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "tools/call",
                json!({
                    "name": "query", "arguments": { "sql": false },
                }),
            )
            .await;
            assert_eq!(rejected["error"]["code"], -32602, "{rejected}");

            let read = rpc(
                &client,
                &base,
                current,
                &capabilities,
                "resources/read",
                json!({
                    "uri": tools::REVIEW_SKILL,
                }),
            )
            .await;
            assert_eq!(read["result"]["contents"][0]["text"], skill);
            // Legacy requests after initialize carry no capabilities.
            if !current || !declares_skills {
                let loaded = rpc(
                    &client,
                    &base,
                    current,
                    &capabilities,
                    "tools/call",
                    json!({
                        "name": tools::REVIEW_TOOL, "arguments": {},
                    }),
                )
                .await;
                assert_eq!(loaded["result"]["content"][0]["text"], skill);
            }
        }
    }

    let indexer = db.indexer().await.unwrap();
    let stored = indexer
        .query_one(
            "SELECT (SELECT body FROM messages WHERE id = 1),
                    (SELECT content FROM patches LIMIT 1),
                    (SELECT line FROM code_line_instances WHERE id = 1),
                    (SELECT message FROM code_commits WHERE addr = 0),
                    (SELECT path FROM code_file_versions WHERE id = 1)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), format!("{RAW}\n\tcaf\u{e9}"));
    for column in 1..=3 {
        assert_eq!(stored.get::<_, String>(column), RAW);
    }
    assert_eq!(stored.get::<_, String>(4), PATH);
}
