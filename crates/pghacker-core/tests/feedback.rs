// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Feedback over HTTP, as clients meet it: an interactive client that can
//! show a form is followed by the pauses between its calls, and the call
//! that starts its next question is answered with the form
//! (`input_required`); the retry carries the user's answer, which is
//! recorded, and runs. Also the clients never asked, the warm-up and the
//! session gap, states bound to their client, `?feedback=off`, and the usage
//! events of each step. One test drives rmcp's own client through the round
//! trip.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
/// The form's message: none, as the rating's title asks the question.
const PROMPT: &str = "";
/// A gap that starts a question: over the tests' question gap, and under
/// their session gap.
const PAUSE: Duration = Duration::from_millis(250);

/// Feedback in every hour, questions apart after 100 ms, sessions active
/// for 3 s, and asks from the question after `min_questions`.
fn settings(min_questions: u32) -> FeedbackSettings {
    FeedbackSettings {
        rate: 1.0,
        question_gap: Duration::from_millis(100),
        session_gap: Duration::from_secs(3),
        min_questions,
        ..FeedbackSettings::default()
    }
}

fn server(settings: FeedbackSettings) -> McpServer<Echo> {
    McpServer::new(Echo, "echo", "1", "Echo tools.")
        .with_feedback(Feedback::new(settings, ID_KEY).unwrap())
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

/// A 2026-07-28 client at `address`, an app named `app` that declares
/// `capabilities`, which posts to `path`.
struct Client {
    http: reqwest::Client,
    base: String,
    address: &'static str,
    app: Option<&'static str>,
    capabilities: Value,
    path: &'static str,
    id: std::sync::atomic::AtomicU64,
}

impl Client {
    /// Copilot CLI, which can show a form.
    fn new(base: &str, address: &'static str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.to_owned(),
            address,
            app: Some("copilot-cli"),
            capabilities: json!({ "elicitation": { "form": {} } }),
            path: "/mcp",
            id: 0.into(),
        }
    }

    fn named(mut self, app: Option<&'static str>) -> Self {
        self.app = app;
        self
    }

    fn declaring(mut self, capabilities: Value) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn at(mut self, path: &'static str) -> Self {
        self.path = path;
        self
    }

    /// The JSON-RPC response to `method` with `params`.
    async fn request(&self, method: &str, params: Value) -> Value {
        let mut params = params;
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": self.capabilities,
        });
        if let Some(app) = self.app {
            params["_meta"]["io.modelcontextprotocol/clientInfo"] =
                json!({ "name": app, "version": "1.0" });
        }
        let id = self.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    /// A call of `echo` with `text`.
    async fn echo(&self, text: &str) -> Value {
        self.call("echo", json!({ "text": text }), None, None).await
    }

    /// A tool call; on a retry, with `responses` to the form and its `state`.
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        responses: Option<Value>,
        state: Option<&str>,
    ) -> Value {
        let mut params = json!({ "name": name, "arguments": arguments });
        if let Some(responses) = responses {
            params["inputResponses"] = responses;
        }
        if let Some(state) = state {
            params["requestState"] = json!(state);
        }
        self.request("tools/call", params).await
    }

    /// `n` questions of one call of `echo` each, a pause apart, none held;
    /// then a pause.
    async fn questions(&self, n: u32) {
        for question in 0..n {
            let answered = self.echo(&format!("q{question}")).await;
            assert_eq!(
                answered["result"]["resultType"], "complete",
                "question {question}: {answered}"
            );
            tokio::time::sleep(PAUSE).await;
        }
    }

    /// The next question's first call, held for the form: the form.
    async fn held(&self) -> Value {
        let asked = self.echo("next").await;
        assert_eq!(asked["result"]["resultType"], "input_required", "{asked}");
        asked["result"].clone()
    }
}

fn state_of(form: &Value) -> &str {
    form["requestState"].as_str().unwrap()
}

fn properties(envelope: &Value) -> &Value {
    &envelope["data"]["baseData"]["properties"]
}

fn measurements(envelope: &Value) -> &Value {
    &envelope["data"]["baseData"]["measurements"]
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
async fn an_interactive_client_is_asked_as_its_next_question_starts_and_its_retry_runs() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(settings(1)), Some(&ingestion)).await;
    let client = Client::new(&base, CLIENT);

    // The first question: two calls, as an answer takes, never held.
    let first = client.echo("hi").await;
    assert_eq!(first["result"]["resultType"], "complete", "{first}");
    assert_eq!(
        first["result"]["structuredContent"],
        json!({ "echo": "hi" })
    );
    assert_eq!(
        first["result"]["content"].as_array().map(Vec::len),
        Some(1),
        "nothing for the model: {first}"
    );
    let second = client
        .call("lines", json!({ "text": "more" }), None, None)
        .await;
    assert_eq!(second["result"]["resultType"], "complete", "{second}");
    tokio::time::sleep(PAUSE).await;

    // The second question's first call is held: the form, which the tool
    // hasn't run for.
    let form = client.held().await;
    let request = &form["inputRequests"]["feedback"];
    assert_eq!(request["method"], "elicitation/create");
    assert_eq!(
        request["params"],
        json!({
            "mode": "form",
            "message": PROMPT,
            "requestedSchema": {
                "type": "object",
                "properties": { "rating": {
                    "type": "string",
                    "title": "Was the last answer helpful?",
                    "oneOf": [
                        { "const": "up", "title": "👍 Helpful" },
                        { "const": "down", "title": "👎 Not helpful" },
                        { "const": "skip", "title": "Skip" },
                    ],
                } },
                "required": ["rating"],
            },
        })
    );
    let state = state_of(&form);
    assert!(state.starts_with("fb3."), "{state}");

    // The retry carries the answer and the state, and gets the tool's result.
    let ran = client
        .call(
            "echo",
            json!({ "text": "next" }),
            Some(answer("up")),
            Some(state),
        )
        .await;
    assert_eq!(ran["result"]["resultType"], "complete", "{ran}");
    assert_eq!(
        ran["result"]["structuredContent"],
        json!({ "echo": "next" })
    );
    // Asked once this hour: the next question runs.
    tokio::time::sleep(PAUSE).await;
    let later = client.echo("later").await;
    assert_eq!(later["result"]["resultType"], "complete", "{later}");

    usage.flush().await;
    let received = ingestion.received();
    let seen: Vec<(&str, &str, &str)> = events(
        &received,
        &["McpToolCall", "McpFeedbackAsked", "McpFeedback"],
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
            ("McpToolCall", "lines", "Succeeded"),
            ("McpToolCall", "echo", "InputRequired"),
            ("McpFeedbackAsked", "echo", ""),
            ("McpFeedback", "echo", "rated"),
            ("McpToolCall", "echo", "Succeeded"),
            ("McpToolCall", "echo", "Succeeded"),
        ],
        "{received:#?}"
    );
    let calls = events(&received, &["McpToolCall"]);
    let asked = events(&received, &["McpFeedbackAsked"])[0];
    let rated = events(&received, &["McpFeedback"])[0];
    // The ask shares the held call's operation, and says what it rates: the
    // first question, its two calls, and the pause after it.
    assert_eq!(
        asked["tags"]["ai.operation.id"],
        calls[2]["tags"]["ai.operation.id"]
    );
    assert_eq!(measurements(asked)["question"], 1.0);
    assert_eq!(measurements(asked)["questionCalls"], 2.0);
    assert_eq!(measurements(asked)["questionFailed"], 0.0);
    let gap = measurements(asked)["gapSeconds"].as_f64().unwrap();
    assert!((0.1..3.0).contains(&gap), "{gap}");
    let start = properties(asked)["questionStart"].as_str().unwrap();
    let end = properties(asked)["questionEnd"].as_str().unwrap();
    assert!(start.ends_with('Z') && start <= end, "{start} {end}");
    // The rated question's calls are the client's within those times.
    assert!(calls[0]["time"].as_str().unwrap() >= start);
    assert!(calls[1]["time"].as_str().unwrap() <= end);
    let ask_id = properties(asked)["askId"].as_str().unwrap();
    assert_eq!(ask_id.len(), 32);
    assert_eq!(properties(rated)["askId"], ask_id);
    assert_eq!(properties(rated)["rating"], "up");
    assert_eq!(properties(rated)["answeredFrom"], "same");
    assert!(properties(rated).get("comment").is_none());
    assert_eq!(properties(rated)["clientName"], "copilot-cli");
    assert_eq!(rated["tags"]["ai.user.id"], asked["tags"]["ai.user.id"]);
    // The retry records its arguments, and never the answer.
    assert_eq!(properties(calls[3])["arg.text"], "next");
    assert!(properties(calls[3]).get("arg.rating").is_none());
}

#[tokio::test]
async fn whatever_the_answer_it_is_recorded_and_the_request_runs() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(settings(1)), Some(&ingestion)).await;
    let answers = [
        ("198.51.100.1", answer("up")),
        ("198.51.100.2", answer("down")),
        ("198.51.100.3", answer("skip")),
        (
            "198.51.100.4",
            json!({ "feedback": { "action": "decline" } }),
        ),
        (
            "198.51.100.5",
            json!({ "feedback": { "action": "cancel" } }),
        ),
        ("198.51.100.6", answer("meh")),
    ];
    for (address, response) in answers {
        let client = Client::new(&base, address);
        client.questions(1).await;
        let form = client.held().await;
        let ran = client
            .call(
                "echo",
                json!({ "text": "next" }),
                Some(response),
                Some(state_of(&form)),
            )
            .await;
        assert_eq!(
            ran["result"]["structuredContent"],
            json!({ "echo": "next" })
        );
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
            ("rated", "up"),
            ("rated", "down"),
            ("skipped", ""),
            ("declined", ""),
            ("dismissed", ""),
            ("dismissed", ""),
        ]
    );
}

#[tokio::test]
async fn only_interactive_clients_that_can_show_a_form_are_asked() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(settings(0)), Some(&ingestion)).await;
    let never = [
        // A script with the Python SDK's default name, or none.
        Client::new(&base, "198.51.100.1").named(Some("mcp")),
        Client::new(&base, "198.51.100.2").named(None),
        // An interactive app that can't show a form.
        Client::new(&base, "198.51.100.3").declaring(json!({})),
        Client::new(&base, "198.51.100.4").declaring(json!({ "elicitation": { "url": {} } })),
    ];
    for client in &never {
        client.questions(3).await;
    }
    // An app whose name contains one on the list, ignoring case, is asked.
    let codex = Client::new(&base, "198.51.100.5").named(Some("Codex-MCP-Client"));
    codex.questions(1).await;
    codex.held().await;

    // A client of an earlier protocol sends no capabilities: never held.
    let http = reqwest::Client::new();
    for n in 0..3 {
        let answered: Value = http
            .post(format!("{base}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2025-11-25")
            .header("x-test-client", "198.51.100.6")
            .json(&json!({ "jsonrpc": "2.0", "id": n, "method": "tools/call",
                "params": { "name": "echo", "arguments": { "text": "hi" } } }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            answered["result"].get("inputRequests").is_none(),
            "{answered}"
        );
        tokio::time::sleep(PAUSE).await;
    }

    usage.flush().await;
    assert_eq!(
        events(&ingestion.received(), &["McpFeedbackAsked"]).len(),
        1
    );
}

#[tokio::test]
async fn the_warm_up_and_the_session_gap_hold_asks_back() {
    let (base, _usage) = listen(
        server(FeedbackSettings {
            session_gap: Duration::from_millis(600),
            ..settings(2)
        }),
        Some(&Ingestion::start().await),
    )
    .await;
    // Questions 1 and 2 are the warm-up; question 3 is asked about 2.
    let client = Client::new(&base, "198.51.100.1");
    client.questions(2).await;
    client.held().await;

    // Back after the session gap, a question isn't asked; the next one is.
    let back = Client::new(&base, "198.51.100.2");
    back.questions(2).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    back.questions(1).await;
    back.held().await;
}

/// A retry of `echo` by `client`, with 👎 and `state`: the call runs,
/// whatever happens to the answer.
async fn retry(client: &Client, state: Option<&str>) {
    let ran = client
        .call(
            "echo",
            json!({ "text": "next" }),
            Some(answer("down")),
            state,
        )
        .await;
    assert_eq!(
        ran["result"]["structuredContent"],
        json!({ "echo": "next" })
    );
}

#[tokio::test]
async fn a_state_answers_once_for_its_app_and_tool_from_any_address() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(settings(1)), Some(&ingestion)).await;
    let client = Client::new(&base, CLIENT);
    client.questions(1).await;
    let form = client.held().await;
    let state = state_of(&form);
    // Another app's retry, or one without the state, records nothing.
    let codex = Client::new(&base, "198.51.100.7").named(Some("codex-mcp-client"));
    retry(&codex, Some(state)).await;
    retry(&client, None).await;
    // Another tool's retry neither.
    let ran = client
        .call(
            "lines",
            json!({ "text": "x" }),
            Some(answer("down")),
            Some(state),
        )
        .await;
    assert_eq!(ran["result"]["content"][0]["text"], "x\n");
    // The app's retry from another address, as behind a gateway that gives
    // each connection its own, records it, once.
    retry(&Client::new(&base, "203.0.113.77"), Some(state)).await;
    retry(&client, Some(state)).await;
    retry(&Client::new(&base, "198.51.100.8"), Some(state)).await;

    usage.flush().await;
    let received = ingestion.received();
    let recorded = events(&received, &["McpFeedback"]);
    assert_eq!(recorded.len(), 1, "{received:#?}");
    let asked = events(&received, &["McpFeedbackAsked"])[0];
    assert_eq!(properties(recorded[0])["askId"], properties(asked)["askId"]);
    assert_eq!(properties(recorded[0])["rating"], "down");
    // From the asked address's /24, under the answering address's identity.
    assert_eq!(properties(recorded[0])["answeredFrom"], "same24");
    assert_ne!(
        recorded[0]["tags"]["ai.user.id"],
        asked["tags"]["ai.user.id"]
    );
    let rejected: Vec<&str> = events(&received, &["McpFeedbackRejected"])
        .into_iter()
        .map(|event| properties(event)["reason"].as_str().unwrap())
        .collect();
    // One rejection a minute per address: the other app's, this client's
    // first, and the third address's.
    assert_eq!(rejected, ["invalid", "invalid", "used"]);
}

#[tokio::test]
async fn feedback_off_in_the_url_is_never_asked_nor_recorded() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(server(settings(0)), Some(&ingestion)).await;
    let client = Client::new(&base, CLIENT).at("/mcp?feedback=off");
    client.questions(3).await;
    // A forged retry records nothing either.
    client
        .call(
            "echo",
            json!({ "text": "hi" }),
            Some(answer("up")),
            Some("fb3.forged"),
        )
        .await;

    usage.flush().await;
    let received = ingestion.received();
    assert!(
        events(
            &received,
            &["McpFeedbackAsked", "McpFeedback", "McpFeedbackRejected"]
        )
        .is_empty(),
        "{received:#?}"
    );
}

#[tokio::test]
async fn requests_without_an_identity_are_never_asked() {
    let (base, _usage) = listen(server(settings(0)), None).await;
    Client::new(&base, CLIENT).questions(3).await;
}

#[tokio::test]
async fn with_comments_on_the_form_takes_one_after_the_rating() {
    let ingestion = Ingestion::start().await;
    let (base, usage) = listen(
        server(FeedbackSettings {
            comments: true,
            ..settings(1)
        }),
        Some(&ingestion),
    )
    .await;
    let client = Client::new(&base, CLIENT);
    client.questions(1).await;
    let form = client.held().await;
    let schema = &form["inputRequests"]["feedback"]["params"]["requestedSchema"];
    assert_eq!(schema["required"], json!(["rating"]));
    assert_eq!(schema["properties"]["remarks"]["maxLength"], 500);
    let response = json!({ "feedback": { "action": "accept", "content": {
        "rating": "down", "remarks": "cited the wrong thread" } } });
    client
        .call(
            "echo",
            json!({ "text": "next" }),
            Some(response),
            Some(state_of(&form)),
        )
        .await;

    usage.flush().await;
    let received = ingestion.received();
    let rated = events(&received, &["McpFeedback"]);
    assert_eq!(properties(rated[0])["comment"], "cited the wrong thread");
    assert_eq!(measurements(rated[0])["commentLength"], 22.0);
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
    let (base, usage) = listen(
        server(FeedbackSettings {
            clients: vec!["person".into()],
            ..settings(1)
        }),
        Some(&ingestion),
    )
    .await;
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
    let echo = |text: &str| {
        CallToolRequestParams::new("echo")
            .with_arguments(json!({ "text": text }).as_object().unwrap().clone())
    };
    client.call_tool(echo("first")).await.unwrap();
    tokio::time::sleep(PAUSE).await;
    // rmcp's client shows the form, retries with the answer and the state,
    // and gets the result.
    let ran = client.call_tool(echo("second")).await.unwrap();
    assert_eq!(ran.structured_content, Some(json!({ "echo": "second" })));
    let asked = person.asked.lock().unwrap().clone();
    assert_eq!(asked, [PROMPT]);
    client.cancel().await.unwrap();

    usage.flush().await;
    let received = ingestion.received();
    let outcomes = events(&received, &["McpFeedback"]);
    assert_eq!(outcomes.len(), 1, "{received:#?}");
    assert_eq!(properties(outcomes[0])["rating"], "down");
    assert_eq!(properties(outcomes[0])["clientName"], "person");
}
