// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The MCP server plumbing both services share: tool specs with JSON
//! schemas, an rmcp handler over a [`Tools`] implementation, and the axum
//! router with `/mcp` and `/healthz`.
//!
//! The service is stateless: no sessions, any replica serves any request. It
//! serves clients of the 2026-07-28 protocol, which send per-request
//! metadata, and earlier clients, which send `initialize` first. Both may
//! ping ([`answer_ping`]).
//!
//! A request that the endpoint's usage middleware gave a [`Scope`]
//! (`crate::usage`) is recorded: its initialize, discover, tools/list or
//! tool call, with the call's arguments, outcome and result's shape, and its
//! reads of the skills ([`McpServer::with_skills`]).

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::handler::server::common::schema_for_type;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomRequest,
    CustomResult, DiscoverResult, ErrorCode, Implementation, InitializeRequestParams,
    InitializeResult, JsonObject, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    ResourceContents, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::calls::abandonable;
use crate::skills::{self, Skills};
use crate::usage::{self, Scope};

/// A tool: its name, its description for agents, what it does to data, and
/// its arguments' schema.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub effect: Effect,
    pub schema: fn() -> Arc<JsonObject>,
}

/// What a tool does to the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Read,
    Write { destructive: bool, idempotent: bool },
}

impl ToolSpec {
    /// The tool as tools/list describes it.
    pub fn tool(&self) -> Tool {
        let annotations = match self.effect {
            Effect::Read => ToolAnnotations::new().read_only(true),
            Effect::Write {
                destructive,
                idempotent,
            } => ToolAnnotations::new()
                .read_only(false)
                .destructive(destructive)
                .idempotent(idempotent),
        };
        Tool::new(self.name, self.description, (self.schema)()).with_annotations(annotations)
    }
}

/// The JSON schema of a tool's arguments.
pub fn schema<T: JsonSchema + 'static>() -> Arc<JsonObject> {
    schema_for_type::<T>()
}

/// What a tool returns.
pub enum Output {
    /// Structured content; clients that read only text get the JSON as text.
    Json(Value),
    /// Text, such as query's CSV or grep's lines.
    Text(String),
    /// A failure the caller should read, such as PostgreSQL's error.
    Failed(String),
}

impl Output {
    pub fn json<T: Serialize>(value: &T) -> Result<Self, ErrorData> {
        serde_json::to_value(value)
            .map(Output::Json)
            .map_err(|error| {
                ErrorData::internal_error(format!("cannot serialize the result: {error}"), None)
            })
    }
}

/// A tool's arguments, or the JSON-RPC error for malformed ones.
pub fn parse<T: DeserializeOwned>(tool: &str, arguments: Value) -> Result<T, ErrorData> {
    serde_json::from_value(arguments)
        .map_err(|error| ErrorData::invalid_params(format!("{tool}'s arguments: {error}"), None))
}

/// A tool failure the caller sees, from any error: the message and its causes.
pub fn failed(error: impl Into<anyhow::Error>) -> Output {
    let error = error.into();
    Output::Failed(format!("{error:#}"))
}

/// A server's tools.
pub trait Tools: Clone + Send + Sync + 'static {
    /// Every tool, in tools/list order.
    fn specs(&self) -> &'static [ToolSpec];

    /// Runs the tool named `name`, which is one of [`Tools::specs`].
    fn call(
        &self,
        name: &str,
        arguments: Value,
    ) -> impl Future<Output = Result<Output, ErrorData>> + Send;
}

/// The rmcp handler over `T`. Cheap to clone; a clone serves each request.
#[derive(Clone)]
pub struct McpServer<T> {
    tools: T,
    list: Arc<Vec<Tool>>,
    info: Arc<ServerConfig>,
    skills: Skills,
    cancel_on_disconnect: bool,
}

impl<T: Tools> McpServer<T> {
    pub fn new(tools: T, name: &str, version: &str, instructions: &str) -> Self {
        let list = tools.specs().iter().map(ToolSpec::tool).collect();
        let info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(name, version))
            .with_instructions(instructions);
        Self {
            tools,
            list: Arc::new(list),
            info: Arc::new(info),
            skills: Skills::default(),
            cancel_on_disconnect: false,
        }
    }

    /// Serves `skills` as the skills extension (SEP-2640) defines: the
    /// capabilities add resources and the extension, `skills/list` and
    /// `skills/get` describe the skills, and resources/list and
    /// resources/read serve their files. Without skills, nothing changes.
    pub fn with_skills(mut self, skills: Skills) -> Self {
        if skills.is_empty() {
            return self;
        }
        let mut info = (*self.info).clone();
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        capabilities.extensions = Some(
            [(skills::EXTENSION.to_owned(), JsonObject::new())]
                .into_iter()
                .collect(),
        );
        info.capabilities = capabilities;
        self.info = Arc::new(info);
        self.skills = skills;
        self
    }

    /// Abandons a call when its client goes away, rather than running it on
    /// unobserved: its future is dropped, and the statements it runs on a
    /// [`CallPool`](crate::calls::CallPool)'s connections are cancelled. Only
    /// for tools that may stop at any await, such as tools that leave no
    /// session setting changed across one.
    pub fn cancel_on_disconnect(mut self) -> Self {
        self.cancel_on_disconnect = true;
        self
    }
}

impl<T: Tools> ServerHandler for McpServer<T> {
    fn get_info(&self) -> ServerConfig {
        (*self.info).clone()
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // rmcp's own initialize, and its event.
        context.peer.set_peer_info(request.clone());
        let result = self.negotiate_initialize(&request);
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event("McpInitialize", "initialize");
            usage::mcp::client(
                &mut event,
                Some(&request.client_info),
                Some(request.protocol_version.as_str()),
                Some(&request.capabilities),
            );
            if let Err(error) = &result {
                usage::mcp::failed(&mut event, "UnsupportedProtocol", &error.message);
            }
            scope.record(event);
        }
        result
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        // rmcp's own discover, and its event.
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event("McpDiscover", "discover");
            request_client(&mut event, &context);
            scope.record(event);
        }
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event("McpToolsList", "tools/list");
            request_client(&mut event, &context);
            scope.record(event);
        }
        Ok(ListToolsResult::with_all_items((*self.list).clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.list.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let arguments = request.arguments.unwrap_or_default();
        // The call's usage event, if the endpoint records usage.
        let usage = usage_scope(&context).map(|scope| {
            let mut event = scope.event("McpToolCall", &name);
            event.property("toolName", &name);
            request_client(&mut event, &context);
            usage::mcp::arguments(&mut event, &arguments);
            (scope, event)
        });
        if !self.tools.specs().iter().any(|spec| spec.name == name) {
            let error = ErrorData::invalid_params(format!("unknown tool {name}"), None);
            if let Some((scope, mut event)) = usage {
                usage::mcp::failed(&mut event, "UnknownTool", &error.message);
                scope.record(event);
            }
            return Err(error);
        }
        let call = self.tools.call(&name, Value::Object(arguments));
        let started = Instant::now();
        let outcome = if self.cancel_on_disconnect {
            // rmcp cancels the request's token when the client disconnects
            // before the answer.
            tokio::select! {
                biased;
                outcome = abandonable(call) => outcome,
                () = context.ct.cancelled() => {
                    let elapsed = started.elapsed();
                    let ms = elapsed.as_millis() as u64;
                    tracing::info!(tool = %name, ms, "tool call abandoned: the client went away");
                    if let Some((scope, mut event)) = usage {
                        usage::mcp::duration(&mut event, elapsed);
                        usage::mcp::failed(&mut event, "Abandoned", "the client went away");
                        scope.record(event);
                    }
                    return Err(ErrorData::internal_error("the client went away", None));
                }
            }
        } else {
            call.await
        };
        let elapsed = started.elapsed();
        let ms = elapsed.as_millis() as u64;
        if let Some((scope, mut event)) = usage {
            usage::mcp::duration(&mut event, elapsed);
            match &outcome {
                Ok(output @ Output::Failed(text)) => {
                    usage::mcp::shape(&mut event, output);
                    usage::mcp::failed(&mut event, "ToolError", text);
                }
                Ok(output) => usage::mcp::succeeded(&mut event, output),
                Err(error) => {
                    let error_type = if error.code == ErrorCode::INVALID_PARAMS {
                        "InvalidParams"
                    } else {
                        "Internal"
                    };
                    usage::mcp::failed(&mut event, error_type, &error.message);
                }
            }
            scope.record(event);
        }
        let result = match outcome {
            Ok(Output::Json(value)) => {
                tracing::info!(tool = %name, ms, "tool call");
                CallToolResult::structured(value)
            }
            Ok(Output::Text(text)) => {
                tracing::info!(tool = %name, ms, "tool call");
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Ok(Output::Failed(text)) => {
                tracing::info!(tool = %name, ms, error = %crate::text::truncate_chars(&text, 300), "tool call failed");
                CallToolResult::error(vec![ContentBlock::text(text)])
            }
            Err(error) => {
                tracing::info!(tool = %name, ms, error = %error.message, "tool call rejected");
                return Err(error);
            }
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event("McpResourcesList", "resources/list");
            request_client(&mut event, &context);
            scope.record(event);
        }
        Ok(ListResourcesResult::with_all_items(self.skills.resources()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri;
        let file = self.skills.file(&uri);
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event("McpResourceRead", "resources/read");
            request_client(&mut event, &context);
            event.property_cut("uri", &uri, URI_CHARS);
            match file {
                Some(_) => {
                    event.property("activityStatus", "Succeeded");
                }
                None => usage::mcp::failed(&mut event, "UnknownResource", "no such resource"),
            }
            scope.record(event);
        }
        let Some(file) = file else {
            // rmcp gives 2026-07-28 clients this error as invalid params.
            return Err(ErrorData::resource_not_found(
                format!("no resource {uri}"),
                None,
            ));
        };
        let contents =
            ResourceContents::text(file.text.clone(), uri).with_mime_type(file.mime_type);
        Ok(ReadResourceResult::new(vec![contents]).into())
    }

    /// `skills/list` and `skills/get`, when the server has skills.
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        let method = request.method.as_str();
        let action = match method {
            "skills/list" if !self.skills.is_empty() => "McpSkillsList",
            "skills/get" if !self.skills.is_empty() => "McpSkillsGet",
            // As rmcp answers any method it doesn't know.
            _ => {
                return Err(ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    request.method,
                    None,
                ));
            }
        };
        let params = request.params.unwrap_or(Value::Null);
        let outcome = self.skills_request(method, &params);
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event(action, method);
            request_client(&mut event, &context);
            if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                event.property_cut("uri", uri, URI_CHARS);
            }
            match &outcome {
                Ok(_) => {
                    event.property("activityStatus", "Succeeded");
                }
                Err(error) => usage::mcp::failed(&mut event, "InvalidParams", &error.message),
            }
            scope.record(event);
        }
        let mut result = outcome?;
        // rmcp completes its own results so for 2026-07-28 clients, but not
        // custom ones: they say they're complete, and listings how long
        // they keep, as tools/list does.
        let current = context
            .protocol_version()
            .is_some_and(|version| version.as_str() >= ProtocolVersion::V_2026_07_28.as_str());
        if current {
            result["resultType"] = json!("complete");
            if method == "skills/list" {
                result["ttlMs"] = json!(0);
                result["cacheScope"] = json!("private");
            }
        }
        Ok(CustomResult::new(result))
    }
}

/// A URI's characters in a usage event, at most.
const URI_CHARS: usize = 1024;

impl<T> McpServer<T> {
    /// The answer to `skills/list` or `skills/get`. One page holds every
    /// skill, so a listing's cursor is one this server never gave.
    fn skills_request(&self, method: &str, params: &Value) -> Result<Value, ErrorData> {
        if method == "skills/list" {
            if params.get("cursor").is_some_and(|cursor| !cursor.is_null()) {
                return Err(ErrorData::invalid_params("unknown cursor", None));
            }
            let skills: Vec<Value> = self.skills.iter().map(skills::Skill::entry).collect();
            return Ok(json!({ "skills": skills }));
        }
        let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| {
            ErrorData::invalid_params("skills/get takes the uri of a skill's SKILL.md", None)
        })?;
        let skill = self
            .skills
            .get(uri)
            .ok_or_else(|| ErrorData::invalid_params(format!("no skill at {uri}"), None))?;
        Ok(json!({ "skill": skill.entry() }))
    }
}

/// The request's usage scope, which the endpoint's usage middleware gives
/// it (`crate::usage::record`).
fn usage_scope(context: &RequestContext<RoleServer>) -> Option<Scope> {
    context
        .extensions
        .get::<http::request::Parts>()?
        .extensions
        .get::<Scope>()
        .cloned()
}

/// The client as a request describes it: a 2026-07-28 client in each
/// request's `_meta`; an earlier one only its protocol version, in a header.
fn request_client(event: &mut usage::Event, context: &RequestContext<RoleServer>) {
    let meta = &context.meta;
    let protocol = meta
        .protocol_version()
        .map(|version| version.to_string())
        .or_else(|| {
            let parts = context.extensions.get::<http::request::Parts>()?;
            let version = parts.headers.get("mcp-protocol-version")?.to_str().ok()?;
            Some(version.to_owned())
        });
    usage::mcp::client(
        event,
        meta.client_info().as_ref(),
        protocol.as_deref(),
        meta.client_capabilities().as_ref(),
    );
}

/// The streamable HTTP service for `server`. `allowed_hosts` lists the Host
/// headers accepted; `None` keeps rmcp's default, loopback only.
pub fn service<T: Tools>(
    server: McpServer<T>,
    allowed_hosts: Option<Vec<String>>,
) -> StreamableHttpService<McpServer<T>, NeverSessionManager> {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None);
    if let Some(hosts) = allowed_hosts {
        config = if hosts.iter().any(|host| host == "*") {
            config.disable_allowed_hosts()
        } else {
            config.with_allowed_hosts(hosts)
        };
    }
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

/// The largest ping read; a ping is a few hundred bytes.
const PING_BYTES: usize = 64 * 1024;

/// Answers `ping` from clients of the 2026-07-28 protocol. That protocol
/// removed it, so rmcp refuses it ("method not found" as HTTP 404, and a
/// warning in the log), but such clients ping anyway. Their pings name
/// themselves in `Mcp-Method`. Each goes to rmcp as an earlier client's ping,
/// which rmcp answers after its usual checks (Host, media types), and the
/// answer comes back in the 2026-07-28 shape. Every other request, earlier
/// clients' pings among them, passes through as it is.
pub async fn answer_ping(request: Request, next: Next) -> Response {
    let headers = request.headers();
    let ping = headers
        .get("mcp-method")
        .is_some_and(|method| method == "ping")
        && headers
            .get("mcp-protocol-version")
            .and_then(|version| version.to_str().ok())
            .is_some_and(|version| version >= "2026-07-28");
    if !ping {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, PING_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let id = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(|message| message["method"] == "ping")
        .map(|message| message["id"].clone());
    let Some(id) = id else {
        // Not a ping after all: rmcp judges it as it was sent.
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    };
    parts.headers.remove("mcp-method");
    parts.headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static("2025-11-25"),
    );
    parts.headers.remove(CONTENT_LENGTH);
    let legacy = json!({ "jsonrpc": "2.0", "id": id, "method": "ping" });
    let response = next
        .run(Request::from_parts(parts, Body::from(legacy.to_string())))
        .await;

    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, PING_BYTES).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut answer) if answer["result"] == json!({}) => {
            answer["result"] = json!({ "resultType": "complete" });
            parts.headers.remove(CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(answer.to_string()))
        }
        // rmcp's refusal (of a Host it doesn't allow, say), as it is.
        _ => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// `/mcp` and `/healthz`.
pub fn router<T: Tools>(server: McpServer<T>, allowed_hosts: Option<Vec<String>>) -> Router {
    Router::new()
        .route_service("/mcp", service(server, allowed_hosts))
        .route_layer(middleware::from_fn(answer_ping))
        .route("/healthz", get(|| async { "ok" }))
}

/// Serves `router` on `addr` until Ctrl+C or SIGTERM.
pub async fn serve(router: Router, addr: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| anyhow::anyhow!("cannot listen on {addr}: {error}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Resolves on Ctrl+C or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
