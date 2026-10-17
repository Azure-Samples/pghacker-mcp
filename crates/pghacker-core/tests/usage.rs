// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Usage telemetry over HTTP, as the MCP server's hooks and the usage
//! middleware record requests: each MCP method, a call's arguments, outcome
//! and result's shape, the client as either protocol describes it, and the
//! requests turned away before the server sees them.

use std::net::IpAddr;

use axum::middleware;
use pghacker_core::mcp::{
    Effect, McpServer, Output, ToolSpec, Tools, failed, parse, router, schema,
};
use pghacker_core::usage::testing::Ingestion;
use pghacker_core::usage::{ACTION, ERROR, Endpoint, Recorder, Scope, Settings, Usage, record};
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
            "echo" => Output::Json(json!({ "echo": args.text, "mode": "fts", "cut": false })),
            _ => failed(anyhow::anyhow!("it failed: {}", args.text)),
        })
    }
}

/// A tool that records an event of its own, as query records the statements
/// it checked by their tokens.
#[derive(Clone)]
struct Noting;

static NOTING: &[ToolSpec] = &[ToolSpec {
    name: "note",
    description: "Notes text.",
    effect: Effect::Read,
    schema: schema::<EchoArgs>,
}];

impl Tools for Noting {
    fn specs(&self) -> &'static [ToolSpec] {
        NOTING
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Output, ErrorData> {
        self.call_with_usage(name, arguments, None).await
    }

    async fn call_with_usage(
        &self,
        name: &str,
        arguments: Value,
        scope: Option<Scope>,
    ) -> Result<Output, ErrorData> {
        let args: EchoArgs = parse(name, arguments)?;
        if let Some(scope) = scope {
            let mut event = scope.event("TestNote", name);
            event.property("text", &args.text);
            scope.record(event);
        }
        Ok(Output::Json(json!({ "noted": true })))
    }
}

/// A listener recording usage to `ingestion`, whose clients name their
/// address in X-Test-Client.
async fn serve(ingestion: &Ingestion) -> (String, Usage) {
    serve_tools(ingestion, Echo).await
}

/// [`serve`], with `tools`.
async fn serve_tools<T: Tools>(ingestion: &Ingestion, tools: T) -> (String, Usage) {
    let usage = Usage::new(Settings {
        endpoint: ingestion.url.clone(),
        instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
        id_key: b"test-usage-id-key-0123456789abcdef".to_vec(),
        role: "pghacker-mcp".into(),
        version: "0.1.0".into(),
        instance: Some("rev-1".into()),
    })
    .unwrap();
    let recorder = Recorder::new(usage.clone(), Endpoint::Public, |request| {
        request
            .headers()
            .get("x-test-client")?
            .to_str()
            .ok()?
            .parse::<IpAddr>()
            .ok()
    });
    let app = router(McpServer::new(tools, "echo", "1", "Echo tools."), None)
        .layer(middleware::from_fn_with_state(recorder, record));
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
    (format!("http://{addr}"), usage)
}

const CLIENT: &str = "203.0.113.9";

/// A request as an earlier client sends it.
async fn rpc(client: &reqwest::Client, base: &str, body: Value) -> u16 {
    client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .header("x-test-client", CLIENT)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// A request as a 2026-07-28 client sends it, describing itself in `_meta`.
async fn rpc_2026(client: &reqwest::Client, base: &str, method: &str, params: Value) -> u16 {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "modern", "version": "2.0" },
        "io.modelcontextprotocol/clientCapabilities": { "elicitation": {} },
    });
    let mut request = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method)
        .header("x-test-client", CLIENT);
    if let Some(name) = params["name"].as_str() {
        request = request.header("mcp-name", name);
    }
    request
        .json(&json!({ "jsonrpc": "2.0", "id": 9, "method": method, "params": params }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": arguments } })
}

fn properties(envelope: &Value) -> &Value {
    &envelope["data"]["baseData"]["properties"]
}

#[tokio::test]
async fn each_mcp_request_is_recorded_with_its_client_and_outcome() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = serve(&ingestion).await;
    let client = reqwest::Client::new();

    let initialize = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": { "roots": {} },
        "clientInfo": { "name": "legacy", "version": "1.0", "title": "Legacy Client" } } });
    assert_eq!(rpc(&client, &base, initialize).await, 200);
    let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    assert_eq!(rpc(&client, &base, list).await, 200);
    assert_eq!(
        rpc(&client, &base, call(3, "echo", json!({ "text": "hi" }))).await,
        200
    );
    assert_eq!(
        rpc(&client, &base, call(4, "fail", json!({ "text": "x" }))).await,
        200
    );
    assert_eq!(
        rpc(&client, &base, call(5, "nope", json!({ "q": 1 }))).await,
        200
    );
    assert_eq!(
        rpc(&client, &base, call(6, "echo", json!({ "wrong": 1 }))).await,
        200
    );
    assert_eq!(
        rpc_2026(&client, &base, "server/discover", json!({})).await,
        200
    );
    assert_eq!(
        rpc_2026(
            &client,
            &base,
            "tools/call",
            json!({ "name": "echo", "arguments": { "text": "now" } })
        )
        .await,
        200
    );
    // Pings and health checks aren't recorded.
    let ping = json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" });
    assert_eq!(rpc(&client, &base, ping).await, 200);
    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    usage.flush().await;
    let events = ingestion.received();
    let seen: Vec<(&str, &str, &str)> = events
        .iter()
        .map(|event| {
            let properties = properties(event);
            (
                properties["action"].as_str().unwrap(),
                properties["toolName"].as_str().unwrap_or(""),
                properties["errorType"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("McpInitialize", "", ""),
            ("McpToolsList", "", ""),
            ("McpToolCall", "echo", ""),
            ("McpToolCall", "fail", "ToolError"),
            ("McpToolCall", "nope", "UnknownTool"),
            ("McpToolCall", "echo", "InvalidParams"),
            ("McpDiscover", "", ""),
            ("McpToolCall", "echo", ""),
        ],
        "{events:#?}"
    );
    let [
        initialized,
        listed,
        echoed,
        failing,
        unknown,
        invalid,
        discovered,
        current,
    ] = &events[..]
    else {
        unreachable!()
    };

    for event in &events {
        assert_eq!(event["tags"]["ai.location.ip"], CLIENT);
        assert_eq!(
            event["tags"]["ai.user.id"],
            initialized["tags"]["ai.user.id"]
        );
        assert_eq!(event["tags"]["ai.cloud.role"], "pghacker-mcp");
        assert_eq!(event["tags"]["ai.cloud.roleInstance"], "rev-1");
        assert_eq!(properties(event)["endpoint"], "public");
        assert_eq!(properties(event)["common.extname"], "pghacker-mcp");
    }

    let legacy = properties(initialized);
    assert_eq!(legacy["clientName"], "legacy");
    assert_eq!(legacy["clientVersion"], "1.0");
    assert_eq!(legacy["clientTitle"], "Legacy Client");
    assert_eq!(legacy["protocolVersion"], "2025-06-18");
    assert_eq!(legacy["clientCapabilities"], r#"{"roots":{}}"#);
    // An earlier client's later requests carry only its protocol version.
    assert_eq!(properties(listed)["protocolVersion"], "2025-06-18");
    assert!(properties(listed).get("clientName").is_none());

    assert_eq!(echoed["data"]["baseData"]["name"], ACTION);
    let echo = properties(echoed);
    assert_eq!(echo["arg.text"], "hi");
    assert_eq!(echo["activityStatus"], "Succeeded");
    assert_eq!(echo["resultKind"], "json");
    assert_eq!(echo["result.mode"], "fts");
    assert_eq!(echo["result.cut"], "false");
    assert!(
        echo.get("result.echo").is_none(),
        "the result's content: {echo}"
    );
    let measurements = &echoed["data"]["baseData"]["measurements"];
    assert!(measurements["durationMs"].as_f64().unwrap() >= 0.0);
    assert!(measurements["responseBytes"].as_f64().unwrap() > 0.0);
    assert_eq!(measurements["argumentsLength"], 13.0);

    for event in [failing, unknown, invalid] {
        assert_eq!(event["data"]["baseData"]["name"], ERROR);
        assert_eq!(properties(event)["activityStatus"], "Failed");
    }
    assert_eq!(properties(failing)["errorMessage"], "it failed: x");
    assert_eq!(properties(unknown)["arg.q"], "1");
    assert!(
        properties(invalid)["errorMessage"]
            .as_str()
            .unwrap()
            .contains("arguments"),
        "{invalid}"
    );

    for event in [discovered, current] {
        let modern = properties(event);
        assert_eq!(modern["clientName"], "modern");
        assert_eq!(modern["clientVersion"], "2.0");
        assert_eq!(modern["protocolVersion"], "2026-07-28");
        assert_eq!(modern["clientCapabilities"], r#"{"elicitation":{}}"#);
    }
    assert_eq!(properties(current)["arg.text"], "now");
    assert_eq!(usage.lost(), 0);
}

#[tokio::test]
async fn requests_turned_away_are_recorded_once_a_minute_per_client() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = serve(&ingestion).await;
    let client = reqwest::Client::new();
    let unacceptable = || {
        client
            .post(format!("{base}/mcp"))
            .header("accept", "text/html")
            .header("content-type", "application/json")
            .header("x-test-client", CLIENT)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
            .send()
    };
    let first = unacceptable().await.unwrap().status();
    assert!(first.is_client_error(), "{first}");
    let again = unacceptable().await.unwrap().status();
    assert_eq!(again, first);
    // Another client is recorded too.
    let other = client
        .post(format!("{base}/mcp"))
        .header("accept", "text/html")
        .header("x-test-client", "198.51.100.7")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), first);

    usage.flush().await;
    let events = ingestion.received();
    assert_eq!(events.len(), 2, "{events:#?}");
    for event in &events {
        assert_eq!(event["data"]["baseData"]["name"], ERROR);
        assert_eq!(properties(event)["action"], "McpHttpRejected");
        assert_eq!(properties(event)["statusCode"], first.as_str());
        assert_eq!(properties(event)["httpMethod"], "POST");
    }
    assert_ne!(
        events[0]["tags"]["ai.user.id"],
        events[1]["tags"]["ai.user.id"]
    );
}

#[tokio::test]
async fn without_a_public_address_a_request_has_no_location_or_user() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = serve(&ingestion).await;
    let client = reqwest::Client::new();
    for address in ["127.0.0.1", "10.0.0.4"] {
        let answer = client
            .post(format!("{base}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("x-test-client", address)
            .json(&call(1, "echo", json!({ "text": "hi" })))
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 200);
    }
    usage.flush().await;
    let events = ingestion.received();
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(event["tags"]["ai.location.ip"], "0.0.0.0");
        assert!(event["tags"].get("ai.user.id").is_none(), "{event}");
        assert!(event["tags"].get("ai.session.id").is_none(), "{event}");
        assert!(event["tags"]["ai.operation.id"].is_string());
    }
}

#[tokio::test]
async fn a_failing_endpoint_costs_events_never_answers() {
    let ingestion = Ingestion::start().await;
    ingestion.answer(&[503, 503]);
    let (base, usage) = serve(&ingestion).await;
    let client = reqwest::Client::new();
    assert_eq!(
        rpc(&client, &base, call(1, "echo", json!({ "text": "hi" }))).await,
        200
    );
    usage.flush().await;
    assert!(ingestion.received().is_empty());
    assert_eq!(usage.lost(), 1);
}

#[tokio::test]
async fn a_tool_records_its_own_events_in_its_calls_operation() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = serve_tools(&ingestion, Noting).await;
    let client = reqwest::Client::new();
    assert_eq!(
        rpc(&client, &base, call(1, "note", json!({ "text": "hello" }))).await,
        200
    );
    usage.flush().await;

    let events = ingestion.received();
    let action = |event: &&Value| properties(event)["action"].clone();
    let note = events
        .iter()
        .find(|event| action(event) == "TestNote")
        .expect("the tool's event");
    let called = events
        .iter()
        .find(|event| action(event) == "McpToolCall")
        .expect("the call's event");
    assert_eq!(properties(note)["text"], "hello");
    assert_eq!(properties(note)["endpoint"], "public");
    for tag in [
        "ai.operation.id",
        "ai.operation.name",
        "ai.user.id",
        "ai.session.id",
    ] {
        assert!(note["tags"][tag].is_string(), "{tag}: {note}");
        assert_eq!(note["tags"][tag], called["tags"][tag], "{tag}");
    }
}
