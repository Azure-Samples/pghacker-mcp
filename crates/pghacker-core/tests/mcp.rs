//! The MCP plumbing over HTTP, as clients use it: a legacy client's
//! initialize then tools/list and tools/call without a session, structured
//! results, tool errors, unknown tools, Host validation, ping from clients of
//! either protocol, and calls whose clients give up.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
/// the method in headers, the version and capabilities in `_meta`.
async fn rpc_2026(
    client: &reqwest::Client,
    base: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let response = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method)
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
