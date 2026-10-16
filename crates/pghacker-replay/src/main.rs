// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-replay: the exactness gate for code search. It replays the
//! search study's recorded agent searches, and generated PG19 searches,
//! through `search` and `grep`, and compares their lines with ripgrep's and
//! GNU grep's on the same files. Each replayed commit is served from a commit
//! history of its own (see the `tree` module).
//!
//! The study's data is private: it is read at run time from the folder given
//! by `--study-dir` or `PGHACKER_STUDY_DIR`, and the clones of the
//! repositories it searched from the folders that `PGHACKER_REPLAY_<NAME>`
//! variables name. Per-search details go only to `.runtime/replay/` (ignored
//! by git). The console shows aggregates.

mod gitstats;
mod gnu;
mod pg19;
mod rg;
mod study;
mod tree;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use pghacker_core::config::{Env, repo_root, with_database};
use pghacker_core::db;
use pghacker_core::migrate;

/// The replay database.
pub const DATABASE: &str = "pghacker_replay";

#[derive(Parser)]
#[command(
    about = "Replays agent searches through code search and checks them against ripgrep and GNU grep"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Creates and migrates the pghacker_replay database.
    Prepare,
    /// Replays the study's recorded searches on the study's commits.
    Study {
        /// The study's folder, with data/trgm/load.json and replay.json.
        #[arg(long, env = "PGHACKER_STUDY_DIR")]
        study_dir: Option<PathBuf>,
        /// Replay only the first N searches.
        #[arg(long)]
        limit: Option<usize>,
        /// Searches run at once.
        #[arg(long, default_value_t = 8)]
        jobs: usize,
        /// Skip the GNU grep run that classifies grep-path differences.
        #[arg(long)]
        skip_gnu: bool,
    },
    /// Serves the PG19 subset from a history and replays generated searches.
    Pg19 {
        /// Searches to generate.
        #[arg(long, default_value_t = 2000)]
        count: usize,
        /// The generator's seed.
        #[arg(long, default_value_t = 19)]
        seed: u64,
        /// Searches run at once.
        #[arg(long, default_value_t = 8)]
        jobs: usize,
    },
}

/// Paths and settings shared by the subcommands.
pub struct Ctx {
    pub env: Env,
    pub runtime: PathBuf,
    pub rg: PathBuf,
}

impl Ctx {
    fn new() -> Result<Self> {
        let env = Env::local()?;
        let root = repo_root().context("cannot find the repository root")?;
        let runtime = root.join(".runtime").join("replay");
        std::fs::create_dir_all(&runtime)?;
        let rg = find_rg(&env);
        Ok(Self { env, runtime, rg })
    }

    pub fn admin_url(&self) -> Result<String> {
        with_database(&self.env.admin_database_url()?, DATABASE)
    }

    pub fn indexer_url(&self) -> Result<String> {
        with_database(&self.env.indexer_database_url()?, DATABASE)
    }

    pub fn reader_url(&self) -> Result<String> {
        with_database(&self.env.reader_database_url()?, DATABASE)
    }
}

/// ripgrep: `PGHACKER_RG`, the newest bundled Copilot ripgrep, or `rg`.
fn find_rg(env: &Env) -> PathBuf {
    if let Some(rg) = env.get("PGHACKER_RG") {
        return PathBuf::from(rg);
    }
    if let Some(home) = env.get("HOME") {
        let base = Path::new(home).join(".copilot/pkg/universal");
        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            versions.sort();
            let arch = if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            };
            for v in versions.iter().rev() {
                let rg = v.join(format!("ripgrep/bin/darwin-{arch}/rg"));
                if rg.is_file() {
                    return rg;
                }
            }
        }
    }
    PathBuf::from("rg")
}

/// Creates the replay database if missing, bootstraps and migrates it.
async fn prepare(ctx: &Ctx) -> Result<()> {
    let admin = ctx.env.admin_database_url()?;
    let maintenance = db::connect(&with_database(&admin, "postgres")?, "pghacker-replay").await?;
    let exists = maintenance
        .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&DATABASE])
        .await?
        .is_some();
    if !exists {
        maintenance
            .batch_execute(&format!("CREATE DATABASE {DATABASE}"))
            .await
            .context("cannot create the replay database")?;
        println!("created database {DATABASE}");
    }
    let mut admin = db::connect(&ctx.admin_url()?, "pghacker-replay").await?;
    if !migrate::roles_exist(&admin).await? {
        migrate::bootstrap_roles(&mut admin, &ctx.env.passwords()?).await?;
    }
    migrate::bootstrap_database(&mut admin).await?;
    let mut indexer = db::connect(&ctx.indexer_url()?, "pghacker-replay").await?;
    let applied = migrate::migrate(&mut indexer).await?;
    println!(
        "{DATABASE} is migrated{}",
        if applied.is_empty() {
            String::new()
        } else {
            format!(" (applied {})", applied.join(", "))
        }
    );
    Ok(())
}

/// The value at quantile `q` of sorted values.
pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)]
}

#[tokio::main]
async fn main() -> Result<()> {
    pghacker_core::telemetry::init();
    let cli = Cli::parse();
    let ctx = Ctx::new()?;
    match cli.command {
        Command::Prepare => prepare(&ctx).await,
        Command::Study {
            study_dir,
            limit,
            jobs,
            skip_gnu,
        } => {
            let dir =
                match study_dir.or_else(|| ctx.env.get("PGHACKER_STUDY_DIR").map(PathBuf::from)) {
                    Some(dir) => dir,
                    None => bail!("give --study-dir or set PGHACKER_STUDY_DIR"),
                };
            study::run(&ctx, &dir, limit, jobs.max(1), skip_gnu).await
        }
        Command::Pg19 { count, seed, jobs } => pg19::run(&ctx, count, seed, jobs.max(1)).await,
    }
}
