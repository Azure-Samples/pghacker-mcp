//! The MCP plumbing over HTTP, as clients use it: a legacy client's
//! initialize then tools/list and tools/call without a session, structured
//! results, tool errors, unknown tools and Host validation.

use std::sync::Arc;

use pghacker_core::mcp::{
    Effect, McpServer, Output, ToolSpec, Tools, failed, parse, router, schema,
};
use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::{Value, json};

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
    let app = router(McpServer::new(Echo, "echo", "1", "Echo tools."), hosts);
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
