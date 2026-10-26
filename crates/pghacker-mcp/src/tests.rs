// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The listeners as `main` assembles them: the public one's Front Door
//! check, usage records, rate limit and feedback, and the internal one's
//! records; their deadline and panics; and the tools under load: a busy
//! pool, and a slow embeddings service.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::HeaderName;
use axum::routing::get;
use pghacker_core::calls::CallPool;
use pghacker_core::embed::{EmbedConfig, Embedder};
use pghacker_core::feedback::{Feedback, Settings as FeedbackSettings};
use pghacker_core::mcp::{Effect, McpServer, Output, ToolSpec, Tools, parse, router, schema};
use pghacker_core::testing::TestDb;
use pghacker_core::usage::testing::Ingestion;
use pghacker_core::usage::{ACTION, ERROR, Settings, Usage};
use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{guarded, internal, public};
use crate::ratelimit::{ClientIp, FrontDoor, RateLimit};
use crate::tools::Server;

#[derive(Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}

#[derive(Clone)]
struct Echo;

static SPECS: &[ToolSpec] = &[ToolSpec {
    name: "echo",
    description: "Echoes text.",
    effect: Effect::Read,
    schema: schema::<EchoArgs>,
}];

impl Tools for Echo {
    fn specs(&self) -> &'static [ToolSpec] {
        SPECS
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Output, ErrorData> {
        let args: EchoArgs = parse(name, arguments)?;
        Ok(Output::Json(json!({ "echo": args.text, "items": [1, 2] })))
    }
}

const FRONT_DOOR_ID: &str = "4f6f1d1e-8b3a-4c55-9a7e-0b5d2c9e7a11";

async fn listen(app: Router) -> String {
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

async fn post(client: &reqwest::Client, base: &str, headers: &[(&str, &str)], body: Value) -> u16 {
    let mut request = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.json(&body).send().await.unwrap().status().as_u16()
}

fn initialize() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": { "name": "test-client", "version": "1.0" } } })
}

fn call(text: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "text": text } } })
}

fn properties(envelope: &Value) -> &Value {
    &envelope["data"]["baseData"]["properties"]
}

#[tokio::test]
async fn the_listeners_record_their_clients_and_whom_they_turn_away() {
    let ingestion = Ingestion::start().await;
    let usage = Usage::new(Settings {
        endpoint: ingestion.url.clone(),
        instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
        id_key: b"test-usage-id-key-0123456789abcdef".to_vec(),
        role: "pghacker-mcp".into(),
        version: "0.1.0".into(),
        instance: None,
        details: true,
    })
    .unwrap();
    let server = McpServer::new(Echo, "echo", "1", "Echo.");
    let socket_ip = ClientIp::Header(HeaderName::from_static("x-azure-socketip"));
    // Two requests, then one a minute.
    let limit = RateLimit::new(1, 2, socket_ip.clone());
    let base = listen(public(
        router(server.clone(), None),
        limit,
        &usage,
        socket_ip,
        Some(FrontDoor(Arc::from(FRONT_DOOR_ID))),
    ))
    .await;
    let internal_base = listen(internal(router(server, None), &usage)).await;
    let client = reqwest::Client::new();
    let through_front_door = [
        ("x-azure-fdid", FRONT_DOOR_ID),
        ("x-azure-socketip", "203.0.113.9"),
        ("x-forwarded-for", "10.9.9.9"),
        ("user-agent", "test-client/1.0"),
    ];

    // Not through this Front Door: refused, and not recorded.
    let elsewhere = [("x-azure-socketip", "198.51.100.1")];
    assert_eq!(post(&client, &base, &elsewhere, initialize()).await, 403);
    assert_eq!(
        post(&client, &base, &through_front_door, initialize()).await,
        200
    );
    assert_eq!(
        post(&client, &base, &through_front_door, call("hi")).await,
        200
    );
    // Over the limit: recorded once a minute.
    assert_eq!(
        post(&client, &base, &through_front_door, call("more")).await,
        429
    );
    assert_eq!(
        post(&client, &base, &through_front_door, call("more")).await,
        429
    );
    // The internal listener: no limit, and no client address.
    let agent = [("x-azure-socketip", "203.0.113.9")];
    assert_eq!(
        post(&client, &internal_base, &agent, call("agent")).await,
        200
    );

    usage.flush().await;
    let events = ingestion.received();
    let seen: Vec<(&str, &str)> = events
        .iter()
        .map(|event| {
            let properties = properties(event);
            (
                properties["action"].as_str().unwrap(),
                properties["endpoint"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("McpInitialize", "public"),
            ("McpToolCall", "public"),
            ("McpRateLimited", "public"),
            ("McpToolCall", "internal"),
        ],
        "{events:#?}"
    );
    let [initialized, called, limited, agent_call] = &events[..] else {
        unreachable!()
    };

    // The client's address is only the location tag's; its user, a keyed
    // hash, and its session are the same across its requests.
    let user = initialized["tags"]["ai.user.id"].as_str().unwrap();
    assert_eq!(user.len(), 32);
    for event in [initialized, called, limited] {
        assert_eq!(event["tags"]["ai.location.ip"], "203.0.113.9");
        assert_eq!(event["tags"]["ai.user.id"], user);
        assert_eq!(
            event["tags"]["ai.session.id"],
            initialized["tags"]["ai.session.id"]
        );
        assert_eq!(
            event.to_string().matches("203.0.113.9").count(),
            1,
            "{event}"
        );
        assert!(!event.to_string().contains("10.9.9.9"), "{event}");
        assert_eq!(properties(event)["userAgent"], "test-client/1.0");
    }
    assert_eq!(properties(initialized)["clientName"], "test-client");
    assert_eq!(properties(initialized)["protocolVersion"], "2025-06-18");

    assert_eq!(called["data"]["baseData"]["name"], ACTION);
    assert_eq!(called["tags"]["ai.operation.name"], "echo");
    assert_eq!(properties(called)["toolName"], "echo");
    assert_eq!(properties(called)["arg.text"], "hi");
    assert_eq!(properties(called)["activityStatus"], "Succeeded");
    assert_eq!(
        called["data"]["baseData"]["measurements"]["result.items"],
        2.0
    );

    assert_eq!(limited["data"]["baseData"]["name"], ERROR);
    assert_eq!(properties(limited)["statusCode"], "429");

    assert_eq!(agent_call["tags"]["ai.location.ip"], "0.0.0.0");
    assert!(
        agent_call["tags"].get("ai.user.id").is_none(),
        "{agent_call}"
    );
    assert!(
        !agent_call.to_string().contains("203.0.113.9"),
        "{agent_call}"
    );
    assert_eq!(properties(agent_call)["arg.text"], "agent");
    assert_eq!(usage.lost(), 0);
}

#[tokio::test]
async fn reads_of_the_skills_are_recorded_with_their_uris() {
    let ingestion = Ingestion::start().await;
    let usage = Usage::new(Settings {
        endpoint: ingestion.url.clone(),
        instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
        id_key: b"test-usage-id-key-0123456789abcdef".to_vec(),
        role: "pghacker-mcp".into(),
        version: "0.1.0".into(),
        instance: None,
        details: true,
    })
    .unwrap();
    let server = McpServer::new(Echo, "echo", "1", "Echo.")
        .with_skills(
            crate::tools::skills().unwrap(),
            crate::tools::EXTENSION_INSTRUCTIONS,
        )
        .unwrap();
    let base = listen(internal(router(server, None), &usage)).await;
    let client = reqwest::Client::new();
    let uri = crate::tools::REVIEW_SKILL;
    let tool = crate::tools::REVIEW_TOOL;
    let requests = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "skills/list", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "skills/get", "params": { "uri": uri } }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": { "uri": uri } }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "skills/get", "params": { "uri": "skill://nope/SKILL.md" } }),
        // A client without the skills extension loads it with its tool.
        json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": tool, "arguments": {} } }),
    ];
    for request in requests {
        assert_eq!(post(&client, &base, &[], request).await, 200);
    }

    usage.flush().await;
    let events = ingestion.received();
    let seen: Vec<(&str, Option<&str>, &str)> = events
        .iter()
        .map(|event| {
            let properties = properties(event);
            (
                properties["action"].as_str().unwrap(),
                properties["uri"]
                    .as_str()
                    .or(properties["toolName"].as_str()),
                properties["activityStatus"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("McpSkillsList", None, "Succeeded"),
            ("McpSkillsGet", Some(uri), "Succeeded"),
            ("McpResourceRead", Some(uri), "Succeeded"),
            ("McpSkillsGet", Some("skill://nope/SKILL.md"), "Failed"),
            ("McpToolCall", Some(tool), "Succeeded"),
        ],
        "{events:#?}"
    );
    assert_eq!(events[0]["tags"]["ai.operation.name"], "skills/list");
    assert_eq!(usage.lost(), 0);
}

/// The JSON-RPC response to `method` from a 2026-07-28 Copilot CLI that can
/// show a form, with `headers`.
async fn ask_2026(
    client: &reqwest::Client,
    base: &str,
    headers: &[(&str, &str)],
    method: &str,
    params: Value,
) -> Value {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "copilot-cli", "version": "1.0.95" },
        "io.modelcontextprotocol/clientCapabilities": { "elicitation": { "form": {} } },
    });
    let mut request = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", method);
    if let Some(name) = params["name"].as_str() {
        request = request.header("mcp-name", name);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    request
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_public_listener_asks_for_feedback_and_the_internal_one_never_does() {
    let ingestion = Ingestion::start().await;
    let usage = Usage::new(Settings {
        endpoint: ingestion.url.clone(),
        instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
        id_key: b"test-usage-id-key-0123456789abcdef".to_vec(),
        role: "pghacker-mcp".into(),
        version: "0.1.0".into(),
        instance: None,
        details: true,
    })
    .unwrap();
    // Every hour armed, no warm-up, and questions apart after 100 ms.
    let feedback = Feedback::new(
        FeedbackSettings {
            rate: 1.0,
            question_gap: Duration::from_millis(100),
            session_gap: Duration::from_secs(30),
            min_questions: 0,
            ..FeedbackSettings::default()
        },
        b"test-usage-id-key-0123456789abcdef",
    )
    .unwrap();
    // As main assembles them.
    let server = McpServer::new(Echo, "echo", "1", "Echo.");
    let socket_ip = ClientIp::Header(HeaderName::from_static("x-azure-socketip"));
    let base = listen(public(
        router(server.clone().with_feedback(feedback), None),
        RateLimit::new(600, 100, socket_ip.clone()),
        &usage,
        socket_ip,
        Some(FrontDoor(Arc::from(FRONT_DOOR_ID))),
    ))
    .await;
    let internal_base = listen(internal(router(server, None), &usage)).await;
    let client = reqwest::Client::new();
    let through_front_door = [
        ("x-azure-fdid", FRONT_DOOR_ID),
        ("x-azure-socketip", "203.0.113.9"),
    ];
    let names = |listed: &Value| -> Vec<String> {
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let echo = json!({ "name": "echo", "arguments": { "text": "hi" } });
    let pause = || tokio::time::sleep(Duration::from_millis(250));

    // The tools are the same on both listeners.
    let listed = ask_2026(&client, &base, &through_front_door, "tools/list", json!({})).await;
    assert_eq!(names(&listed), ["echo"]);
    // The client the Front Door names: its first question runs; the next
    // one's first call is held for the form.
    let answered = ask_2026(
        &client,
        &base,
        &through_front_door,
        "tools/call",
        echo.clone(),
    )
    .await;
    assert_eq!(answered["result"]["resultType"], "complete", "{answered}");
    assert_eq!(
        answered["result"]["content"].as_array().map(Vec::len),
        Some(1),
        "{answered}"
    );
    pause().await;
    let asked = ask_2026(
        &client,
        &base,
        &through_front_door,
        "tools/call",
        echo.clone(),
    )
    .await;
    assert_eq!(asked["result"]["resultType"], "input_required", "{asked}");

    // The internal listener's agents: never held.
    let agent = [("x-azure-socketip", "203.0.113.10")];
    let listed = ask_2026(&client, &internal_base, &agent, "tools/list", json!({})).await;
    assert_eq!(names(&listed), ["echo"]);
    for _ in 0..3 {
        let answered = ask_2026(&client, &internal_base, &agent, "tools/call", echo.clone()).await;
        assert_eq!(answered["result"]["resultType"], "complete", "{answered}");
        pause().await;
    }

    usage.flush().await;
    let asks: Vec<Value> = ingestion
        .received()
        .into_iter()
        .filter(|event| properties(event)["action"] == "McpFeedbackAsked")
        .collect();
    assert_eq!(asks.len(), 1);
    assert_eq!(properties(&asks[0])["endpoint"], "public");
}

/// The served tools, each answering without running.
#[derive(Clone)]
struct Served;

impl Tools for Served {
    fn specs(&self) -> &'static [ToolSpec] {
        crate::tools::served()
    }

    async fn call(&self, _name: &str, _arguments: Value) -> Result<Output, ErrorData> {
        Ok(Output::Text("ok".into()))
    }
}

/// The served tools' fixed-choice arguments, each with one of its choices.
const CHOICES: &[(&str, &str, &str)] = &[
    ("search_threads", "mode", r#""hybrid""#),
    ("get_thread", "full_text", "true"),
    ("search_extracts", "kind", r#""decision""#),
    ("search_extracts", "mode", r#""semantic""#),
    ("author_perspective", "mode", r#""fulltext""#),
    ("search", "case_sensitive", "false"),
    ("search", "group_by", r#""file""#),
    ("search", "match", r#""identifier""#),
    ("search", "order", r#""coverage""#),
    ("search", "output", r#""counts""#),
    ("search", "whole_word", "true"),
    ("search_patches", "case_sensitive", "true"),
    ("blame", "ignore_revs", "false"),
    ("blame", "proposals", "true"),
    ("blame", "reverse", "false"),
    ("get_commit", "diff", "true"),
    ("search_commits", "ignore_case", "true"),
    ("search_commits", "occurrences_regex", "false"),
];

#[tokio::test]
async fn without_details_calls_of_the_served_tools_keep_only_their_choices() {
    const SECRET: &str = "what the user typed";
    let ingestion = Ingestion::start().await;
    let usage = Usage::new(Settings {
        endpoint: ingestion.url.clone(),
        instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
        id_key: b"test-usage-id-key-0123456789abcdef".to_vec(),
        role: "pghacker-mcp".into(),
        version: "0.1.0".into(),
        instance: None,
        details: false,
    })
    .unwrap();
    let server = McpServer::new(Served, "pghacker-mcp", "1", "Served.");
    let base = listen(internal(router(server, None), &usage)).await;
    let client = reqwest::Client::new();

    // Every argument of every tool: a choice where the schema offers some,
    // else text, and the choices again as text.
    let mut expected = std::collections::BTreeMap::new();
    for (id, spec) in crate::tools::served().iter().enumerate() {
        let schema = (spec.schema)();
        let names = schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut chosen = serde_json::Map::new();
        let mut typed = serde_json::Map::new();
        for name in &names {
            let choice = CHOICES
                .iter()
                .find(|(tool, argument, _)| *tool == spec.name && argument == name);
            let value = match choice {
                Some((_, _, value)) => serde_json::from_str(value).unwrap(),
                None => json!(SECRET),
            };
            chosen.insert(name.clone(), value);
            typed.insert(name.clone(), json!(SECRET));
        }
        let choices: std::collections::BTreeMap<String, String> = CHOICES
            .iter()
            .filter(|(tool, _, _)| *tool == spec.name)
            .map(|(_, argument, value)| {
                assert!(
                    names.contains(&argument.to_string()),
                    "{}.{argument}",
                    spec.name
                );
                let value: Value = serde_json::from_str(value).unwrap();
                let text = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                (format!("arg.{argument}"), text)
            })
            .collect();
        for (arguments, kept) in [(chosen, choices), (typed, Default::default())] {
            let request = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": spec.name, "arguments": arguments } });
            assert_eq!(post(&client, &base, &[], request).await, 200);
            usage.flush().await;
            let mut names = names.clone();
            names.sort();
            expected.insert(
                ingestion.received().len(),
                (spec.name, kept, Value::from(names).to_string()),
            );
        }
    }

    let events = ingestion.received();
    assert_eq!(events.len(), expected.len());
    for (count, (tool, kept, names)) in expected {
        let properties = properties(&events[count - 1]).as_object().unwrap();
        assert_eq!(properties["toolName"], tool);
        assert_eq!(properties["common.details"], "off");
        assert_eq!(properties["argumentNames"], names.as_str(), "{tool}");
        let recorded: std::collections::BTreeMap<String, String> = properties
            .iter()
            .filter(|(key, _)| key.starts_with("arg."))
            .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(recorded, kept, "{tool}");
        let event = events[count - 1].to_string();
        assert!(!event.contains(SECRET), "{event}");
    }
    assert_eq!(usage.lost(), 0);
}

#[tokio::test]
async fn a_stalled_request_is_answered_503_and_a_panic_500() {
    let app = Router::new()
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        )
        .route(
            "/panic",
            get(|| async {
                if Instant::now().elapsed() < Duration::from_secs(3600) {
                    panic!("a test panic");
                }
                "unreachable"
            }),
        );
    let base = listen(guarded(app, Duration::from_millis(300))).await;
    let client = reqwest::Client::new();
    let started = Instant::now();
    let slow = client.get(format!("{base}/slow")).send().await.unwrap();
    assert_eq!(slow.status().as_u16(), 503);
    assert!(started.elapsed() < Duration::from_secs(5));
    let panicked = client.get(format!("{base}/panic")).send().await.unwrap();
    assert_eq!(panicked.status().as_u16(), 500);
    // The listener serves on.
    let again = client.get(format!("{base}/panic")).send().await.unwrap();
    assert_eq!(again.status().as_u16(), 500);
}

/// The tools on `db` with a pool of one connection, waited for up to 300 ms.
fn server(db: &TestDb, embedder: Option<Embedder>) -> (Server, CallPool) {
    let pool = CallPool::new(db.reader_pool(1).unwrap(), Duration::from_millis(300));
    let query = pghacker_sqlquery::QueryTool::new(&db.reader_url, 1).unwrap();
    (Server::new(pool.clone(), embedder, query), pool)
}

#[tokio::test]
async fn a_call_fails_soon_as_busy_when_every_connection_is_taken() {
    let db = TestDb::create().await.unwrap();
    let (server, pool) = server(&db, None);
    let held = pool.get().await.unwrap();
    for (tool, arguments) in [
        ("search_authors", json!({ "query": "Tom" })),
        ("search_threads", json!({ "query": "vacuum" })),
        ("grep", json!({ "command": "grep -r vacuum src" })),
    ] {
        let started = Instant::now();
        let output = server.call(tool, arguments).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(3), "{tool}");
        match output {
            Output::Failed(text) => assert!(text.contains("busy; try again"), "{tool}: {text}"),
            _ => panic!("{tool} ran without a connection"),
        }
    }
    drop(held);
    let output = server
        .call("search_authors", json!({ "query": "Tom" }))
        .await
        .unwrap();
    assert!(matches!(output, Output::Json(_)));
}

#[tokio::test]
async fn a_slow_embeddings_service_turns_semantic_search_to_full_text_without_a_connection() {
    // An embeddings endpoint that takes connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (accepted, mut accepts) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
            let _ = accepted.send(());
        }
    });
    let embedder = Embedder::new(EmbedConfig {
        url,
        model: "test".into(),
        dim: pghacker_core::embed::DIMENSIONS,
        api_key: None,
        api_key_header: "api-key".into(),
        bearer: false,
    })
    .unwrap()
    .with_interactive_budget(Duration::from_millis(800));
    let db = TestDb::create().await.unwrap();
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO authors (id, display_name, message_count) VALUES (1, 'Tom Lane', 0)",
        )
        .await
        .unwrap();
    let (server, pool) = server(&db, Some(embedder));
    for (tool, arguments, results) in [
        ("search_threads", json!({ "query": "vacuum" }), "threads"),
        (
            "search_extracts",
            json!({ "query": "vacuum", "mode": "semantic" }),
            "results",
        ),
        (
            "author_perspective",
            json!({ "author": 1, "topic": "vacuum" }),
            "positions",
        ),
    ] {
        let started = Instant::now();
        let call = tokio::spawn({
            let server = server.clone();
            async move { server.call(tool, arguments).await }
        });
        accepts.recv().await.unwrap();
        // While the call waits on the endpoint, its connection is free.
        let free = pool.get().await.expect("the pool's one connection");
        drop(free);
        let output = call.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3), "{tool}");
        match output {
            Output::Json(value) => {
                assert_eq!(value["mode"], "fulltext", "{tool}: {value}");
                assert!(value[results].is_array(), "{tool}: {value}");
                let note = value["notes"][0].as_str().unwrap_or_default();
                assert!(
                    note.contains("semantic search is unavailable right now"),
                    "{tool}: {value}"
                );
            }
            Output::Failed(text) => panic!("{tool}: {text}"),
            Output::Text(text) => panic!("{tool}: {text}"),
        }
    }
}

#[tokio::test]
async fn a_database_failure_records_its_sqlstate_without_details() {
    let db = TestDb::create().await.unwrap();
    let pool = CallPool::new(
        pghacker_core::db::pool_with_options(
            &db.reader_url,
            1,
            "pghacker-test",
            "-c statement_timeout=100",
        )
        .unwrap(),
        Duration::from_millis(300),
    );
    let query = pghacker_sqlquery::QueryTool::new(&db.reader_url, 1).unwrap();
    let server = McpServer::new(
        Server::new(pool, None, query),
        "pghacker-mcp",
        "1",
        "Served.",
    );
    let ingestion = Ingestion::start().await;
    let usage = Usage::new(Settings {
        endpoint: ingestion.url.clone(),
        instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
        id_key: b"test-usage-id-key-0123456789abcdef".to_vec(),
        role: "pghacker-mcp".into(),
        version: "0.1.0".into(),
        instance: None,
        details: false,
    })
    .unwrap();
    let base = listen(internal(router(server, None), &usage)).await;
    let mut indexer = db.indexer().await.unwrap();
    let tx = indexer.transaction().await.unwrap();
    tx.batch_execute("LOCK TABLE authors IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "author_perspective",
            "arguments": { "author": "Tom Lane", "topic": "vacuum", "mode": "fulltext" }
        }
    });
    assert_eq!(post(&client, &base, &[], request).await, 200);
    drop(tx);
    usage.flush().await;

    let events = ingestion.received();
    assert_eq!(events.len(), 1, "{events:#?}");
    let props = properties(&events[0]);
    assert_eq!(props["toolName"], "author_perspective");
    assert_eq!(props["activityStatus"], "Failed");
    assert_eq!(props["errorType"], "ToolError");
    assert_eq!(props["errorCode"], "57014");
    assert!(props.get("errorMessage").is_none(), "{props}");
}
