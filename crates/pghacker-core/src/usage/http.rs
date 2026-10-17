// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The HTTP side of usage telemetry: middleware that gives each request to
//! the MCP endpoint its [`Scope`], what that request's events share, and
//! records the requests turned away before the MCP server sees them, such as
//! those over the rate limit.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::USER_AGENT;
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use super::event::{Event, tags};
use super::{Identity, Usage};
use crate::clients;
use crate::text::truncate_chars;

/// The listener a request came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The public endpoint, for clients anywhere.
    Public,
    /// The listener for agents in the same deployment.
    Internal,
}

impl Endpoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Endpoint::Public => "public",
            Endpoint::Internal => "internal",
        }
    }
}

/// A User-Agent's characters at most.
const USER_AGENT_CHARS: usize = 512;

/// What the events of one request share: the client's address, which only
/// their location tag carries, its identity, the request's ID, and whether
/// they record details. The MCP server's hooks read it from the request
/// (`crate::mcp`).
#[derive(Clone)]
pub struct Scope {
    inner: Arc<ScopeInner>,
}

struct ScopeInner {
    usage: Usage,
    endpoint: Endpoint,
    address: Option<IpAddr>,
    identity: Option<Identity>,
    operation: String,
    user_agent: Option<String>,
    recorded: AtomicBool,
    /// Properties a tool adds to its call's event ([`Scope::annotate`]).
    annotations: Mutex<Vec<(String, String)>>,
}

impl Scope {
    /// The scope of a request to `endpoint` from `address` at `now`. An
    /// address that isn't public, such as a proxy's, is no client's: such a
    /// request has no location and no user.
    pub fn new(
        usage: Usage,
        endpoint: Endpoint,
        address: Option<IpAddr>,
        user_agent: Option<&str>,
        now: Instant,
    ) -> Self {
        let address = address.filter(|ip| clients::is_public(*ip));
        let identity = address.and_then(|ip| usage.identify(ip, now));
        Self {
            inner: Arc::new(ScopeInner {
                usage,
                endpoint,
                address,
                identity,
                operation: Uuid::new_v4().simple().to_string(),
                user_agent: user_agent
                    .map(|agent| truncate_chars(agent, USER_AGENT_CHARS).to_owned()),
                recorded: AtomicBool::new(false),
                annotations: Mutex::new(Vec::new()),
            }),
        }
    }

    /// An event of `action` in this request, for `operation`: the tool
    /// called, or the request's method.
    pub fn event(&self, action: &str, operation: &str) -> Event {
        let inner = &self.inner;
        inner.recorded.store(true, Ordering::Relaxed);
        let mut event = Event::new(action);
        if !inner.usage.details() {
            event.without_details();
        }
        // Without an address of its own, Application Insights would locate
        // the event's sender, this server.
        let location = inner
            .address
            .map_or_else(|| "0.0.0.0".to_owned(), |ip| ip.to_string());
        event
            .tag(tags::LOCATION_IP, location)
            .tag(tags::OPERATION, inner.operation.clone())
            .tag(tags::OPERATION_NAME, operation)
            .property("endpoint", inner.endpoint.as_str());
        if let Some(identity) = &inner.identity {
            event
                .tag(tags::USER, identity.user.clone())
                .tag(tags::SESSION, identity.session.clone());
        }
        if let Some(agent) = &inner.user_agent {
            event.property("userAgent", agent);
        }
        event
    }

    pub fn record(&self, event: Event) {
        self.inner.usage.record(event);
    }

    /// Whether this request's events record what users type into tools.
    pub fn details(&self) -> bool {
        self.inner.usage.details()
    }

    /// Adds `key`: `value` to the event of the tool call this request makes,
    /// which the MCP server records when the tool returns. For what the tool
    /// alone knows, such as a failed query's SQLSTATE; it's recorded whether
    /// or not the scope records details, so it must not quote the user.
    pub fn annotate(&self, key: &str, value: impl Into<String>) {
        self.inner
            .annotations
            .lock()
            .expect("the scope's annotations' lock")
            .push((key.to_owned(), value.into()));
    }

    /// Takes the properties tools added ([`Scope::annotate`]), as the MCP
    /// server does to add them to the call's event.
    pub fn take_annotations(&self) -> Vec<(String, String)> {
        std::mem::take(
            &mut *self
                .inner
                .annotations
                .lock()
                .expect("the scope's annotations' lock"),
        )
    }

    /// Whether an event of this request was made.
    pub fn recorded(&self) -> bool {
        self.inner.recorded.load(Ordering::Relaxed)
    }

    pub fn identity(&self) -> Option<&Identity> {
        self.inner.identity.as_ref()
    }

    /// The client's public address, if it has one.
    pub fn address(&self) -> Option<IpAddr> {
        self.inner.address
    }
}

/// Where a request's client address comes from.
type Address = dyn Fn(&Request) -> Option<IpAddr> + Send + Sync;

/// The middleware's state for one listener.
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<RecorderInner>,
}

struct RecorderInner {
    usage: Usage,
    endpoint: Endpoint,
    address: Box<Address>,
    /// When each client was last recorded turned away, by status.
    turned_away: Mutex<HashMap<(String, u16), Instant>>,
}

/// At most one event this often for a client turned away with a status.
const TURNED_AWAY_EVERY: Duration = Duration::from_secs(60);
const MAX_TURNED_AWAY: usize = 10_000;

impl Recorder {
    /// The state for `endpoint`, whose clients' addresses `address` finds.
    pub fn new(
        usage: Usage,
        endpoint: Endpoint,
        address: impl Fn(&Request) -> Option<IpAddr> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(RecorderInner {
                usage,
                endpoint,
                address: Box::new(address),
                turned_away: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Whether `scope`'s client, turned away with `status` at `now`, is the
    /// first time this minute.
    fn first_this_minute(&self, scope: &Scope, status: StatusCode, now: Instant) -> bool {
        let client = scope
            .identity()
            .map_or_else(String::new, |identity| identity.user.clone());
        let key = (client, status.as_u16());
        let mut seen = self
            .inner
            .turned_away
            .lock()
            .expect("the turned-away clients' lock");
        if seen.len() >= MAX_TURNED_AWAY && !seen.contains_key(&key) {
            seen.retain(|_, at| now.duration_since(*at) < TURNED_AWAY_EVERY);
        }
        match seen.get(&key) {
            Some(at) if now.duration_since(*at) < TURNED_AWAY_EVERY => false,
            _ => {
                seen.insert(key, now);
                true
            }
        }
    }
}

/// The middleware: gives each request to `/mcp` its [`Scope`]. A request
/// answered with an error status that no event of the MCP server covers is
/// recorded once a minute per client and status: as `McpRateLimited` for
/// 429, else `McpHttpRejected`.
pub async fn record(
    State(recorder): State<Recorder>,
    mut request: Request,
    next: Next,
) -> Response {
    let inner = &recorder.inner;
    if !inner.usage.is_on() || request.uri().path() != "/mcp" {
        return next.run(request).await;
    }
    let now = Instant::now();
    let user_agent = request
        .headers()
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let scope = Scope::new(
        inner.usage.clone(),
        inner.endpoint,
        (inner.address)(&request),
        user_agent,
        now,
    );
    let method = request.method().clone();
    let mcp_method = request
        .headers()
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        .map(|value| truncate_chars(value, 64).to_owned());
    request.extensions_mut().insert(scope.clone());

    let response = next.run(request).await;

    let status = response.status();
    let turned_away = status.is_client_error() || status.is_server_error();
    if turned_away && !scope.recorded() && recorder.first_this_minute(&scope, status, now) {
        let action = if status == StatusCode::TOO_MANY_REQUESTS {
            "McpRateLimited"
        } else {
            "McpHttpRejected"
        };
        let mut event = scope.event(action, "http");
        event
            .fail()
            .property("statusCode", status.as_str())
            .property("httpMethod", method.as_str());
        if let Some(mcp_method) = mcp_method {
            event.property("mcpMethod", mcp_method);
        }
        scope.record(event);
    }
    response
}
