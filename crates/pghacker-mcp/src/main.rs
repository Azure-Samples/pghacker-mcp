// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-mcp: the public MCP server. Stateless, read-only (pgh_reader),
//! rate-limited per client, no LLM calls beyond query embeddings. It serves
//! the repository's skills too (`skills/`). An optional second listener, for
//! agents in the same deployment, has a rate limit of its own, per address
//! that agents connect from, without the public one's Front Door check.
//! With PGHACKER_USAGE_ENDPOINT set, both listeners record usage
//! (`pghacker_core::usage`), and with PGHACKER_FEEDBACK_RATE above 0 too, the
//! public one now and then asks an interactive client's user, as their next
//! question starts, whether the last answer helped
//! (`pghacker_core::feedback`).
//!
//! Under load, the public listener limits the calls it runs at once, per
//! client (PGHACKER_MCP_CALLS_PER_CLIENT) and in all (PGHACKER_MCP_CALLS): by
//! default twice the connections of the pools, which both listeners share.
//! A tool call waits a bounded time for a connection
//! (PGHACKER_MCP_POOL_WAIT_SECONDS), so the calls past the connections keep
//! them busy, and runs to a deadline (PGHACKER_MCP_CALL_TIMEOUT_SECONDS); one
//! whose client goes away is abandoned. Either way its statements are
//! cancelled.

mod admission;
mod ratelimit;
mod tools;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum::Router;
use axum::http::{HeaderName, StatusCode};
use axum::middleware;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::timeout::TimeoutLayer;

use pghacker_core::calls::CallPool;
use pghacker_core::config::Env;
use pghacker_core::embed::Embedder;
use pghacker_core::feedback::Feedback;
use pghacker_core::mcp::{McpServer, router, serve};
use pghacker_core::usage::{self, Endpoint, Recorder, Usage};
use pghacker_core::{db, telemetry};

use crate::admission::Admission;
use crate::ratelimit::{ClientIp, FrontDoor, RateLimit};
use crate::tools::{Server, instructions};

/// How long a tool call waits for a connection, by default, before it fails
/// as busy; and a request on the public listener for its turn.
const POOL_WAIT_SECONDS: u64 = 10;

/// The most connections the two pools, PGHACKER_MCP_POOL and
/// PGHACKER_QUERY_POOL, hold in all: pgh_reader's connection limit, 160, is
/// a backstop above it.
const MAX_POOLED: usize = 128;

/// The calls a client may run at once on the public listener, by default.
const CALLS_PER_CLIENT: usize = 10;

/// A tool call's deadline, by default: past code search's 15-second budget
/// and query's own 20-second deadline, so that those report their own
/// outcome.
const CALL_TIMEOUT_SECONDS: u64 = 30;

/// How much longer than a call's wait for its turn and its deadline a
/// request may take before it's answered 503: the call's own error comes
/// first.
const REQUEST_MARGIN: Duration = Duration::from_secs(10);

/// The runtime's threads, which run the tool calls and query's checks, have
/// query's stack size.
fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(pghacker_sqlquery::STACK_BYTES)
        .build()
        .context("cannot start the async runtime")?
        .block_on(run())
}

async fn run() -> Result<()> {
    telemetry::init();
    let env = Env::process();
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("pghacker-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let reader_url = env.reader_database_url()?;
    let addr = env.get_or("PGHACKER_MCP_ADDR", "127.0.0.1:3102").to_owned();
    let internal_addr = env.get("PGHACKER_MCP_INTERNAL_ADDR").map(str::to_owned);
    let pool_size = env.parse_or("PGHACKER_MCP_POOL", 16usize)?;
    let query_pool_size = env.parse_or("PGHACKER_QUERY_POOL", 8usize)?;
    ensure!(
        pool_size >= 1 && query_pool_size >= 1 && pool_size + query_pool_size <= MAX_POOLED,
        "PGHACKER_MCP_POOL and PGHACKER_QUERY_POOL must each be at least 1, and at most {MAX_POOLED} in all"
    );
    // A connection serves one call at a time, and a call waits for one, so
    // twice as many calls as connections, by default, keep them all busy.
    let calls = env.parse_or("PGHACKER_MCP_CALLS", 2 * (pool_size + query_pool_size))?;
    let calls_per_client = env.parse_or("PGHACKER_MCP_CALLS_PER_CLIENT", CALLS_PER_CLIENT)?;
    ensure!(calls > 0, "PGHACKER_MCP_CALLS must be at least 1");
    ensure!(
        calls_per_client > 0,
        "PGHACKER_MCP_CALLS_PER_CLIENT must be at least 1"
    );
    let pool_wait = env.parse_or("PGHACKER_MCP_POOL_WAIT_SECONDS", POOL_WAIT_SECONDS)?;
    let call_timeout = env.parse_or("PGHACKER_MCP_CALL_TIMEOUT_SECONDS", CALL_TIMEOUT_SECONDS)?;
    ensure!(
        pool_wait > 0,
        "PGHACKER_MCP_POOL_WAIT_SECONDS must be at least 1"
    );
    ensure!(
        call_timeout > 0,
        "PGHACKER_MCP_CALL_TIMEOUT_SECONDS must be at least 1"
    );
    let pool_wait = Duration::from_secs(pool_wait);
    let call_timeout = Duration::from_secs(call_timeout);
    if call_timeout <= pghacker_sqlquery::CALL_TIMEOUT {
        tracing::warn!(
            "PGHACKER_MCP_CALL_TIMEOUT_SECONDS is within query's own {}-second deadline, which then can't report its outcome",
            pghacker_sqlquery::CALL_TIMEOUT.as_secs()
        );
    }
    let pool = CallPool::new(db::pool(&reader_url, pool_size, "pghacker-mcp")?, pool_wait);
    let embedder = Embedder::from_env(&env)?;
    if embedder.is_none() {
        tracing::warn!(
            "HORIZON_EMBED_* is not set: semantic search is off and hybrid falls back to full text"
        );
    }
    let query = pghacker_sqlquery::QueryTool::new(&reader_url, query_pool_size)?;
    let usage = Usage::from_env(&env, "pghacker-mcp", env!("CARGO_PKG_VERSION"))?;
    let feedback = Feedback::from_env(&env, &usage)?;
    // The client's address: a header the proxy in front sets
    // (PGHACKER_CLIENT_IP_HEADER, such as x-azure-socketip), the first
    // X-Forwarded-For address (PGHACKER_TRUST_FORWARDED), or the peer.
    let client_ip = match env.get("PGHACKER_CLIENT_IP_HEADER") {
        Some(name) => ClientIp::Header(
            HeaderName::try_from(name.trim().to_ascii_lowercase())
                .context("PGHACKER_CLIENT_IP_HEADER isn't a header name")?,
        ),
        None if env.parse_or("PGHACKER_TRUST_FORWARDED", false)? => ClientIp::FirstForwardedFor,
        None => ClientIp::Socket,
    };
    let limit = RateLimit::new(
        env.parse_or("PGHACKER_RATE_PER_MINUTE", 120u32)?,
        env.parse_or("PGHACKER_RATE_BURST", 40u32)?,
        client_ip.clone(),
    );
    // The internal listener's: per connection peer, since agents choose
    // their headers. A deployment's agents all connect from one address, so
    // they share it.
    let internal_limit = RateLimit::new(
        env.parse_or("PGHACKER_INTERNAL_RATE_PER_MINUTE", 300u32)?,
        env.parse_or("PGHACKER_INTERNAL_RATE_BURST", 60u32)?,
        ClientIp::Socket,
    );
    let allowed_hosts = host_list(&env, "PGHACKER_ALLOWED_HOSTS");
    // The Host headers the internal listener accepts: loopback when unset;
    // in a cluster, its Service's names, which agents reach it by.
    let internal_allowed_hosts = host_list(&env, "PGHACKER_INTERNAL_ALLOWED_HOSTS");
    let server = McpServer::new(
        Server::new(pool, embedder, query),
        "pghacker-mcp",
        env!("CARGO_PKG_VERSION"),
        instructions(),
    )
    .cancel_on_disconnect()
    .with_call_timeout(call_timeout)
    .with_skills(
        tools::skills().context("the embedded skills")?,
        tools::EXTENSION_INSTRUCTIONS,
    )
    .context("serving the skills")?;
    let front_door = env
        .get("PGHACKER_FRONT_DOOR_ID")
        .map(|id| FrontDoor(Arc::from(id.trim())));
    let admission = Admission::new(calls, calls_per_client, pool_wait, client_ip.clone());
    // Feedback on the public listener only: no person reads the answers of
    // the internal one's agents.
    let app = guarded(
        public(
            router(server.clone().with_feedback(feedback), allowed_hosts),
            limit,
            admission,
            &usage,
            client_ip,
            front_door,
        ),
        pool_wait + call_timeout + REQUEST_MARGIN,
    );
    let served = match internal_addr {
        Some(internal_addr) => {
            let internal_app = guarded(
                internal(
                    router(server, internal_allowed_hosts),
                    internal_limit,
                    &usage,
                ),
                call_timeout + REQUEST_MARGIN,
            );
            tokio::try_join!(serve(app, &addr), serve(internal_app, &internal_addr)).map(|_| ())
        }
        None => serve(app, &addr).await,
    };
    usage.shutdown().await;
    served
}

/// The Host headers a listener accepts, from `name`'s comma-separated list;
/// unset, none but loopback's.
fn host_list(env: &Env, name: &str) -> Option<Vec<String>> {
    env.get(name).map(|hosts| {
        hosts
            .split(',')
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
            .collect()
    })
}

/// `app` with a deadline for each response, a backstop past the calls' own
/// for a request that stalls anywhere, such as one whose body is slow to
/// come, answered 503; and a panic answered 500 rather than with a dropped
/// connection. The deadline bounds the time to the response's head only, so
/// it couldn't cut a stream short; and the stateless `/mcp` serves none: a
/// call's answer is one JSON body, and GET is refused.
fn guarded(app: Router, request_timeout: Duration) -> Router {
    app.layer(TimeoutLayer::with_status_code(
        StatusCode::SERVICE_UNAVAILABLE,
        request_timeout,
    ))
    .layer(CatchPanicLayer::new())
}

/// The public listener: `router` behind the limit on calls at once, then the
/// rate limit, then the usage middleware, which so records the requests the
/// limits turn away, then, behind Azure Front Door, whose addresses other
/// profiles share, the check that requests came through this profile
/// (PGHACKER_FRONT_DOOR_ID). Requests it refuses aren't recorded: their
/// client address can't be trusted.
fn public(
    router: Router,
    limit: RateLimit,
    admission: Admission,
    usage: &Usage,
    client_ip: ClientIp,
    front_door: Option<FrontDoor>,
) -> Router {
    let recorder = Recorder::new(usage.clone(), Endpoint::Public, move |request| {
        client_ip.of(request)
    });
    let app = router
        .layer(middleware::from_fn_with_state(admission, admission::admit))
        .layer(middleware::from_fn_with_state(limit, ratelimit::limit))
        .layer(middleware::from_fn_with_state(recorder, usage::record));
    match front_door {
        Some(front_door) => app.layer(middleware::from_fn_with_state(
            front_door,
            ratelimit::front_door,
        )),
        None => app,
    }
}

/// The internal listener, for agents: `router` behind a rate limit of its
/// own, which should take the connection's peer as the client (main gives it
/// `ClientIp::Socket`), then the usage middleware. Its requests, those the
/// limit turns away too, are recorded as internal, with no client address,
/// so no location or user.
fn internal(router: Router, limit: RateLimit, usage: &Usage) -> Router {
    let recorder = Recorder::new(usage.clone(), Endpoint::Internal, |_| None);
    router
        .layer(middleware::from_fn_with_state(limit, ratelimit::limit))
        .layer(middleware::from_fn_with_state(recorder, usage::record))
}

#[cfg(test)]
mod tests;
