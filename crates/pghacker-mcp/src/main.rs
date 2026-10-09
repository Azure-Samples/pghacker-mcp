//! pghacker-mcp: the public MCP server. Stateless, read-only (pgh_reader),
//! rate-limited per client, no LLM calls beyond query embeddings. An
//! optional second listener, for agents in the same deployment, serves
//! loopback clients without the limit.

mod ratelimit;
mod tools;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::http::HeaderName;
use axum::middleware;

use pghacker_core::config::Env;
use pghacker_core::embed::Embedder;
use pghacker_core::mcp::{McpServer, router, serve};
use pghacker_core::{db, telemetry};

use crate::ratelimit::{ClientIp, FrontDoor, RateLimit};
use crate::tools::{INSTRUCTIONS, Server};

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
    // The client's address: a header the proxy in front sets
    // (PGHACKER_CLIENT_IP_HEADER, such as x-azure-clientip), the first
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
        client_ip,
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
        INSTRUCTIONS,
    );
    let mut app = router(server.clone(), allowed_hosts)
        .layer(middleware::from_fn_with_state(limit, ratelimit::limit));
    // Behind Azure Front Door, whose addresses other profiles share: only
    // this profile's requests (PGHACKER_FRONT_DOOR_ID).
    if let Some(id) = env.get("PGHACKER_FRONT_DOOR_ID") {
        app = app.layer(middleware::from_fn_with_state(
            FrontDoor(Arc::from(id.trim())),
            ratelimit::front_door,
        ));
    }
    match env.get("PGHACKER_MCP_INTERNAL_ADDR") {
        Some(internal) => {
            let internal = internal.to_owned();
            tokio::try_join!(serve(app, &addr), serve(router(server, None), &internal))?;
            Ok(())
        }
        None => serve(app, &addr).await,
    }
}
