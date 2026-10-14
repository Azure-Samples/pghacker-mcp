//! The indexer's settings, from the environment the launcher builds.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use pghacker_core::config::Env;

use crate::sources::SourcesMode;

#[derive(Clone, Debug)]
pub struct Settings {
    /// pgh_indexer's URL.
    pub database_url: String,
    /// Where the MCP server listens: `PGHACKER_INDEXER_ADDR`.
    pub addr: String,
    /// Concurrent job workers: `PGHACKER_WORKERS`. They have connections of
    /// their own, one each and two to spare.
    pub workers: usize,
    /// Connections for tool calls, apart from the workers' so that no call
    /// can keep the jobs waiting: `PGHACKER_TOOL_CONNECTIONS`, 8 by default.
    pub tool_connections: usize,
    /// How long a tool call's statement may run before it's cancelled:
    /// `PGHACKER_TOOL_TIMEOUT_SECONDS`, 30 by default.
    pub tool_timeout: Duration,
    /// The test slices: `PGHACKER_SLICES`.
    pub slices_path: PathBuf,
    /// The HTTP cache: `PGHACKER_CACHE_DIR`.
    pub cache_dir: PathBuf,
    /// Git clones, such as the commit history's: `PGHACKER_GIT_DIR`.
    pub git_dir: PathBuf,
    /// Accepted Host headers, comma-separated, `*` for any:
    /// `PGHACKER_ALLOWED_HOSTS`. Unset keeps loopback only.
    pub allowed_hosts: Option<Vec<String>>,
    /// What add_source may add: `PGHACKER_SOURCES`, `test-slices` (the
    /// default) or `open`.
    pub sources: SourcesMode,
    /// Whether a scheduler runs extraction sessions by itself:
    /// `PGHACKER_EXTRACTION`, `manual` (the default) or `automatic`.
    pub extraction_automatic: bool,
    /// How often a full graph sync reconciles the graph with the tables:
    /// `PGHACKER_GRAPH_RECONCILE_HOURS`, 168 by default; 0 never does.
    pub graph_reconcile_every: Option<Duration>,
}

impl Settings {
    pub fn from_env(env: &Env) -> Result<Self> {
        let workers = env.parse_or("PGHACKER_WORKERS", 5usize)?.max(1);
        Ok(Self {
            database_url: env.indexer_database_url()?,
            addr: env
                .get_or("PGHACKER_INDEXER_ADDR", "127.0.0.1:3101")
                .to_owned(),
            workers,
            tool_connections: env.parse_or("PGHACKER_TOOL_CONNECTIONS", 8usize)?.max(1),
            tool_timeout: Duration::from_secs(
                env.parse_or("PGHACKER_TOOL_TIMEOUT_SECONDS", 30u64)?.max(1),
            ),
            slices_path: env
                .get_or("PGHACKER_SLICES", "config/test-slices.toml")
                .into(),
            cache_dir: env.get_or("PGHACKER_CACHE_DIR", ".runtime/cache").into(),
            git_dir: env.get_or("PGHACKER_GIT_DIR", ".runtime/git").into(),
            allowed_hosts: env.get("PGHACKER_ALLOWED_HOSTS").map(|hosts| {
                hosts
                    .split(',')
                    .map(|host| host.trim().to_owned())
                    .filter(|host| !host.is_empty())
                    .collect()
            }),
            sources: env.get_or("PGHACKER_SOURCES", "test-slices").parse()?,
            extraction_automatic: match env.get_or("PGHACKER_EXTRACTION", "manual").trim() {
                "manual" => false,
                "automatic" => true,
                other => {
                    anyhow::bail!("PGHACKER_EXTRACTION must be manual or automatic, not {other:?}")
                }
            },
            graph_reconcile_every: match env.parse_or("PGHACKER_GRAPH_RECONCILE_HOURS", 168u64)? {
                0 => None,
                hours => Some(Duration::from_secs(hours * 3600)),
            },
        })
    }
}
