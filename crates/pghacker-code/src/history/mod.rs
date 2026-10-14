//! Commit history: every line of every commit, stored once.
//!
//! Each line instance is one row with the addresses of the commits it is
//! alive in; each file version is one row the same way. A commit's tree is
//! the versions alive at its address, and a version's lines are its
//! lineage's instances alive there, in weave order. See [`addr`] for commit
//! addresses, [`graph`] for lanes, [`walk`] for the builder, [`store`] for
//! the tables and [`verify`] for the rebuild checks.
//!
//! A build writes a schema of its own and can be swapped into `pghacker`,
//! where the `code_history_sync` job applies new commits ([`sync`]).

pub mod addr;
pub mod backpatch;
pub mod fetch;
pub mod graph;
pub mod message;
pub mod store;
pub mod sync;
pub mod verify;
pub mod walk;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;

use graph::Graph;
use walk::{Arena, LineSink, WalkStats, Walker};

/// What to build.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// A clone with every branch and tag (bare or not).
    pub git_dir: PathBuf,
    /// The branch the others fork from.
    pub master: String,
    /// Where syncs fetch new commits from.
    pub url: Option<String>,
    /// The schema the build is written in.
    pub schema: String,
    /// Path prefixes to keep; empty keeps the whole tree.
    pub paths: Vec<String>,
    /// Swap the build into schema pghacker when it is done.
    pub install: bool,
}

/// What a build did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BuildStats {
    pub walk: WalkStats,
    pub stored: store::Stored,
    pub lanes: usize,
    pub refs: usize,
    pub texts: u64,
    pub walk_secs: u64,
    pub write_secs: u64,
    pub finish_secs: u64,
}

/// Opens a clone for the walk, with a generous object cache.
pub fn open(git_dir: &Path) -> Result<gix::Repository> {
    let mut repo =
        gix::open(git_dir).with_context(|| format!("cannot open {}", git_dir.display()))?;
    repo.object_cache_size_if_unset(256 << 20);
    Ok(repo)
}

const BATCH: usize = 20_000;

/// Sends new lines' text and lengths to the COPY in batches.
struct ChannelSink {
    batch: Vec<(u32, String, u32)>,
    tx: mpsc::Sender<Vec<(u32, String, u32)>>,
}

impl ChannelSink {
    fn flush(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let full = std::mem::replace(&mut self.batch, Vec::with_capacity(BATCH));
        self.tx
            .blocking_send(full)
            .map_err(|_| anyhow!("the text COPY stopped"))
    }
}

impl LineSink for ChannelSink {
    fn line(&mut self, id: u32, text: &str, bytes: u32) -> Result<()> {
        self.batch.push((id, text.to_owned(), bytes));
        if self.batch.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }
}

/// The path prefixes a build kept.
pub async fn built_paths(client: &tokio_postgres::Client, schema: &str) -> Result<Vec<String>> {
    let s = store::readable_schema(schema)?;
    let row = client
        .query_opt(&format!("SELECT paths FROM {s}.code_history"), &[])
        .await
        .with_context(|| format!("schema {s} has no history"))?
        .with_context(|| format!("schema {s} has no history build"))?;
    Ok(row.get(0))
}

/// Walks the history without storing it: the graph and the arena, to
/// measure or simulate.
pub fn walk_only(opts: &BuildOptions) -> Result<(Graph, Arena, WalkStats)> {
    let repo = open(&opts.git_dir)?;
    let graph = Graph::read(&repo, &opts.master)?;
    let mut walker = Walker::new(&repo, &opts.paths, |_: u32, _: &str, _: u32| Ok(()))?;
    walker.walk(&graph)?;
    let (ledger, stats) = walker.finish();
    Ok((graph, ledger.arena, stats))
}

/// Builds the history into `opts.schema` afresh.
pub async fn build(database_url: &str, opts: &BuildOptions) -> Result<BuildStats> {
    let mut client = pghacker_core::db::connect(database_url, "pghacker-history").await?;
    let texts_client = pghacker_core::db::connect(database_url, "pghacker-history-texts").await?;
    let schema = store::schema_name(&opts.schema)?.to_owned();
    store::create(&client, &schema).await?;

    let started = std::time::Instant::now();
    let (tx, rx) = mpsc::channel::<Vec<(u32, String, u32)>>(8);
    let copy = {
        let schema = schema.clone();
        tokio::spawn(async move { store::copy_texts(&texts_client, &schema, rx).await })
    };
    let walk_opts = opts.clone();
    let walked = tokio::task::spawn_blocking(move || -> Result<(Graph, Arena, WalkStats)> {
        let repo = open(&walk_opts.git_dir)?;
        let graph = Graph::read(&repo, &walk_opts.master)?;
        tracing::info!(
            commits = graph.commits.len(),
            lanes = graph.lanes.len(),
            refs = graph.refs.len(),
            "read the commit graph"
        );
        let sink = ChannelSink {
            batch: Vec::with_capacity(BATCH),
            tx,
        };
        let mut walker = Walker::new(&repo, &walk_opts.paths, sink)?;
        walker.walk(&graph)?;
        let (mut ledger, stats) = walker.finish();
        ledger.sink.flush()?;
        Ok((graph, ledger.arena, stats))
    })
    .await
    .context("the walk panicked")?;
    let texts = copy.await.context("the text COPY panicked")?;
    let (graph, arena, walk_stats) = walked?;
    let texts = texts?;
    let walk_secs = started.elapsed().as_secs();
    if texts != arena.instances() as u64 {
        bail!("copied {texts} lines of {}", arena.instances());
    }
    tracing::info!(
        instances = arena.instances(),
        versions = arena.versions.len(),
        lineages = arena.lineages.len(),
        secs = walk_secs,
        "walked the history"
    );

    let started = std::time::Instant::now();
    let mut stored = store::write(&mut client, &schema, &graph, &arena).await?;
    stored.backpatched = backpatch::regroup(&client, &schema).await?;
    let write_secs = started.elapsed().as_secs();
    let lanes = graph.lanes.len();
    let refs = graph.refs.len();
    let head = graph.commits[*graph.lanes[0].commits.last().context("master is empty")?]
        .id
        .to_string();
    drop(arena);
    drop(graph);
    let started = std::time::Instant::now();
    let mut stats = BuildStats {
        walk: walk_stats,
        stored,
        lanes,
        refs,
        texts,
        walk_secs,
        write_secs,
        finish_secs: 0,
    };
    let built = store::Built {
        master: &opts.master,
        url: opts.url.as_deref(),
        paths: &opts.paths,
        head: &head,
        stats: serde_json::to_value(&stats)?,
    };
    store::finish(&client, &schema, &built).await?;
    stats.finish_secs = started.elapsed().as_secs();
    client
        .execute(
            &format!("UPDATE {schema}.code_history SET stats = $1"),
            &[&serde_json::to_value(&stats)?],
        )
        .await?;
    if opts.install {
        store::swap(&mut client, &schema).await?;
        tracing::info!("installed the history in schema pghacker");
    }
    Ok(stats)
}

/// Which commits to check against git.
#[derive(Clone, Debug, Default)]
pub struct VerifyOptions {
    pub git_dir: PathBuf,
    pub schema: String,
    /// Revisions, as git names them.
    pub revs: Vec<String>,
    /// This many commits more, picked at random.
    pub sample: u32,
    /// Every branch head too.
    pub heads: bool,
    /// Also compare index and sequential scans.
    pub scans: bool,
}

/// Rebuilds every file at the chosen commits and checks it against git.
pub async fn verify_build(database_url: &str, opts: &VerifyOptions) -> Result<verify::Verified> {
    let client = pghacker_core::db::connect(database_url, "pghacker-history").await?;
    let s = store::readable_schema(&opts.schema)?;
    let paths = built_paths(&client, s).await?;
    let repo = open(&opts.git_dir)?;
    let mut commits = Vec::new();
    for rev in &opts.revs {
        let id = repo
            .rev_parse_single(rev.as_str())
            .with_context(|| format!("no revision {rev}"))?
            .object()?
            .peel_to_commit()?
            .id;
        commits.push(id);
    }
    let mut shas: Vec<String> = Vec::new();
    if opts.heads {
        for row in client
            .query(
                &format!(
                    "SELECT c.sha FROM {s}.code_refs r JOIN {s}.code_commits c ON c.addr = r.addr \
                     WHERE r.kind = 'branch' ORDER BY r.name"
                ),
                &[],
            )
            .await?
        {
            shas.push(row.get(0));
        }
    }
    if opts.sample > 0 {
        for row in client
            .query(
                &format!("SELECT sha FROM {s}.code_commits ORDER BY random() LIMIT $1"),
                &[&i64::from(opts.sample)],
            )
            .await?
        {
            shas.push(row.get(0));
        }
    }
    for sha in shas {
        commits.push(gix::ObjectId::from_hex(sha.as_bytes())?);
    }
    let mut seen = std::collections::HashSet::new();
    commits.retain(|id| seen.insert(*id));
    verify::verify(&client, s, &repo, &paths, &commits, opts.scans).await
}
