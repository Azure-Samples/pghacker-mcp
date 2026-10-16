// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-mcp: the public MCP server. Stateless, read-only (pgh_reader),
//! rate-limited per client, no LLM calls beyond query embeddings. It serves
//! the repository's skills too (`skills/`). An optional second listener, for
//! agents in the same deployment, serves loopback clients without the limit.
//! With PGHACKER_USAGE_ENDPOINT set, both listeners record usage
//! (`pghacker_core::usage`), and with PGHACKER_FEEDBACK_RATE above 0 too, the
//! public one now and then asks whether an answer helped
//! (`pghacker_core::feedback`).

mod ratelimit;
mod tools;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::http::HeaderName;
use axum::middleware;

use pghacker_core::config::Env;
use pghacker_core::embed::Embedder;
use pghacker_core::feedback::Feedback;
use pghacker_core::mcp::{McpServer, router, serve};
use pghacker_core::usage::{self, Endpoint, Recorder, Usage};
use pghacker_core::{db, telemetry};

use crate::ratelimit::{ClientIp, FrontDoor, RateLimit};
use crate::tools::{Server, instructions};

#[tokio::main]
async fn main() -> Result<()> {
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
    let pool_size = env.parse_or("PGHACKER_MCP_POOL", 16usize)?;
    let pool = db::pool(&reader_url, pool_size, "pghacker-mcp")?;
    let embedder = Embedder::from_env(&env)?;
    if embedder.is_none() {
        tracing::warn!(
            "HORIZON_EMBED_* is not set: semantic search is off and hybrid falls back to full text"
        );
    }
    let query = pghacker_sqlquery::QueryTool::new(
        &reader_url,
        env.parse_or("PGHACKER_QUERY_POOL", 8usize)?,
    )?;
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
    let allowed_hosts = env.get("PGHACKER_ALLOWED_HOSTS").map(|hosts| {
        hosts
            .split(',')
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
            .collect()
    });
    let server = McpServer::new(
        Server::new(pool, embedder, query),
        "pghacker-mcp",
        env!("CARGO_PKG_VERSION"),
        instructions(),
    )
    .with_skills(tools::skills().context("the embedded skills")?);
    let front_door = env
        .get("PGHACKER_FRONT_DOOR_ID")
        .map(|id| FrontDoor(Arc::from(id.trim())));
    // Feedback on the public listener only: no person reads the answers of
    // the internal one's agents.
    let app = public(
        router(server.clone().with_feedback(feedback), allowed_hosts),
        limit,
        &usage,
        client_ip,
        front_door,
    );
    let served = match env.get("PGHACKER_MCP_INTERNAL_ADDR") {
        Some(internal_addr) => {
            let internal_addr = internal_addr.to_owned();
            let internal_app = internal(router(server, None), &usage);
            tokio::try_join!(serve(app, &addr), serve(internal_app, &internal_addr)).map(|_| ())
        }
        None => serve(app, &addr).await,
    };
    usage.shutdown().await;
    served
}

/// The public listener: `router` behind the rate limit, then the usage
/// middleware, which so records the requests the limit turns away, then,
/// behind Azure Front Door, whose addresses other profiles share, the check
/// that requests came through this profile (PGHACKER_FRONT_DOOR_ID). Requests
/// it refuses aren't recorded: their client address can't be trusted.
fn public(
    router: Router,
    limit: RateLimit,
    usage: &Usage,
    client_ip: ClientIp,
    front_door: Option<FrontDoor>,
) -> Router {
    let recorder = Recorder::new(usage.clone(), Endpoint::Public, move |request| {
        client_ip.of(request)
    });
    let app = router
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

/// The internal listener: its requests are recorded as internal, with no
/// client address, so no location or user.
fn internal(router: Router, usage: &Usage) -> Router {
    let recorder = Recorder::new(usage.clone(), Endpoint::Internal, |_| None);
    router.layer(middleware::from_fn_with_state(recorder, usage::record))
}

#[cfg(test)]
mod tests;
