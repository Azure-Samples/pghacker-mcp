// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The indexer's settings, from the environment the launcher builds.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use pghacker_core::config::Env;

use crate::lanes::Order;
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
    /// The order next_extraction_batch leases threads in:
    /// `PGHACKER_EXTRACTION_ORDER`, `lanes` (the default) or `largest`.
    pub extraction_order: Order,
    /// New mail's share of the threads leased while the backfill has threads
    /// too: `PGHACKER_EXTRACTION_NEW_MAIL_SHARE`, above 0 and up to 1, 0.5 by
    /// default.
    pub new_mail_share: f64,
    /// How recent a thread's latest message must be for the thread to be new
    /// mail: `PGHACKER_EXTRACTION_NEW_MAIL_DAYS`, 1 to 3650 days, 7 by
    /// default.
    pub new_mail_days: i32,
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
            extraction_order: env.get_or("PGHACKER_EXTRACTION_ORDER", "lanes").parse()?,
            new_mail_share: match env.parse_or("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", 0.5f64)? {
                share if share > 0.0 && share <= 1.0 => share,
                share => anyhow::bail!(
                    "PGHACKER_EXTRACTION_NEW_MAIL_SHARE must be above 0 and up to 1, not {share}"
                ),
            },
            new_mail_days: match env.parse_or("PGHACKER_EXTRACTION_NEW_MAIL_DAYS", 7i32)? {
                days @ 1..=3650 => days,
                days => anyhow::bail!(
                    "PGHACKER_EXTRACTION_NEW_MAIL_DAYS must be from 1 to 3650, not {days}"
                ),
            },
            graph_reconcile_every: match env.parse_or("PGHACKER_GRAPH_RECONCILE_HOURS", 168u64)? {
                0 => None,
                hours => Some(Duration::from_secs(hours * 3600)),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Result<Settings> {
        let mut all = vec![(
            "PGHACKER_INDEXER_DATABASE_URL",
            "postgresql://pgh_indexer:secret@127.0.0.1/pghacker",
        )];
        all.extend_from_slice(pairs);
        Settings::from_env(&Env::from_pairs(all))
    }

    #[test]
    fn the_extraction_order_defaults_to_the_lanes_and_checks_its_settings() {
        let defaults = settings(&[]).unwrap();
        assert_eq!(defaults.extraction_order, Order::Lanes);
        assert_eq!(defaults.new_mail_share, 0.5);
        assert_eq!(defaults.new_mail_days, 7);
        let set = settings(&[
            ("PGHACKER_EXTRACTION_ORDER", "largest"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "1"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "3"),
        ])
        .unwrap();
        assert_eq!(set.extraction_order, Order::Largest);
        assert_eq!(set.new_mail_share, 1.0);
        assert_eq!(set.new_mail_days, 3);
        for (name, value) in [
            ("PGHACKER_EXTRACTION_ORDER", "newest"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "0"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "1.5"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "NaN"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_SHARE", "half"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "0"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "3651"),
            ("PGHACKER_EXTRACTION_NEW_MAIL_DAYS", "2.5"),
        ] {
            let error = settings(&[(name, value)]).unwrap_err();
            assert!(error.to_string().contains(name), "{name}={value}: {error}");
        }
    }
}
