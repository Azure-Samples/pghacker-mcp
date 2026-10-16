// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Feedback over HTTP, as clients meet it: a note after a sampled answer for
//! a client that declares form elicitation, `user_feedback`'s form
//! (`input_required`) and the retry that records the user's choice, the
//! clients never asked, ids bound to their client, `?feedback=off`, and the
//! usage events of each step. One test drives rmcp's own client through the
//! round trip.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::middleware;
use pghacker_core::feedback::{Feedback, Settings as FeedbackSettings};
use pghacker_core::mcp::{
    Effect, McpServer, Output, ToolSpec, Tools, failed, parse, router, schema,
};
use pghacker_core::usage::testing::Ingestion;
use pghacker_core::usage::{Endpoint, Recorder, Settings, Usage, record};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
    ElicitationAction, Implementation, ProtocolVersion,
};
use rmcp::service::{RequestContext, RoleClient};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData};
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
        description: "Echoes text as JSON.",
        effect: Effect::Read,
        schema: schema::<EchoArgs>,
    },
    ToolSpec {
        name: "lines",
        description: "Echoes text as text.",
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
            "lines" => Output::Text(format!("{}\n", args.text)),
            _ => failed(anyhow::anyhow!("it failed: {}", args.text)),
        })
    }
}

const ID_KEY: &[u8] = b"test-usage-id-key-0123456789abcdef";
/// The address of a client that names none, such as rmcp's.
const CLIENT: &str = "203.0.113.9";
const FORM: &str = "pghacker-mcp asks: was the last answer helpful?";

fn feedback(rate: f64, comments: bool) -> Feedback {
    Feedback::new(FeedbackSettings { rate, comments }, ID_KEY).unwrap()
}

fn server(feedback: Feedback) -> McpServer<Echo> {
    McpServer::new(Echo, "echo", "1", "Echo tools.").with_feedback(feedback)
}

/// A listener for `server`, recording usage to `ingestion` unless that's
/// `None`. Clients name their address in X-Test-Client, or are [`CLIENT`].
async fn listen(server: McpServer<Echo>, ingestion: Option<&Ingestion>) -> (String, Usage) {
    let mut app = router(server, None);
    let usage = match ingestion {
        Some(ingestion) => Usage::new(Settings {
            endpoint: ingestion.url.clone(),
            instrumentation_key: "11111111-2222-4333-8444-555555555555".into(),
            id_key: ID_KEY.to_vec(),
            role: "pghacker-mcp".into(),
            version: "0.1.0".into(),
            instance: None,
        })
        .unwrap(),
        None => Usage::off(),
    };
    if usage.is_on() {
        let recorder = Recorder::new(usage.clone(), Endpoint::Public, |request| {
            let named = request
                .headers()
                .get("x-test-client")
                .and_then(|value| value.to_str().ok())
                .unwrap_or(CLIENT);
            named.parse::<IpAddr>().ok()
        });
        app = app.layer(middleware::from_fn_with_state(recorder, record));
    }
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

/// A 2026-07-28 client at `address`, declaring `capabilities`, that posts to
/// `path`.
struct Client {
    http: reqwest::Client,
    base: String,
    address: &'static str,
    capabilities: Value,
    path: &'static str,
}

impl Client {
    fn new(base: &str, address: &'static str, capabilities: Value) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.to_owned(),
            address,
            capabilities,
            path: "/mcp",
        }
    }

    fn at(mut self, path: &'static str) -> Self {
        self.path = path;
        self
    }

    /// The JSON-RPC response to `method` with `params`.
    async fn request(&self, id: u64, method: &str, params: Value) -> Value {
        let mut params = params;
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "test-client", "version": "1.0" },
            "io.modelcontextprotocol/clientCapabilities": self.capabilities,
        });
        let mut request = self
            .http
            .post(format!("{}{}", self.base, self.path))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", method)
            .header("x-test-client", self.address);
        if let Some(name) = params["name"].as_str() {
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

    /// A tool call, with `responses` to a form on a retry.
    async fn call(&self, id: u64, name: &str, arguments: Value, responses: Option<Value>) -> Value {
        let mut params = json!({ "name": name, "arguments": arguments });
        if let Some(responses) = responses {
            params["inputResponses"] = responses;
        }
        self.request(id, "tools/call", params).await
    }
}

/// The capabilities of a client that can show a form.
fn form_capable() -> Value {
    json!({ "elicitation": { "form": {} } })
}

/// The feedback id in a result's note, if it has one.
fn note(response: &Value) -> Option<String> {
    response["result"]["content"]
        .as_array()?
        .iter()
        .find_map(|block| {
            let text = block["text"].as_str()?;
            let start = text.find("feedback_id: ")? + "feedback_id: ".len();
            Some(text[start..].split_whitespace().next()?.to_owned())
        })
}

fn text_of(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

fn properties(envelope: &Value) -> &Value {
    &envelope["data"]["baseData"]["properties"]
}

fn action(envelope: &Value) -> &str {
    properties(envelope)["action"].as_str().unwrap_or_default()
}

/// The events of `actions`, in order.
fn events<'a>(events: &'a [Value], actions: &[&str]) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| actions.contains(&action(event)))
        .collect()
}

fn answer(rating: &str) -> Value {
    json!({ "feedback": { "action": "accept", "content": { "rating": rating } } })
}

#[tokio::test]
async fn a_client_that_can_show_a_form_is_asked_after_an_answer_and_its_choice_recorded() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, false)), Some(&ingestion)).await;
    let client = Client::new(&base, CLIENT, form_capable());

    // The answer, as it was, then the note for the model.
    let answered = client.call(1, "echo", json!({ "text": "hi" }), None).await;
    let result = &answered["result"];
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "{answered}");
    assert_eq!(result["structuredContent"], json!({ "echo": "hi" }));
    let first: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(first, json!({ "echo": "hi" }));
    assert_eq!(content[1]["annotations"]["audience"], json!(["assistant"]));
    let id = note(&answered).expect("a note");
    // An hour has one: the next answer has none.
    let again = client
        .call(2, "lines", json!({ "text": "again" }), None)
        .await;
    assert_eq!(
        again["result"]["content"].as_array().unwrap().len(),
        1,
        "{again}"
    );

    // The first call answers with the form.
    let asked = client
        .call(3, "user_feedback", json!({ "feedback_id": id }), None)
        .await;
    let result = &asked["result"];
    assert_eq!(result["resultType"], "input_required", "{asked}");
    assert!(result.get("requestState").is_none());
    let form = &result["inputRequests"]["feedback"];
    assert_eq!(form["method"], "elicitation/create");
    assert_eq!(form["params"]["mode"], "form");
    assert!(
        form["params"]["message"]
            .as_str()
            .unwrap()
            .starts_with(FORM)
    );
    let schema = &form["params"]["requestedSchema"];
    assert_eq!(schema["required"], json!(["rating"]));
    assert_eq!(
        schema["properties"].as_object().unwrap().len(),
        1,
        "{schema}"
    );
    let rating = &schema["properties"]["rating"];
    assert_eq!(rating["default"], "skip");
    let choices: Vec<&str> = rating["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice["const"].as_str().unwrap())
        .collect();
    assert_eq!(choices, ["up", "down", "skip"]);

    // The retry carries the user's choice, recorded once.
    let recorded = client
        .call(
            4,
            "user_feedback",
            json!({ "feedback_id": id }),
            Some(answer("up")),
        )
        .await;
    assert_eq!(recorded["result"]["resultType"], "complete", "{recorded}");
    assert_eq!(text_of(&recorded), "Thanks: your rating was recorded.");
    let repeated = client
        .call(
            5,
            "user_feedback",
            json!({ "feedback_id": id }),
            Some(answer("down")),
        )
        .await;
    assert_eq!(text_of(&repeated), "Nothing more to record.", "{repeated}");

    usage.flush().await;
    let received = ingestion.received();
    let seen: Vec<(&str, &str, &str)> = events(
        &received,
        &["McpToolCall", "McpFeedbackOffered", "McpFeedback"],
    )
    .into_iter()
    .map(|event| {
        let properties = properties(event);
        (
            action(event),
            properties["toolName"].as_str().unwrap_or_default(),
            properties["activityStatus"]
                .as_str()
                .or(properties["outcome"].as_str())
                .unwrap_or_default(),
        )
    })
    .collect();
    assert_eq!(
        seen,
        [
            ("McpToolCall", "echo", "Succeeded"),
            ("McpFeedbackOffered", "echo", ""),
            ("McpToolCall", "lines", "Succeeded"),
            ("McpToolCall", "user_feedback", "InputRequired"),
            ("McpToolCall", "user_feedback", "Succeeded"),
            ("McpFeedback", "", "rated"),
            ("McpToolCall", "user_feedback", "Succeeded"),
        ],
        "{received:#?}"
    );
    let calls = events(&received, &["McpToolCall"]);
    let offered = events(&received, &["McpFeedbackOffered"])[0];
    let rated = events(&received, &["McpFeedback"])[0];
    // The offer shares its answer's operation, and names the id the outcome names.
    assert_eq!(
        offered["tags"]["ai.operation.id"],
        calls[0]["tags"]["ai.operation.id"]
    );
    let feedback_id = properties(offered)["feedbackId"].as_str().unwrap();
    assert_eq!(feedback_id.len(), 32);
    assert_eq!(properties(rated)["feedbackId"], feedback_id);
    assert_eq!(properties(rated)["rating"], "up");
    assert!(properties(rated).get("comment").is_none());
    assert_eq!(properties(rated)["clientName"], "test-client");
    assert!(
        rated["tags"]["ai.user.id"]
            .as_str()
            .is_some_and(|user| user.len() == 32)
    );
    // The call records the id it was given, and no rating.
    assert_eq!(properties(calls[2])["arg.feedback_id"], id.as_str());
    assert!(properties(calls[3]).get("arg.rating").is_none());
}

#[tokio::test]
async fn each_answer_is_recorded_as_what_the_user_chose() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, false)), Some(&ingestion)).await;
    let cases: [(&'static str, Value, &str); 6] = [
        (
            "198.51.100.1",
            answer("down"),
            "Thanks: your rating was recorded.",
        ),
        ("198.51.100.2", answer("skip"), "Okay: no rating was given."),
        (
            "198.51.100.3",
            json!({ "feedback": { "action": "decline" } }),
            "Okay: no rating was given.",
        ),
        (
            "198.51.100.4",
            json!({ "feedback": { "action": "cancel" } }),
            "Okay: no rating was given.",
        ),
        ("198.51.100.5", json!({}), "Okay: no rating was given."),
        (
            "198.51.100.6",
            json!({ "feedback": "garbage" }),
            "Okay: no rating was given.",
        ),
    ];
    for (n, (address, responses, expected)) in cases.into_iter().enumerate() {
        let client = Client::new(&base, address, form_capable());
        let id = note(&client.call(1, "lines", json!({ "text": "x" }), None).await)
            .unwrap_or_else(|| panic!("a note for case {n}"));
        let answered = client
            .call(
                2,
                "user_feedback",
                json!({ "feedback_id": id }),
                Some(responses),
            )
            .await;
        assert_eq!(text_of(&answered), expected, "case {n}: {answered}");
    }
    usage.flush().await;
    let received = ingestion.received();
    let outcomes: Vec<(&str, &str)> = events(&received, &["McpFeedback"])
        .into_iter()
        .map(|event| {
            let properties = properties(event);
            (
                properties["outcome"].as_str().unwrap(),
                properties["rating"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            ("rated", "down"),
            ("dismissed", ""),
            ("declined", ""),
            ("dismissed", ""),
            ("dismissed", ""),
            ("dismissed", ""),
        ]
    );
}

#[tokio::test]
async fn clients_that_cannot_show_a_form_are_never_asked() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, false)), Some(&ingestion)).await;
    for capabilities in [json!({}), json!({ "elicitation": { "url": {} } })] {
        let client = Client::new(&base, CLIENT, capabilities.clone());
        let answered = client.call(1, "echo", json!({ "text": "x" }), None).await;
        assert_eq!(note(&answered), None, "{capabilities}: {answered}");
    }
    // An earlier client declares elicitation in initialize, which nothing
    // keeps, and sends no capabilities with a call.
    let http = reqwest::Client::new();
    let legacy = |body: Value| {
        http.post(format!("{base}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2025-06-18")
            .header("x-test-client", "198.51.100.9")
            .json(&body)
            .send()
    };
    let initialize = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": { "elicitation": {} },
        "clientInfo": { "name": "legacy", "version": "1.0" } } });
    assert_eq!(legacy(initialize).await.unwrap().status(), 200);
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "text": "x" } } });
    let called: Value = legacy(call).await.unwrap().json().await.unwrap();
    assert_eq!(
        called["result"]["content"].as_array().map(Vec::len),
        Some(1),
        "{called}"
    );

    // `elicitation: {}` is form mode: asked. Its id, on a request that can't
    // show the form, records nothing.
    let client = Client::new(&base, CLIENT, json!({ "elicitation": {} }));
    let id = note(&client.call(3, "echo", json!({ "text": "x" }), None).await).expect("a note");
    let cannot = Client::new(&base, CLIENT, json!({}));
    let answered = cannot
        .call(4, "user_feedback", json!({ "feedback_id": id }), None)
        .await;
    assert_eq!(
        text_of(&answered),
        "This client can't show the prompt, so no feedback was recorded.",
        "{answered}"
    );
    // A failed call gets no note.
    let other = Client::new(&base, "198.51.100.10", form_capable());
    let failed = other.call(5, "fail", json!({ "text": "x" }), None).await;
    assert_eq!(failed["result"]["isError"], true, "{failed}");
    assert_eq!(note(&failed), None);
    usage.flush().await;
    assert!(events(&ingestion.received(), &["McpFeedback"]).is_empty());
}

#[tokio::test]
async fn ids_are_bound_to_their_client_and_cannot_be_forged() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, false)), Some(&ingestion)).await;
    let owner = Client::new(&base, CLIENT, form_capable());
    let id = note(&owner.call(1, "echo", json!({ "text": "x" }), None).await).expect("a note");
    let stranger = Client::new(&base, "198.51.100.20", form_capable());

    let invalid = "This feedback id isn't valid (it may have expired).";
    let long = format!("{id}{}", "A".repeat(200));
    for (n, attempt) in [id.as_str(), "fb1.forged", long.as_str()]
        .into_iter()
        .enumerate()
    {
        let refused = stranger
            .call(
                2 + n as u64,
                "user_feedback",
                json!({ "feedback_id": attempt }),
                Some(answer("up")),
            )
            .await;
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        assert_eq!(text_of(&refused), invalid);
    }
    // Arguments other than the id are the caller's mistake.
    for bad in [json!({}), json!({ "feedback_id": id, "rating": "up" })] {
        let error = stranger.call(9, "user_feedback", bad, None).await;
        assert_eq!(error["error"]["code"], -32602, "{error}");
    }
    // The owner still can.
    let asked = owner
        .call(10, "user_feedback", json!({ "feedback_id": id }), None)
        .await;
    assert_eq!(asked["result"]["resultType"], "input_required", "{asked}");

    usage.flush().await;
    let received = ingestion.received();
    // One rejection a minute per client.
    let rejected = events(&received, &["McpFeedbackRejected"]);
    assert_eq!(rejected.len(), 1, "{received:#?}");
    assert_eq!(properties(rejected[0])["reason"], "invalid");
    assert_eq!(
        rejected[0]["data"]["baseData"]["name"],
        pghacker_core::usage::ERROR
    );
    assert!(events(&received, &["McpFeedback"]).is_empty());
}

#[tokio::test]
async fn feedback_off_in_the_url_stops_offers_and_answers() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, false)), Some(&ingestion)).await;
    let off = Client::new(&base, CLIENT, form_capable()).at("/mcp?feedback=off");
    let answered = off.call(1, "echo", json!({ "text": "x" }), None).await;
    assert_eq!(
        answered["result"]["structuredContent"],
        json!({ "echo": "x" })
    );
    assert_eq!(note(&answered), None, "{answered}");

    // An id from the plain URL records nothing through the opted-out one.
    let on = Client::new(&base, CLIENT, form_capable());
    let id = note(&on.call(2, "echo", json!({ "text": "x" }), None).await).expect("a note");
    for responses in [None, Some(answer("up"))] {
        let answered = off
            .call(3, "user_feedback", json!({ "feedback_id": id }), responses)
            .await;
        assert_eq!(
            text_of(&answered),
            "Feedback is off for this connection; no feedback was recorded.",
            "{answered}"
        );
    }
    usage.flush().await;
    assert!(events(&ingestion.received(), &["McpFeedback"]).is_empty());
}

#[tokio::test]
async fn the_tool_and_its_instructions_come_only_with_feedback_on() {
    let ingestion = Ingestion::start().await;
    let plain = McpServer::new(Echo, "echo", "1", "Echo tools.");
    // As main serves the internal listener: the same server, without feedback.
    let (internal, _) = listen(plain.clone(), Some(&ingestion)).await;
    let (public, _) = listen(plain.with_feedback(feedback(0.1, false)), Some(&ingestion)).await;

    let tools = |listed: &Value| -> Vec<String> {
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let client = Client::new(&public, CLIENT, form_capable());
    let listed = client.request(1, "tools/list", json!({})).await;
    assert_eq!(tools(&listed), ["echo", "lines", "fail", "user_feedback"]);
    let tool = &listed["result"]["tools"][3];
    assert_eq!(
        tool["annotations"],
        json!({ "readOnlyHint": false, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false })
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["feedback_id"]));
    let discovered = client.request(2, "server/discover", json!({})).await;
    let instructions = discovered.to_string();
    assert!(instructions.contains("Echo tools."), "{discovered}");
    assert!(
        instructions.contains("call user_feedback once"),
        "{discovered}"
    );

    let inside = Client::new(&internal, CLIENT, form_capable());
    let listed = inside.request(3, "tools/list", json!({})).await;
    assert_eq!(tools(&listed), ["echo", "lines", "fail"]);
    let discovered = inside.request(4, "server/discover", json!({})).await;
    assert!(
        !discovered.to_string().contains("user_feedback"),
        "{discovered}"
    );
    // Without feedback, the tool is unknown, and answers have no note.
    let unknown = inside
        .call(5, "user_feedback", json!({ "feedback_id": "fb1.x" }), None)
        .await;
    assert!(
        unknown.to_string().contains("unknown tool user_feedback"),
        "{unknown}"
    );
    let (base, _) = listen(server(Feedback::off()), Some(&ingestion)).await;
    let answered = Client::new(&base, CLIENT, form_capable())
        .call(6, "echo", json!({ "text": "x" }), None)
        .await;
    assert_eq!(note(&answered), None);
}

#[tokio::test]
async fn requests_without_an_identity_are_never_asked() {
    let (base, _) = listen(server(feedback(1.0, false)), None).await;
    let client = Client::new(&base, CLIENT, form_capable());
    let answered = client.call(1, "echo", json!({ "text": "x" }), None).await;
    assert_eq!(note(&answered), None, "{answered}");
    let refused = client
        .call(2, "user_feedback", json!({ "feedback_id": "fb1.x" }), None)
        .await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(text_of(&refused), "Feedback isn't available here.");
}

#[tokio::test]
async fn with_comments_on_the_form_takes_one_after_the_rating() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, true)), Some(&ingestion)).await;
    let client = Client::new(&base, CLIENT, form_capable());
    let id = note(&client.call(1, "echo", json!({ "text": "x" }), None).await).expect("a note");
    let response = client
        .http
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", "user_feedback")
        .header("x-test-client", CLIENT)
        .json(
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "user_feedback", "arguments": { "feedback_id": id },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": form_capable(),
            } } }),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Clients show fields in the order they arrive: the rating first.
    let rating = response.find("\"rating\":").expect("a rating field");
    let remarks = response.find("\"remarks\":").expect("a comment field");
    assert!(rating < remarks, "{response}");

    let answered = client
        .call(
            3,
            "user_feedback",
            json!({ "feedback_id": id }),
            Some(json!({ "feedback": { "action": "accept", "content": {
                "rating": "down", "remarks": "cited the wrong\nthread " } } })),
        )
        .await;
    assert_eq!(text_of(&answered), "Thanks: your rating was recorded.");
    usage.flush().await;
    let received = ingestion.received();
    let rated = events(&received, &["McpFeedback"]);
    assert_eq!(properties(rated[0])["comment"], "cited the wrong thread");
    assert_eq!(
        rated[0]["data"]["baseData"]["measurements"]["commentLength"],
        22.0
    );
    // The comment comes from the form, never from the call's arguments.
    for call in events(&received, &["McpToolCall"]) {
        assert!(properties(call).get("arg.remarks").is_none());
    }
}

/// A person at an rmcp client, who answers the form 👎.
#[derive(Clone, Default)]
struct Person {
    asked: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for Person {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_elicitation().build(),
            Implementation::new("person", "1.0"),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        if let ElicitRequestParams::FormElicitationParams { message, .. } = &request {
            self.asked.lock().unwrap().push(message.clone());
        }
        Ok(ElicitResult::new(ElicitationAction::Accept).with_content(json!({ "rating": "down" })))
    }
}

#[tokio::test]
async fn rmcps_own_client_completes_the_round_trip() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(feedback(1.0, false)), Some(&ingestion)).await;
    let person = Person::default();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")),
    );
    let client = person
        .clone()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("the client starts");
    let arguments = |value: Value| value.as_object().unwrap().clone();
    let answered = client
        .call_tool(
            CallToolRequestParams::new("echo").with_arguments(arguments(json!({ "text": "hi" }))),
        )
        .await
        .unwrap();
    let id = answered
        .content
        .iter()
        .find_map(|block| {
            let text = &block.as_text()?.text;
            let start = text.find("feedback_id: ")? + "feedback_id: ".len();
            Some(text[start..].split_whitespace().next()?.to_owned())
        })
        .expect("a note");
    let rated = client
        .call_tool(
            CallToolRequestParams::new("user_feedback")
                .with_arguments(arguments(json!({ "feedback_id": id }))),
        )
        .await
        .unwrap();
    assert_eq!(
        rated.content[0].as_text().unwrap().text,
        "Thanks: your rating was recorded."
    );
    let asked = person.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].starts_with(FORM), "{asked:?}");
    client.cancel().await.unwrap();

    usage.flush().await;
    let received = ingestion.received();
    let outcomes = events(&received, &["McpFeedback"]);
    assert_eq!(outcomes.len(), 1, "{received:#?}");
    assert_eq!(properties(outcomes[0])["rating"], "down");
    assert_eq!(properties(outcomes[0])["clientName"], "person");
}
