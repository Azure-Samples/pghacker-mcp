//! pghacker-indexer's command line: `bootstrap` (as the database's
//! administrator), `serve` (workers and the MCP server), `status`,
//! `graph-reconcile`, the local-only `reset`, and `history` (building,
//! syncing and checking the commit history).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use pghacker_core::config::Env;
use pghacker_core::mcp::{McpServer, router, serve, shutdown_signal};
use pghacker_core::{db, migrate, telemetry};
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, graph, jobs, status, tools};

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
    /// Asks for a full graph sync, which recomputes the whole graph from the
    /// tables and repairs any difference; the graph sync job runs it next.
    GraphReconcile,
    /// Drops schema pghacker and the graph, then bootstraps again. Local only.
    Reset {
        #[arg(long)]
        yes: bool,
    },
    /// The commit history: builds it into a schema of its own, installs and
    /// syncs it, and checks it against git (as the administrator).
    History {
        #[command(subcommand)]
        action: History,
    },
}

#[derive(Subcommand)]
enum History {
    /// Walks every branch and tag of a clone into the schema, afresh.
    Build {
        /// A clone with every branch and tag.
        #[arg(long)]
        git_dir: PathBuf,
        #[arg(long, default_value = "pghacker_history")]
        schema: String,
        #[arg(long, default_value = "master")]
        master: String,
        /// A path prefix to keep (repeatable); none keeps the whole tree.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Walk without storing, and print the walk's numbers.
        #[arg(long)]
        dry_run: bool,
        /// Swap the build into schema pghacker when it is done, and ask for
        /// a full graph sync: the graph's files come from the history.
        #[arg(long)]
        install: bool,
        /// Where syncs fetch new commits from.
        #[arg(long)]
        url: Option<String>,
        /// Fetch every branch and tag from --url into --git-dir first,
        /// creating a bare clone there if there's none.
        #[arg(long, requires = "url")]
        fetch: bool,
    },
    /// Applies the clone's new commits to the history.
    Sync {
        #[arg(long)]
        git_dir: PathBuf,
        /// The build's schema, or pghacker once installed.
        #[arg(long, default_value = "pghacker")]
        schema: String,
        /// Fetch every branch and tag from the history's URL first.
        #[arg(long)]
        fetch: bool,
        /// New commits per transaction.
        #[arg(long, default_value_t = 200)]
        batch: usize,
    },
    /// Groups the commits into back-patch groups afresh.
    Regroup {
        /// The build's schema, or pghacker once installed.
        #[arg(long, default_value = "pghacker")]
        schema: String,
    },
    /// Rebuilds every file at commits and checks it against git.
    Verify {
        #[arg(long)]
        git_dir: PathBuf,
        /// The build's schema, or pghacker once installed.
        #[arg(long, default_value = "pghacker_history")]
        schema: String,
        /// A revision to check (repeatable).
        #[arg(long = "rev")]
        revs: Vec<String>,
        /// This many commits more, picked at random.
        #[arg(long, default_value_t = 0)]
        sample: u32,
        /// Every branch head too.
        #[arg(long)]
        heads: bool,
        /// Also compare index and sequential scans.
        #[arg(long)]
        scans: bool,
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
        Command::GraphReconcile => {
            let settings = Settings::from_env(&env)?;
            let client = db::connect(&settings.database_url, "pghacker-indexer").await?;
            graph::request_full(&client, "requested from the command line").await?;
            jobs::wake(&client, jobs::kind::GRAPH_SYNC).await?;
            println!("A full graph sync is requested; the graph sync job runs it next.");
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
        Command::History { action } => history(&env, action).await,
    }
}

async fn history(env: &Env, action: History) -> Result<()> {
    use pghacker_code::history;
    let json = match action {
        History::Build {
            git_dir,
            schema,
            master,
            paths,
            dry_run,
            install,
            url,
            fetch,
        } => {
            if fetch {
                let repo_url = url.clone().context("--fetch needs --url")?;
                let dir = git_dir.clone();
                tokio::task::spawn_blocking(move || history::fetch::update_clone(&repo_url, &dir))
                    .await??;
            }
            let opts = history::BuildOptions {
                git_dir,
                master,
                url,
                schema,
                paths,
                install,
            };
            if dry_run {
                let started = std::time::Instant::now();
                let (graph, arena, stats) =
                    tokio::task::spawn_blocking(move || history::walk_only(&opts)).await??;
                let widest = (0..arena.heads.len() as u32)
                    .map(|l| (arena.weave(l).len(), l))
                    .max()
                    .map(|(n, l)| (n, arena.lineages[l as usize].path.clone()));
                serde_json::json!({
                    "walk": stats,
                    "commits": graph.commits.len(),
                    "lanes": graph.lanes.len(),
                    "refs": graph.refs.len(),
                    "instances": arena.instances(),
                    "versions": arena.versions.len(),
                    "lineages": arena.lineages.len(),
                    "widest_lineage": widest,
                    "secs": started.elapsed().as_secs(),
                })
            } else {
                let database_url = env.admin_database_url()?;
                let stats = history::build(&database_url, &opts).await?;
                if opts.install {
                    // The graph's files come from the history, which the
                    // install swapped in whole.
                    let client = db::connect(&database_url, "pghacker-indexer").await?;
                    client
                        .batch_execute("SET search_path = pghacker, ag_catalog, public")
                        .await?;
                    graph::request_full(&client, "the commit history was installed").await?;
                    jobs::wake(&client, jobs::kind::GRAPH_SYNC).await?;
                }
                serde_json::to_value(stats)?
            }
        }
        History::Sync {
            git_dir,
            schema,
            fetch,
            batch,
        } => {
            let url = env.admin_database_url()?;
            let client = db::connect(&url, "pghacker-indexer").await?;
            if fetch {
                let row = client
                    .query_one(&format!("SELECT url FROM {schema}.code_history"), &[])
                    .await
                    .context("the schema has no history")?;
                let repo_url: Option<String> = row.get(0);
                let repo_url = repo_url.context("the history has no URL to fetch from")?;
                let dir = git_dir.clone();
                tokio::task::spawn_blocking(move || history::fetch::update_clone(&repo_url, &dir))
                    .await??;
            }
            // The installed history's new files reach the graph, as the job's do.
            let capture = if schema == pghacker_core::code::DEFAULT_HISTORY_SCHEMA {
                let recorder = db::connect(&url, "pghacker-indexer").await?;
                let newest = pghacker_indexer::history::newest_version(&recorder).await?;
                Some((recorder, newest))
            } else {
                None
            };
            let synced = history::sync::sync(
                client,
                history::sync::SyncOptions {
                    git_dir,
                    schema,
                    batch,
                },
            )
            .await;
            if let Some((recorder, newest)) = capture {
                pghacker_indexer::history::capture_files(&recorder, newest).await?;
            }
            serde_json::to_value(synced?)?
        }
        History::Regroup { schema } => {
            let client = db::connect(&env.admin_database_url()?, "pghacker-indexer").await?;
            let grouped = history::backpatch::regroup(&client, &schema).await?;
            serde_json::json!({ "schema": schema, "backpatched": grouped })
        }
        History::Verify {
            git_dir,
            schema,
            revs,
            sample,
            heads,
            scans,
        } => {
            let opts = history::VerifyOptions {
                git_dir,
                schema,
                revs,
                sample,
                heads,
                scans,
            };
            serde_json::to_value(history::verify_build(&env.admin_database_url()?, &opts).await?)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
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
    )
    .cancel_on_disconnect();
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
