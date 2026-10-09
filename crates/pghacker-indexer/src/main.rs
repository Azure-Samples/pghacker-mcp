//! pghacker-indexer's command line: `bootstrap` (as the database's
//! administrator), `serve` (workers and the MCP server), `status` and the
//! local-only `reset`.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use pghacker_core::config::Env;
use pghacker_core::mcp::{McpServer, router, serve, shutdown_signal};
use pghacker_core::{db, migrate, telemetry};
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, jobs, status, tools};

#[derive(Parser)]
#[command(name = "pghacker-indexer", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Creates the database if it's missing, then the roles, extensions,
    /// schema and graph (as the administrator: PGHACKER_ADMIN_DATABASE_URL),
    /// then migrates.
    Bootstrap,
    /// Migrates, then runs the job workers and the MCP server.
    Serve,
    /// Prints ingest_status.
    Status,
    /// Drops schema pghacker and the graph, then bootstraps again. Local only.
    Reset {
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    telemetry::init();
    let cli = Cli::parse();
    let env = Env::process();
    match cli.command {
        Command::Bootstrap => bootstrap(&env).await,
        Command::Serve => run(&env).await,
        Command::Status => {
            let indexer = Indexer::new(Settings::from_env(&env)?, &env)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&status::ingest_status(&indexer).await?)?
            );
            Ok(())
        }
        Command::Reset { yes } => {
            if !yes {
                bail!("reset drops every pghacker table and the graph; pass --yes");
            }
            let admin = db::connect(&env.admin_database_url()?, "pghacker-indexer").await?;
            migrate::reset(&admin).await?;
            bootstrap(&env).await
        }
    }
}

async fn bootstrap(env: &Env) -> Result<()> {
    let admin_url = env.admin_database_url()?;
    migrate::create_database_if_missing(&admin_url)
        .await
        .context("bootstrap creates the database")?;
    let mut admin = db::connect(&admin_url, "pghacker-indexer")
        .await
        .context("bootstrap connects as the administrator")?;
    migrate::bootstrap(&mut admin, &env.passwords()?).await?;
    let mut indexer = db::connect(&env.indexer_database_url()?, "pghacker-indexer").await?;
    let applied = migrate::migrate(&mut indexer).await?;
    tracing::info!(?applied, "bootstrapped and migrated");
    Ok(())
}

async fn run(env: &Env) -> Result<()> {
    let settings = Settings::from_env(env)?;
    {
        let mut client = db::connect(&settings.database_url, "pghacker-indexer").await?;
        let applied = migrate::migrate(&mut client).await?;
        if !applied.is_empty() {
            tracing::info!(?applied, "migrated");
        }
    }
    let indexer = Indexer::new(settings, env)?;
    let shutdown = CancellationToken::new();
    // The tools serve at once; the workers wait until no other indexer runs
    // the jobs, as while a new deployment replaces the old.
    let workers = tokio::spawn({
        let indexer = indexer.clone();
        let shutdown = shutdown.clone();
        async move {
            let Some(_leader) = jobs::lead(&indexer.settings.database_url, &shutdown).await? else {
                return Ok(());
            };
            let workers = indexer.settings.workers;
            jobs::run(indexer, workers, shutdown).await
        }
    });
    let server = McpServer::new(
        indexer.clone(),
        "pghacker-indexer",
        env!("CARGO_PKG_VERSION"),
        tools::INSTRUCTIONS,
    );
    let app = router(server, indexer.settings.allowed_hosts.clone());
    let addr = indexer.settings.addr.clone();
    let served = tokio::spawn(async move { serve(app, &addr).await });
    shutdown_signal().await;
    tracing::info!("shutting down; finishing running jobs");
    shutdown.cancel();
    served.abort();
    match tokio::time::timeout(std::time::Duration::from_secs(20), workers).await {
        Ok(result) => result??,
        Err(_) => {
            tracing::warn!("jobs still running after 20 s; they'll be requeued at the next start")
        }
    }
    Ok(())
}
