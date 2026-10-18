// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The MCP plumbing over HTTP, as clients use it: a legacy client's
//! initialize then tools/list and tools/call without a session, structured
//! results, tool errors, unknown tools, Host validation, ping from clients of
//! either protocol, calls whose clients give up, and skills (SEP-2640) as
//! clients of either protocol find, read and check them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pghacker_core::mcp::{
    Effect, McpServer, Output, ToolSpec, Tools, failed, parse, router, schema,
};
use pghacker_core::skills::{EXTENSION, Skills, frontmatter};
use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}

#[derive(Clone)]
struct Echo;

static SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "echo",
        description: "Echoes text.",
        effect: Effect::Read,
        schema: schema::<EchoArgs>,
    },
    ToolSpec {
        name: "fail",
        description: "Fails.",
        effect: Effect::Read,
        schema: schema::<EchoArgs>,
    },
];

impl Tools for Echo {
    fn specs(&self) -> &'static [ToolSpec] {
        SPECS
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Output, ErrorData> {
        let args: EchoArgs = parse(name, arguments)?;
        Ok(match name {
            "echo" => Output::Json(json!({ "echo": args.text })),
            _ => failed(anyhow::anyhow!("it failed: {}", args.text)),
        })
    }
}

async fn serve(hosts: Option<Vec<String>>) -> String {
    serve_server(McpServer::new(Echo, "echo", "1", "Echo tools."), hosts).await
}

async fn serve_server<T: Tools>(server: McpServer<T>, hosts: Option<Vec<String>>) -> String {
    let app = router(server, hosts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

async fn rpc(client: &reqwest::Client, base: &str, body: Value) -> (u16, Value) {
    let response = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map_or(text.as_str(), str::trim)
        .to_owned();
    (
        status,
        serde_json::from_str(&json).unwrap_or(Value::String(text)),
    )
}

/// A request as a 2026-07-28 client sends it: no initialize, the version and
/// the method in headers (with the name, a tool's or a resource's URI, for
/// the methods that route by it), the version and capabilities in `_meta`.
async fn rpc_2026(
    client: &reqwest::Client,
    base: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    rpc_2026_as(client, base, id, method, params, json!({})).await
}

/// [`rpc_2026`] from a client with these `capabilities`.
async fn rpc_2026_as(
    client: &reqwest::Client,
    base: &str,
    id: u64,
    method: &str,
    params: Value,
    capabilities: Value,
) -> Value {
    let name = match method {
        "tools/call" | "prompts/get" => params.get("name"),
        "resources/read" => params.get("uri"),
        _ => None,
    }
    .and_then(Value::as_str)
    .map(str::to_owned);
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": capabilities,
    });
    let mut request = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if let Some(name) = name {
        request = request.header("mcp-name", name);
    }
    let response = request
        .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
        .send()
        .await
        .unwrap();
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[tokio::test]
async fn ping_is_answered_whichever_protocol_the_client_speaks() {
    let base = serve(None).await;
    let client = reqwest::Client::new();

    let (_, legacy) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
    )
    .await;
    assert_eq!(legacy["result"], json!({}), "{legacy}");

    // The 2026-07-28 protocol removed ping, but its clients may still send it.
    let current = rpc_2026(&client, &base, 2, "ping", json!({})).await;
    assert_eq!(
        current["result"],
        json!({ "resultType": "complete" }),
        "{current}"
    );

    // Only ping: other requests reach rmcp as they are.
    let listed = rpc_2026(&client, &base, 3, "tools/list", json!({})).await;
    assert_eq!(
        listed["result"]["tools"].as_array().map(Vec::len),
        Some(2),
        "{listed}"
    );
    let removed = rpc_2026(
        &client,
        &base,
        4,
        "logging/setLevel",
        json!({ "level": "info" }),
    )
    .await;
    assert_eq!(removed["error"]["code"], -32601, "{removed}");

    // rmcp still checks the ping it answers.
    let response = client
        .post(format!("{base}/mcp"))
        .header("host", "evil.example")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "ping")
        .json(&json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
}

#[tokio::test]
async fn a_legacy_client_initializes_lists_and_calls_without_a_session() {
    let base = serve(None).await;
    let client = Arc::new(reqwest::Client::new());
    let (status, init) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }),
    )
    .await;
    assert_eq!(status, 200, "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "echo");
    assert_eq!(init["result"]["instructions"], "Echo tools.");

    let (_, listed) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2, "{listed}");
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[0]["inputSchema"]["required"][0], "text");

    let (_, called) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } }),
    )
    .await;
    assert_eq!(
        called["result"]["structuredContent"]["echo"], "hi",
        "{called}"
    );
    assert_eq!(called["result"]["content"][0]["text"], "{\"echo\":\"hi\"}");

    let (_, failed) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "fail", "arguments": { "text": "x" } } }),
    )
    .await;
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(failed["result"]["content"][0]["text"], "it failed: x");

    let (_, unknown) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "nope", "arguments": {} } }),
    )
    .await;
    assert!(unknown["error"].is_object(), "{unknown}");

    let (_, bad) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "echo", "arguments": { "wrong": 1 } } }),
    )
    .await;
    assert!(
        bad["error"].is_object() || bad["result"]["isError"] == true,
        "{bad}"
    );

    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn hosts_outside_the_allowed_list_are_refused() {
    let base = serve(None).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/mcp"))
        .header("host", "evil.example")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NoArgs {}

/// A tool that never finishes, and says when its call is dropped.
#[derive(Clone)]
struct Hang {
    dropped: Arc<AtomicBool>,
}

static HANG: &[ToolSpec] = &[ToolSpec {
    name: "hang",
    description: "Never finishes.",
    effect: Effect::Read,
    schema: schema::<NoArgs>,
}];

struct OnDrop(Arc<AtomicBool>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Tools for Hang {
    fn specs(&self) -> &'static [ToolSpec] {
        HANG
    }

    async fn call(&self, _name: &str, _arguments: Value) -> Result<Output, ErrorData> {
        let _dropped = OnDrop(self.dropped.clone());
        std::future::pending::<()>().await;
        Ok(Output::Text(String::new()))
    }
}

/// Calls `hang` from a client that gives up after a moment; returns whether
/// the call was dropped within two seconds after.
async fn hang_dropped(cancel_on_disconnect: bool) -> bool {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut server = McpServer::new(
        Hang {
            dropped: dropped.clone(),
        },
        "hang",
        "1",
        "Hangs.",
    );
    if cancel_on_disconnect {
        server = server.cancel_on_disconnect();
    }
    let base = serve_server(server, None).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let answer = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "hang", "arguments": {} } }))
        .send()
        .await;
    assert!(answer.is_err(), "hang answered");
    for _ in 0..20 {
        if dropped.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn a_call_is_dropped_when_its_client_gives_up_if_the_server_cancels_on_disconnect() {
    assert!(hang_dropped(true).await, "the abandoned call ran on");
    assert!(
        !hang_dropped(false).await,
        "without cancel_on_disconnect, a call runs on as it always has"
    );
}

const SKILL: &str = "---\nname: demo\ndescription: Shows how; use when asked for a demo.\nlicense: MIT\nmetadata:\n  version: \"1\"\n---\n\n# Demo\n\nRead references/notes.md, then call echo.\n";
const NOTES: &str = "Notes for the demo.\n";
const SKILL_URI: &str = "skill://demo/SKILL.md";
const NOTES_URI: &str = "skill://demo/references/notes.md";
const SKILL_TOOL: &str = "load_skill_demo";
/// The server's instructions, for clients without the skills extension.
const TOOL_INSTRUCTIONS: &str = "Echo tools; for a demo, call load_skill_demo.";
/// The instructions for clients that declare the skills extension.
const EXTENSION_INSTRUCTIONS: &str = "Echo tools; for a demo, use the skill demo.";

fn demo_skills() -> Skills {
    Skills::from_files([
        ("demo/SKILL.md", SKILL.as_bytes().to_vec()),
        ("demo/references/notes.md", NOTES.as_bytes().to_vec()),
    ])
    .unwrap()
}

async fn serve_skills() -> String {
    let server = McpServer::new(Echo, "echo", "1", TOOL_INSTRUCTIONS)
        .with_skills(demo_skills(), EXTENSION_INSTRUCTIONS)
        .unwrap();
    serve_server(server, None).await
}

fn digest(text: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(text.as_bytes())))
}

/// The skill's entry, as skills/list and skills/get give it.
fn demo_entry() -> Value {
    json!({
        "uri": SKILL_URI,
        "frontmatter": { "name": "demo", "description": "Shows how; use when asked for a demo.",
                         "license": "MIT", "metadata": { "version": "1" } },
        "resources": [
            { "uri": SKILL_URI, "digest": digest(SKILL), "size": SKILL.len() },
            { "uri": NOTES_URI, "digest": digest(NOTES), "size": NOTES.len() },
        ],
    })
}

#[tokio::test]
async fn a_legacy_client_finds_reads_and_checks_the_skills() {
    let base = serve_skills().await;
    let client = reqwest::Client::new();
    let (_, init) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }),
    )
    .await;
    let capabilities = &init["result"]["capabilities"];
    assert_eq!(capabilities["extensions"][EXTENSION], json!({}), "{init}");
    assert!(capabilities["resources"].is_object(), "{init}");
    assert!(capabilities["tools"].is_object(), "{init}");

    let (_, listed) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "skills/list", "params": {} }),
    )
    .await;
    assert_eq!(
        listed["result"],
        json!({ "skills": [demo_entry()] }),
        "{listed}"
    );

    let (_, got) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "skills/get", "params": { "uri": SKILL_URI } }),
    )
    .await;
    assert_eq!(got["result"], json!({ "skill": demo_entry() }), "{got}");

    // A host reads each file and checks it against the entry.
    for (id, uri, text, mime) in [
        (4, SKILL_URI, SKILL, "text/markdown"),
        (5, NOTES_URI, NOTES, "text/markdown"),
    ] {
        let (_, read) = rpc(
            &client,
            &base,
            json!({ "jsonrpc": "2.0", "id": id, "method": "resources/read", "params": { "uri": uri } }),
        )
        .await;
        let contents = &read["result"]["contents"][0];
        assert_eq!(contents["uri"], uri, "{read}");
        assert_eq!(contents["mimeType"], mime);
        let served = contents["text"].as_str().unwrap();
        assert_eq!(served, text);
        assert_eq!(digest(served), digest(text));
        assert!(read["result"].get("resultType").is_none(), "{read}");
    }
    // The served frontmatter is the listed one.
    assert_eq!(
        Value::Object(frontmatter(SKILL).unwrap()),
        demo_entry()["frontmatter"]
    );

    let (_, resources) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "resources/list", "params": {} }),
    )
    .await;
    let resources = resources["result"]["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0]["uri"], SKILL_URI);
    assert_eq!(resources[0]["name"], "demo");
    assert_eq!(
        resources[0]["description"],
        "Shows how; use when asked for a demo."
    );
    assert_eq!(resources[0]["size"], SKILL.len());
    assert_eq!(resources[1]["name"], "references/notes.md");

    let (_, unknown) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 7, "method": "skills/get", "params": { "uri": "skill://nope/SKILL.md" } }),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let (_, unread) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 8, "method": "resources/read", "params": { "uri": "skill://demo/nope.md" } }),
    )
    .await;
    assert_eq!(unread["error"]["code"], -32002, "{unread}");
    let (_, paged) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 9, "method": "skills/list", "params": { "cursor": "x" } }),
    )
    .await;
    assert_eq!(paged["error"]["code"], -32602, "{paged}");
}

#[tokio::test]
async fn a_2026_client_discovers_and_reads_the_skills_with_complete_results() {
    let base = serve_skills().await;
    let client = reqwest::Client::new();
    let discovered = rpc_2026(&client, &base, 1, "server/discover", json!({})).await;
    assert_eq!(
        discovered["result"]["capabilities"]["extensions"][EXTENSION],
        json!({}),
        "{discovered}"
    );
    assert!(discovered["result"]["capabilities"]["resources"].is_object());

    let listed = rpc_2026(&client, &base, 2, "skills/list", json!({})).await;
    let mut expected = json!({ "skills": [demo_entry()] });
    expected["resultType"] = json!("complete");
    expected["ttlMs"] = json!(0);
    expected["cacheScope"] = json!("private");
    assert_eq!(listed["result"], expected, "{listed}");

    let got = rpc_2026(&client, &base, 3, "skills/get", json!({ "uri": SKILL_URI })).await;
    assert_eq!(
        got["result"],
        json!({ "resultType": "complete", "skill": demo_entry() }),
        "{got}"
    );

    let read = rpc_2026(
        &client,
        &base,
        4,
        "resources/read",
        json!({ "uri": NOTES_URI }),
    )
    .await;
    assert_eq!(read["result"]["resultType"], "complete", "{read}");
    assert_eq!(read["result"]["contents"][0]["text"], NOTES);

    let resources = rpc_2026(&client, &base, 5, "resources/list", json!({})).await;
    assert_eq!(
        resources["result"]["resources"].as_array().map(Vec::len),
        Some(2),
        "{resources}"
    );

    // Unknown skills and resources are invalid params on this protocol.
    let unknown = rpc_2026(&client, &base, 6, "skills/get", json!({ "uri": NOTES_URI })).await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let unread = rpc_2026(
        &client,
        &base,
        7,
        "resources/read",
        json!({ "uri": "skill://x/SKILL.md" }),
    )
    .await;
    assert_eq!(unread["error"]["code"], -32602, "{unread}");
}

#[tokio::test]
async fn a_server_without_skills_declares_none_and_knows_no_skills_methods() {
    let base = serve(None).await;
    let client = reqwest::Client::new();
    let discovered = rpc_2026(&client, &base, 1, "server/discover", json!({})).await;
    let capabilities = &discovered["result"]["capabilities"];
    assert!(capabilities["tools"].is_object(), "{discovered}");
    assert!(capabilities.get("extensions").is_none(), "{discovered}");
    assert!(capabilities.get("resources").is_none(), "{discovered}");
    let listed = rpc_2026(&client, &base, 2, "skills/list", json!({})).await;
    assert_eq!(listed["error"]["code"], -32601, "{listed}");
    let (_, legacy) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "skills/get", "params": { "uri": SKILL_URI } }),
    )
    .await;
    assert_eq!(legacy["error"]["code"], -32601, "{legacy}");
    let tools = rpc_2026(&client, &base, 4, "tools/list", json!({})).await;
    assert_eq!(tools_named(&tools), ["echo", "fail"], "{tools}");
}

/// The names of the tools a tools/list answer lists.
fn tools_named(listed: &Value) -> Vec<&str> {
    listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

/// A client's capabilities, with the skills extension or without.
fn capabilities(skills: bool) -> Value {
    if skills {
        json!({ "extensions": { EXTENSION: {} } })
    } else {
        json!({ "extensions": { "io.modelcontextprotocol/ui": {} } })
    }
}

#[tokio::test]
async fn a_client_without_the_extension_gets_a_tool_that_loads_the_skill() {
    let base = serve_skills().await;
    let client = reqwest::Client::new();

    // A 2026-07-28 client: the instructions name the tool, which tools/list
    // has, without arguments and with the skill's description.
    let discovered = rpc_2026_as(
        &client,
        &base,
        1,
        "server/discover",
        json!({}),
        capabilities(false),
    )
    .await;
    assert_eq!(
        discovered["result"]["instructions"], TOOL_INSTRUCTIONS,
        "{discovered}"
    );
    let listed = rpc_2026_as(
        &client,
        &base,
        2,
        "tools/list",
        json!({}),
        capabilities(false),
    )
    .await;
    assert_eq!(
        tools_named(&listed),
        ["echo", "fail", SKILL_TOOL],
        "{listed}"
    );
    let tool = &listed["result"]["tools"][2];
    assert_eq!(
        tool["inputSchema"],
        json!({ "type": "object", "properties": {} })
    );
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    let description = tool["description"].as_str().unwrap();
    assert!(
        description.contains("Shows how; use when asked for a demo."),
        "{description}"
    );
    assert!(!description.contains("skill://"), "{description}");

    // The tool returns SKILL.md: what resources/read gives, the listing's
    // digest.
    let called = rpc_2026_as(
        &client,
        &base,
        3,
        "tools/call",
        json!({ "name": SKILL_TOOL, "arguments": {} }),
        capabilities(false),
    )
    .await;
    assert_eq!(called["result"]["isError"], false, "{called}");
    let text = called["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, SKILL);
    assert_eq!(digest(text), demo_entry()["resources"][0]["digest"]);
    let read = rpc_2026_as(
        &client,
        &base,
        4,
        "resources/read",
        json!({ "uri": SKILL_URI }),
        capabilities(false),
    )
    .await;
    assert_eq!(read["result"]["contents"][0]["text"], text);

    // An earlier client: initialize's instructions name the tool, and its
    // requests after, which carry no capabilities, get it.
    let (_, init) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": capabilities(false),
            "clientInfo": { "name": "t", "version": "1" } } }),
    )
    .await;
    assert_eq!(init["result"]["instructions"], TOOL_INSTRUCTIONS, "{init}");
    let (_, listed) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(
        tools_named(&listed),
        ["echo", "fail", SKILL_TOOL],
        "{listed}"
    );
    let (_, called) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": SKILL_TOOL } }),
    )
    .await;
    assert_eq!(called["result"]["content"][0]["text"], SKILL, "{called}");
}

#[tokio::test]
async fn a_client_with_the_extension_gets_the_skill_only_through_it() {
    let base = serve_skills().await;
    let client = reqwest::Client::new();
    let discovered = rpc_2026_as(
        &client,
        &base,
        1,
        "server/discover",
        json!({}),
        capabilities(true),
    )
    .await;
    assert_eq!(
        discovered["result"]["instructions"], EXTENSION_INSTRUCTIONS,
        "{discovered}"
    );
    let listed = rpc_2026_as(
        &client,
        &base,
        2,
        "tools/list",
        json!({}),
        capabilities(true),
    )
    .await;
    assert_eq!(tools_named(&listed), ["echo", "fail"], "{listed}");
    let called = rpc_2026_as(
        &client,
        &base,
        3,
        "tools/call",
        json!({ "name": SKILL_TOOL, "arguments": {} }),
        capabilities(true),
    )
    .await;
    assert_eq!(called["error"]["code"], -32602, "{called}");
    // The extension, and the resource, serve the skill.
    let got = rpc_2026_as(
        &client,
        &base,
        4,
        "skills/get",
        json!({ "uri": SKILL_URI }),
        capabilities(true),
    )
    .await;
    assert_eq!(got["result"]["skill"], demo_entry(), "{got}");
    let read = rpc_2026_as(
        &client,
        &base,
        5,
        "resources/read",
        json!({ "uri": SKILL_URI }),
        capabilities(true),
    )
    .await;
    assert_eq!(read["result"]["contents"][0]["text"], SKILL, "{read}");

    // An earlier client that declares it at initialize gets these
    // instructions; its later requests carry no capabilities.
    let (_, init) = rpc(
        &client,
        &base,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": capabilities(true),
            "clientInfo": { "name": "t", "version": "1" } } }),
    )
    .await;
    assert_eq!(
        init["result"]["instructions"], EXTENSION_INSTRUCTIONS,
        "{init}"
    );
}

#[derive(Clone)]
struct Clash;

static CLASH: &[ToolSpec] = &[ToolSpec {
    name: SKILL_TOOL,
    description: "Has the demo skill's tool's name.",
    effect: Effect::Read,
    schema: schema::<NoArgs>,
}];

impl Tools for Clash {
    fn specs(&self) -> &'static [ToolSpec] {
        CLASH
    }

    async fn call(&self, _name: &str, _arguments: Value) -> Result<Output, ErrorData> {
        Ok(Output::Text(String::new()))
    }
}

#[test]
fn a_skill_whose_tool_another_tool_names_is_refused() {
    let refused = McpServer::new(Clash, "clash", "1", "Clashes.")
        .with_skills(demo_skills(), "Clashes.")
        .err()
        .unwrap();
    assert!(refused.to_string().contains(SKILL_TOOL), "{refused}");
}
