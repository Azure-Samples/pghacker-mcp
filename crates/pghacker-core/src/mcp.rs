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
//! reads of the skills ([`McpServer::with_skills`]), whose tools, for clients
//! without the skills extension, are recorded as tool calls. A tool may
//! record events of its own in its call's operation
//! ([`Tools::call_with_usage`]). With feedback ([`McpServer::with_feedback`]),
//! a call that starts a user's next question may first be answered with a
//! form that asks whether the last answer helped; its retry carries the
//! answer, and runs.

use std::future::Future;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

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
use crate::feedback::{Arrival, Ask, Caller, Feedback, Outcome};
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

/// A rejected call's error for the log at info. For malformed arguments
/// ([`parse`]), it's the kind of problem, such as missing field `sql` or
/// invalid type, without the values and unknown names serde's message
/// quotes; for other errors, which are the server's own, the message.
fn rejection(error: &ErrorData) -> String {
    if error.code != ErrorCode::INVALID_PARAMS {
        return error.message.to_string();
    }
    let problem = error
        .message
        .split_once("'s arguments: ")
        .map_or("", |(_, problem)| problem);
    if let Some((field, _)) = problem
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split_once('`'))
    {
        return format!("missing field `{field}`");
    }
    [
        "unknown field",
        "duplicate field",
        "invalid type",
        "invalid value",
        "invalid length",
        "unknown variant",
    ]
    .into_iter()
    .find(|kind| problem.starts_with(kind))
    .unwrap_or("invalid arguments")
    .to_owned()
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

    /// Runs the tool as [`Tools::call`] does, with the request's usage
    /// `scope`, for a tool that records events of its own: they share the
    /// call's operation. Without an override, it's [`Tools::call`].
    fn call_with_usage(
        &self,
        name: &str,
        arguments: Value,
        scope: Option<Scope>,
    ) -> impl Future<Output = Result<Output, ErrorData>> + Send {
        let _ = scope;
        self.call(name, arguments)
    }
}

/// The rmcp handler over `T`. Cheap to clone; a clone serves each request.
#[derive(Clone)]
pub struct McpServer<T> {
    tools: T,
    /// The tools listed: with skills, those that load them too.
    list: Arc<Vec<Tool>>,
    /// The tools listed to a client that declares the skills extension.
    extension_list: Arc<Vec<Tool>>,
    info: Arc<ServerConfig>,
    /// The server's information for a client that declares the skills
    /// extension: its own instructions.
    extension_info: Arc<ServerConfig>,
    skills: Skills,
    feedback: Feedback,
    cancel_on_disconnect: bool,
}

impl<T: Tools> McpServer<T> {
    pub fn new(tools: T, name: &str, version: &str, instructions: &str) -> Self {
        let list = Arc::new(tools.specs().iter().map(ToolSpec::tool).collect());
        let info = Arc::new(
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new(name, version))
                .with_instructions(instructions),
        );
        Self {
            tools,
            extension_list: Arc::clone(&list),
            list,
            extension_info: Arc::clone(&info),
            info,
            skills: Skills::default(),
            feedback: Feedback::off(),
            cancel_on_disconnect: false,
        }
    }

    /// Asks for feedback now and then ([`crate::feedback`]): a call that
    /// starts a user's next question may first be answered with the form,
    /// and its retry, with the answer, runs. The tools and the instructions
    /// don't change. Only for a listener whose answers people read: not the
    /// internal one, whose clients are agents.
    pub fn with_feedback(mut self, feedback: Feedback) -> Self {
        self.feedback = feedback;
        self
    }

    /// Serves `skills` as the skills extension (SEP-2640) defines: the
    /// capabilities add resources and the extension, `skills/list` and
    /// `skills/get` describe the skills, and resources/list and
    /// resources/read serve their files.
    ///
    /// A client that declares the extension ([`skills::declared`]) gets the
    /// skills that way only, and `extension_instructions` in place of the
    /// server's instructions. Any other client gets the server's
    /// instructions, which should name the tools that load the skills: a
    /// tool per skill, without arguments, which returns its `SKILL.md`
    /// ([`skills::Skill::tool`]). A request carries its client's
    /// capabilities in initialize or, from 2026-07-28 clients, in its
    /// `_meta`; one without them, such as an earlier client's tools/list,
    /// gets the tools. Without skills, nothing changes. Refuses a skill's
    /// tool whose name another tool has.
    pub fn with_skills(
        mut self,
        skills: Skills,
        extension_instructions: &str,
    ) -> anyhow::Result<Self> {
        if skills.is_empty() {
            return Ok(self);
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
        let mut list = (*self.list).clone();
        for skill in skills.iter() {
            let tool = skill.tool();
            anyhow::ensure!(
                !list.iter().any(|other| other.name == tool.name),
                "the tool {} that loads the skill {} has another tool's name",
                tool.name,
                skill.name
            );
            list.push(tool);
        }
        self.extension_list = Arc::clone(&self.list);
        self.list = Arc::new(list);
        self.extension_info = Arc::new(info.clone().with_instructions(extension_instructions));
        self.info = Arc::new(info);
        self.skills = skills;
        Ok(self)
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
        // rmcp's own initialize, with the instructions for the client, and
        // its event.
        context.peer.set_peer_info(request.clone());
        let mut result = self.negotiate_initialize(&request);
        if let Ok(result) = &mut result
            && skills::declared(Some(&request.capabilities))
        {
            result
                .instructions
                .clone_from(&self.extension_info.instructions);
        }
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
        // rmcp's own discover, with the instructions for the client, and
        // its event.
        if let Some(scope) = usage_scope(&context) {
            let mut event = scope.event("McpDiscover", "discover");
            request_client(&mut event, &context);
            scope.record(event);
        }
        let info = if declares_skills(&context) {
            &self.extension_info
        } else {
            &self.info
        };
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            (**info).clone(),
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
        Ok(ListToolsResult::with_all_items(
            (**self.tools_seen(&context)).clone(),
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.list.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Before the call's usage event takes its time, so that a question
        // feedback follows starts no later than its first call's event.
        let now = SystemTime::now();
        let name = request.name.to_string();
        let arguments = request.arguments.unwrap_or_default();
        let scope = usage_scope(&context);
        let tool = self
            .tools_seen(&context)
            .iter()
            .find(|tool| tool.name == name);
        // The call's usage event, if the endpoint records usage.
        let usage = scope.clone().map(|scope| {
            // An unknown tool's name is the client's, cut without details.
            let recorded = if tool.is_some() || scope.details() {
                name.as_str()
            } else {
                crate::text::truncate_chars(&name, UNKNOWN_TOOL_CHARS)
            };
            let mut event = scope.event("McpToolCall", recorded);
            event.property("toolName", recorded);
            request_client(&mut event, &context);
            let schema = tool.map(|tool| tool.input_schema.as_ref());
            usage::mcp::arguments(&mut event, &arguments, schema);
            (scope, event)
        });
        if tool.is_none() {
            let error = ErrorData::invalid_params(format!("unknown tool {name}"), None);
            if let Some((scope, mut event)) = usage {
                usage::mcp::failed(&mut event, "UnknownTool", &error.message);
                scope.record(event);
            }
            return Err(error);
        }
        // Feedback: a call that starts a user's next question may be held
        // for the form; the retry, which carries the answer, runs.
        let followed = if self.feedback.is_on() {
            let caller = caller(&context, scope.as_ref());
            if request.input_responses.is_some() || request.request_state.is_some() {
                let resumed = self.feedback.resume(
                    &caller,
                    &name,
                    request.request_state.as_deref(),
                    request.input_responses.as_ref(),
                    now,
                );
                record_answer(
                    resumed.outcome,
                    resumed.rejection,
                    &name,
                    &context,
                    scope.as_ref(),
                );
                Some(resumed.call)
            } else {
                match self.feedback.arrive(&caller, &name, now) {
                    Arrival::Run(call) => Some(call),
                    Arrival::Ask(ask) => return Ok(ask_feedback(ask, &name, &context, usage)),
                }
            }
        } else {
            None
        };
        // A skill's tool returns its SKILL.md; the others are the server's.
        let skill = self.skills.by_tool(&name);
        let call = async {
            match skill {
                Some(skill) => Ok(Output::Text(skill.instructions().to_owned())),
                None => {
                    self.tools
                        .call_with_usage(&name, Value::Object(arguments), scope.clone())
                        .await
                }
            }
        };
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
                        annotate(&mut event, &scope);
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
        if let Some(followed) = followed {
            let ok = matches!(outcome, Ok(Output::Json(_) | Output::Text(_)));
            followed.end(ok, SystemTime::now());
        }
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
            annotate(&mut event, &scope);
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
                // A failure can quote the caller's arguments: only debug logs have it.
                tracing::info!(tool = %name, ms, "tool call failed");
                tracing::debug!(tool = %name, error = %crate::text::truncate_chars(&text, 300), "why the tool call failed");
                CallToolResult::error(vec![ContentBlock::text(text)])
            }
            Err(error) => {
                tracing::info!(tool = %name, ms, code = error.code.0, error = %rejection(&error), "tool call rejected");
                tracing::debug!(tool = %name, error = %error.message, "why the tool call was rejected");
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
/// An unknown tool's name's characters in a usage event without details, at
/// most.
const UNKNOWN_TOOL_CHARS: usize = 128;

impl<T> McpServer<T> {
    /// The tools a request's client sees: without those that load the
    /// skills if it declares the skills extension.
    fn tools_seen(&self, context: &RequestContext<RoleServer>) -> &Arc<Vec<Tool>> {
        if declares_skills(context) {
            &self.extension_list
        } else {
            &self.list
        }
    }

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

/// Whether the request's client declares the skills extension, in the
/// capabilities a 2026-07-28 request carries in its `_meta`. An earlier
/// client's requests after initialize carry none.
fn declares_skills(context: &RequestContext<RoleServer>) -> bool {
    skills::declared(context.meta.client_capabilities().as_ref())
}

/// Adds to a tool call's event what the tool added to its scope
/// ([`Scope::annotate`]).
fn annotate(event: &mut usage::Event, scope: &Scope) {
    for (key, value) in scope.take_annotations() {
        event.property(key, value);
    }
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

/// What a request tells feedback about its client: its identity and
/// address, its app, whether it can show a form, and whether its URL turns
/// feedback off.
fn caller<'a>(context: &RequestContext<RoleServer>, scope: Option<&'a Scope>) -> Caller<'a> {
    Caller {
        user: scope
            .and_then(Scope::identity)
            .map(|identity| identity.user.as_str()),
        address: scope.and_then(Scope::address),
        app: context.meta.client_info().map(|info| info.name),
        elicits: elicits_form(context),
        opted_out: feedback_off(context),
    }
}

/// Holds a call for the feedback form: its usage event says input is
/// required, and the ask's event says which question it rates.
fn ask_feedback(
    ask: Ask,
    tool: &str,
    context: &RequestContext<RoleServer>,
    usage: Option<(Scope, usage::Event)>,
) -> CallToolResponse {
    let rated = ask.rated;
    if let Some((scope, mut event)) = usage {
        usage::mcp::input_required(&mut event);
        scope.record(event);
        let mut asked = scope.event("McpFeedbackAsked", tool);
        asked
            .property("askId", &ask.ask_id)
            .property("toolName", tool)
            .property("questionStart", rfc3339(rated.start))
            .property("questionEnd", rfc3339(rated.end))
            .measure("question", f64::from(rated.number))
            .measure("questionCalls", f64::from(rated.calls))
            .measure("questionFailed", f64::from(rated.failed))
            .measure("gapSeconds", rated.gap.as_secs_f64());
        request_client(&mut asked, context);
        scope.record(asked);
    }
    tracing::info!(tool, question = rated.number, "feedback asked");
    CallToolResponse::InputRequired(ask.form)
}

/// The answer a retry carried, or why it isn't recorded, as usage events.
fn record_answer(
    outcome: Option<Outcome>,
    rejection: Option<&'static str>,
    tool: &str,
    context: &RequestContext<RoleServer>,
    scope: Option<&Scope>,
) {
    if let Some(outcome) = &outcome {
        tracing::info!(
            tool,
            outcome = outcome.outcome,
            answered_from = outcome.answered_from,
            "feedback recorded"
        );
    }
    let Some(scope) = scope else {
        return;
    };
    if let Some(outcome) = outcome {
        let mut event = scope.event("McpFeedback", tool);
        event
            .property("askId", &outcome.ask_id)
            .property("toolName", tool)
            .property("outcome", outcome.outcome)
            .property("answeredFrom", outcome.answered_from);
        if let Some(rating) = outcome.rating {
            event.property("rating", rating);
        }
        if let Some(comment) = &outcome.comment {
            event
                .property("comment", comment)
                .measure("commentLength", comment.chars().count() as f64);
        }
        request_client(&mut event, context);
        scope.record(event);
    }
    if let Some(reason) = rejection {
        let mut event = scope.event("McpFeedbackRejected", tool);
        event
            .fail()
            .property("reason", reason)
            .property("toolName", tool);
        request_client(&mut event, context);
        scope.record(event);
    }
}

/// `time` as RFC 3339, in UTC to the millisecond.
fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Whether the request is a 2026-07-28 one that declares form elicitation:
/// `elicitation` with `form`, or without `url` (`{}` means form). Earlier
/// requests carry no capabilities, and only 2026-07-28 ones may be answered
/// `input_required`.
fn elicits_form(context: &RequestContext<RoleServer>) -> bool {
    let current = context
        .protocol_version()
        .is_some_and(|version| version.as_str() >= ProtocolVersion::V_2026_07_28.as_str());
    current
        && context
            .meta
            .client_capabilities()
            .and_then(|capabilities| capabilities.elicitation)
            .is_some_and(|elicitation| elicitation.form.is_some() || elicitation.url.is_none())
}

/// Whether the request's URL has `feedback=off`, as in `/mcp?feedback=off`.
fn feedback_off(context: &RequestContext<RoleServer>) -> bool {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.uri.query())
        .is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .any(|(name, value)| name == "feedback" && value.eq_ignore_ascii_case("off"))
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Args {
        sql: String,
        max_rows: Option<u32>,
    }

    fn rejected(arguments: Value) -> ErrorData {
        parse::<Args>("query", arguments).expect_err("malformed arguments")
    }

    #[test]
    fn rejections_are_logged_without_the_callers_values() {
        for (arguments, expected, quoted) in [
            (json!({}), "missing field `sql`", "`sql`"),
            (
                json!({"sql": "SELECT 1", "secret_name": 1}),
                "unknown field",
                "secret_name",
            ),
            (
                json!({"sql": "SELECT 'secret value'", "max_rows": "secret count"}),
                "invalid type",
                "secret count",
            ),
            (
                json!({"sql": "SELECT 1", "max_rows": -424242}),
                "invalid value",
                "424242",
            ),
            (json!("secret element"), "invalid type", "secret element"),
        ] {
            let error = rejected(arguments);
            assert!(error.message.contains(quoted), "{}", error.message);
            assert_eq!(rejection(&error), expected, "{}", error.message);
        }
        let internal = ErrorData::internal_error("cannot serialize the result", None);
        assert_eq!(rejection(&internal), "cannot serialize the result");
    }
}
