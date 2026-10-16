// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-indexer: async workers that retrieve sources (monthly mbox
//! archives, seed threads, the commit history's new commits), embed chunks,
//! resolve anchors, find duplicate candidates among the extracts and
//! project the tables into the AGE graph; and the MCP tools that crawler
//! sessions use to manage sources, watch progress and submit extracts.

pub mod addresses;
pub mod anchors;
pub mod duplicates;
pub mod embedding;
pub mod extraction;
pub mod graph;
pub mod history;
pub mod jobs;
pub mod lanes;
pub mod repair;
pub mod rethread;
pub mod settings;
pub mod slices;
pub mod sources;
pub mod status;
pub mod tools;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use pghacker_core::calls::CallPool;
use pghacker_core::db::{self, Pool};
use pghacker_core::embed::Embedder;
use pghacker_mail::{ArchiveClient, ArchiveConfig};
use tokio::sync::Semaphore;

use crate::lanes::Lanes;
use crate::settings::Settings;
use crate::slices::Slices;

/// How long a tool call waits for a connection before it fails as busy.
const TOOL_CONNECTION_WAIT: Duration = Duration::from_secs(10);

/// find_duplicates calls that may run at once, of either kind.
pub const FIND_DUPLICATES_AT_ONCE: usize = 2;

/// What the workers and tools share. Cheap to clone.
#[derive(Clone)]
pub struct Indexer {
    /// The job workers' connections.
    pub pool: Pool,
    /// The tool calls' connections, apart from the workers': each statement
    /// may run `settings.tool_timeout`, and a call whose client goes away has
    /// its statements cancelled.
    pub tools: CallPool,
    /// Room for find_duplicates calls ([`FIND_DUPLICATES_AT_ONCE`]).
    pub find_duplicates: Arc<Semaphore>,
    /// The extraction lanes' share of the threads leased.
    pub lanes: Arc<Lanes>,
    pub settings: Arc<Settings>,
    pub slices: Arc<Slices>,
    pub embedder: Option<Embedder>,
    pub archive: ArchiveClient,
}

impl Indexer {
    pub fn new(settings: Settings, env: &pghacker_core::config::Env) -> Result<Self> {
        let pool = db::pool(
            &settings.database_url,
            settings.workers + 2,
            "pghacker-indexer",
        )?;
        let tools = CallPool::new(
            db::pool_with_options(
                &settings.database_url,
                settings.tool_connections,
                "pghacker-indexer tools",
                &format!("-c statement_timeout={}", settings.tool_timeout.as_millis()),
            )?,
            TOOL_CONNECTION_WAIT,
        );
        let slices = Slices::load(&settings.slices_path)?;
        let embedder = Embedder::from_env(env)?;
        if embedder.is_none() {
            tracing::warn!(
                "HORIZON_EMBED_* is not set: semantic and hybrid search degrade to full text"
            );
        }
        let archive =
            ArchiveClient::new(ArchiveConfig::from_env(env, settings.cache_dir.clone())?)?;
        Ok(Self {
            pool,
            tools,
            find_duplicates: Arc::new(Semaphore::new(FIND_DUPLICATES_AT_ONCE)),
            lanes: Arc::new(Lanes::default()),
            settings: Arc::new(settings),
            slices: Arc::new(slices),
            embedder,
            archive,
        })
    }
}
