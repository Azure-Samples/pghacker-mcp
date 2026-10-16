// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The history's clone: every branch and tag of PostgreSQL's repository,
//! fetched into a bare repository that syncs read.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow};
use gix::remote::Direction;

/// Branches and tags, mirrored: branches stay branches in a bare clone.
const REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

/// Fetches every branch and tag of `url` into the bare repository at `dir`,
/// creating it on first use (a full clone: about 850 MB for PostgreSQL).
/// Blocking.
pub fn update_clone(url: &str, dir: &Path) -> Result<()> {
    let repo = if dir.join("HEAD").exists() {
        gix::open(dir).with_context(|| format!("cannot open {}", dir.display()))?
    } else {
        std::fs::create_dir_all(dir)?;
        gix::init_bare(dir).with_context(|| format!("cannot create {}", dir.display()))?
    };
    let interrupt = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let remote = repo
        .remote_at(url)
        .with_context(|| format!("{url} is not a git URL"))?
        .with_refspecs(REFSPECS, Direction::Fetch)
        .map_err(|e| anyhow!("cannot use the refspecs: {e}"))?;
    let outcome = remote
        .connect(Direction::Fetch)
        .with_context(|| format!("cannot connect to {url}"))?
        .prepare_fetch(gix::progress::Discard, Default::default())
        .context("cannot prepare the fetch")?
        .receive(gix::progress::Discard, &interrupt)
        .with_context(|| format!("cannot fetch from {url}"))?;
    tracing::info!(
        %url,
        refs = outcome.ref_map.mappings.len(),
        secs = started.elapsed().as_secs(),
        "fetched the history's clone"
    );
    Ok(())
}
