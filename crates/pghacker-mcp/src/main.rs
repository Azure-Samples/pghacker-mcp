//! pghacker-mcp: the public MCP server. Stateless, read-only (pgh_reader),
//! rate-limited per client, no LLM calls beyond query embeddings.

mod ratelimit;
mod tools;

use anyhow::Result;
use axum::middleware;

use pghacker_core::config::Env;
use pghacker_core::embed::Embedder;
use pghacker_core::mcp::{McpServer, router, serve};
use pghacker_core::{db, telemetry};

use crate::ratelimit::RateLimit;
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
    let limit = RateLimit::new(
        env.parse_or("PGHACKER_RATE_PER_MINUTE", 120u32)?,
        env.parse_or("PGHACKER_RATE_BURST", 40u32)?,
        env.parse_or("PGHACKER_TRUST_FORWARDED", false)?,
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
    let app = router(server, allowed_hosts)
        .layer(middleware::from_fn_with_state(limit, ratelimit::limit));
    serve(app, &addr).await
}
