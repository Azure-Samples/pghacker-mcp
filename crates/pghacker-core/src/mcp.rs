//! The MCP server plumbing both services share: tool specs with JSON
//! schemas, an rmcp handler over a [`Tools`] implementation, and the axum
//! router with `/mcp` and `/healthz`.
//!
//! The service is stateless: no sessions, any replica serves any request. It
//! serves clients of the 2026-07-28 protocol, which send per-request
//! metadata, and earlier clients, which send `initialize` first.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::routing::get;
use rmcp::handler::server::common::schema_for_type;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

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
        }
    }
}

impl<T: Tools> ServerHandler for McpServer<T> {
    fn get_info(&self) -> ServerConfig {
        (*self.info).clone()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items((*self.list).clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.list.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        if !self.tools.specs().iter().any(|spec| spec.name == name) {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {name}"),
                None,
            ));
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let started = Instant::now();
        let outcome = self.tools.call(&name, arguments).await;
        let ms = started.elapsed().as_millis() as u64;
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

/// `/mcp` and `/healthz`.
pub fn router<T: Tools>(server: McpServer<T>, allowed_hosts: Option<Vec<String>>) -> Router {
    Router::new()
        .route_service("/mcp", service(server, allowed_hosts))
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
