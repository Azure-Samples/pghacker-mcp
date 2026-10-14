//! pghacker-indexer: async workers that retrieve sources (monthly mbox
//! archives, seed threads, git snapshots, the commit history), embed chunks,
//! resolve anchors and
//! project the tables into the AGE graph; and the MCP tools that crawler
//! sessions use to manage sources, watch progress and submit extracts.

pub mod anchors;
pub mod embedding;
pub mod extraction;
pub mod graph;
pub mod history;
pub mod jobs;
pub mod repair;
pub mod settings;
pub mod slices;
pub mod sources;
pub mod status;
pub mod tools;

use std::sync::Arc;

use anyhow::Result;
use pghacker_core::db::{self, Pool};
use pghacker_core::embed::Embedder;
use pghacker_mail::{ArchiveClient, ArchiveConfig};

use crate::settings::Settings;
use crate::slices::Slices;

/// What the workers and tools share. Cheap to clone.
#[derive(Clone)]
pub struct Indexer {
    pub pool: Pool,
    pub settings: Arc<Settings>,
    pub slices: Arc<Slices>,
    pub embedder: Option<Embedder>,
    pub archive: ArchiveClient,
}

impl Indexer {
    pub fn new(settings: Settings, env: &pghacker_core::config::Env) -> Result<Self> {
        let pool = db::pool(
            &settings.database_url,
            settings.pool_size,
            "pghacker-indexer",
        )?;
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
            settings: Arc::new(settings),
            slices: Arc::new(slices),
            embedder,
            archive,
        })
    }
}
