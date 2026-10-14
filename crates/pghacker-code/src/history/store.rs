//! The history's tables, in a schema of their own while the prototype is
//! measured against the snapshots.
//!
//! - `code_commits`: every commit with its address and lane.
//! - `code_lanes`: master and each branch, with its address block.
//! - `code_refs`: branches and tags.
//! - `code_lineages`: a file's identity across whole-file renames.
//! - `code_file_versions`: a file's content at a path, alive in [lo, hi)
//!   except where `alive` (set only when there are holes) says otherwise.
//! - `code_line_instances`: one row per line instance (`line` is its text),
//!   ordered within its lineage by `weave`, alive the same way.
//!
//! A row is alive at address X when `lo <= X AND X < hi AND (alive IS NULL
//! OR alive @> X)`. Multiranges are only ever checked as filters: GiST scans
//! of multiranges can miss rows on PostgreSQL 14 and later.

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use tokio::sync::mpsc;
use tokio_postgres::Client;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::Type;

use super::addr::Alive;
use super::graph::Graph;
use super::walk::Arena;

/// The gap between consecutive weave keys, so syncs can insert lines.
pub const WEAVE_STEP: i64 = 1 << 32;

/// The schema name, checked: lower-case letters, digits and underscores,
/// and never one of the application's or the system's.
pub fn schema_name(name: &str) -> Result<&str> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let reserved = ["public", "pghacker", "ag_catalog", "information_schema"].contains(&name)
        || name.starts_with("pg_");
    if !valid || reserved {
        bail!("{name:?} can't be the history's schema");
    }
    Ok(name)
}

/// Creates the schema afresh, dropping one an earlier build made. A schema
/// that a build didn't make is refused.
pub async fn create(client: &Client, schema: &str) -> Result<()> {
    let s = schema_name(schema)?;
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT FROM pg_namespace WHERE nspname = $1)",
            &[&s],
        )
        .await?
        .get(0);
    if exists {
        let ours: bool = client
            .query_one(
                "SELECT EXISTS (SELECT FROM pg_tables WHERE schemaname = $1 AND tablename = 'history_build')",
                &[&s],
            )
            .await?
            .get(0);
        if !ours {
            bail!("schema {s} exists and isn't a history build's; refusing to drop it");
        }
        client
            .batch_execute(&format!("DROP SCHEMA {s} CASCADE"))
            .await
            .with_context(|| format!("cannot drop schema {s}"))?;
    }
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {s};
             CREATE TABLE {s}.history_build (
                 started_at timestamptz NOT NULL DEFAULT now(),
                 finished_at timestamptz,
                 master text NOT NULL,
                 paths text[] NOT NULL,
                 head text,
                 stats jsonb
             );
             CREATE UNLOGGED TABLE {s}.stage_text (id int4 NOT NULL, text text NOT NULL);"
        ))
        .await
        .with_context(|| format!("cannot create schema {s}"))?;
    Ok(())
}

/// Copies each new line's text as the walk produces it.
pub async fn copy_texts(
    client: &Client,
    schema: &str,
    mut rx: mpsc::Receiver<Vec<(u32, String)>>,
) -> Result<u64> {
    let s = schema_name(schema)?;
    let sink = client
        .copy_in(&format!("COPY {s}.stage_text (id, text) FROM STDIN BINARY"))
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::TEXT]);
    let mut writer = std::pin::pin!(writer);
    while let Some(batch) = rx.recv().await {
        for (id, text) in &batch {
            writer.as_mut().write(&[&(*id as i32), text]).await?;
        }
    }
    Ok(writer.finish().await?)
}

/// What the store wrote.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stored {
    pub commits: u64,
    pub lanes: u64,
    pub refs: u64,
    pub lineages: u64,
    pub versions: u64,
    pub instances: u64,
    /// Instances alive in more than one range: deleted on some branches.
    pub instances_with_holes: u64,
}

/// Writes everything but the lines' text, then builds the final tables and
/// their indexes.
pub async fn write(
    client: &mut Client,
    schema: &str,
    graph: &Graph,
    arena: &Arena,
) -> Result<Stored> {
    let s = schema_name(schema)?;
    let mut stored = Stored::default();
    client
        .batch_execute(&format!(
            "SET maintenance_work_mem = '1GB';
             SET work_mem = '256MB';
             CREATE TABLE {s}.code_lanes (
                 id int2 PRIMARY KEY,
                 name text NOT NULL UNIQUE,
                 fork_addr int8,
                 lo int8 NOT NULL,
                 hi int8 NOT NULL,
                 head_addr int8 NOT NULL
             );
             CREATE TABLE {s}.code_commits (
                 addr int8 PRIMARY KEY,
                 sha text NOT NULL UNIQUE,
                 lane int2 NOT NULL,
                 committed_at timestamptz NOT NULL,
                 author_name text NOT NULL,
                 subject text NOT NULL
             );
             CREATE TABLE {s}.code_refs (
                 kind text NOT NULL,
                 name text NOT NULL,
                 addr int8 NOT NULL,
                 PRIMARY KEY (kind, name)
             );
             CREATE TABLE {s}.code_lineages (
                 id int4 PRIMARY KEY,
                 first_path text NOT NULL,
                 born int8 NOT NULL
             );
             CREATE UNLOGGED TABLE {s}.stage_versions (
                 id int4, lineage int4, path text, blob_sha text, lo int8, hi int8,
                 alive text, size int4, line_count int4, is_binary bool,
                 final_newline bool, bom bool, subsystem text
             );
             CREATE UNLOGGED TABLE {s}.stage_meta (
                 id int4, lineage int4, weave int8, lo int8, hi int8, alive text
             );"
        ))
        .await?;

    for lane in &graph.lanes {
        let fork = lane.fork.map(super::addr::master);
        client
            .execute(
                &format!(
                    "INSERT INTO {s}.code_lanes (id, name, fork_addr, lo, hi, head_addr) \
                     VALUES ($1, $2, $3, $4, $5, $6)"
                ),
                &[
                    &(lane.id as i16),
                    &lane.name,
                    &fork,
                    &lane.block.0,
                    &lane.block.1,
                    &lane.head_addr(graph),
                ],
            )
            .await?;
        stored.lanes += 1;
    }
    for r in &graph.refs {
        let addr = graph.commits[graph.index[&r.id]].addr;
        client
            .execute(
                &format!("INSERT INTO {s}.code_refs (kind, name, addr) VALUES ($1, $2, $3)"),
                &[&r.kind, &r.name, &addr],
            )
            .await?;
        stored.refs += 1;
    }

    {
        let sink = client
            .copy_in(&format!(
                "COPY {s}.code_commits (addr, sha, lane, committed_at, author_name, subject) \
                 FROM STDIN BINARY"
            ))
            .await?;
        let writer = BinaryCopyInWriter::new(
            sink,
            &[
                Type::INT8,
                Type::TEXT,
                Type::INT2,
                Type::TIMESTAMPTZ,
                Type::TEXT,
                Type::TEXT,
            ],
        );
        let mut writer = std::pin::pin!(writer);
        for c in &graph.commits {
            let at = DateTime::from_timestamp(c.committed_at, 0).unwrap_or_default();
            writer
                .as_mut()
                .write(&[
                    &c.addr,
                    &c.id.to_string(),
                    &(c.lane as i16),
                    &at,
                    &c.author,
                    &c.subject,
                ])
                .await?;
        }
        stored.commits = writer.finish().await?;
    }

    {
        let sink = client
            .copy_in(&format!(
                "COPY {s}.code_lineages (id, first_path, born) FROM STDIN BINARY"
            ))
            .await?;
        let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::TEXT, Type::INT8]);
        let mut writer = std::pin::pin!(writer);
        for (id, l) in arena.lineages.iter().enumerate() {
            writer
                .as_mut()
                .write(&[&(id as i32), &l.path, &l.born])
                .await?;
        }
        stored.lineages = writer.finish().await?;
    }

    {
        let sink = client
            .copy_in(&format!(
                "COPY {s}.stage_versions (id, lineage, path, blob_sha, lo, hi, alive, size, \
                 line_count, is_binary, final_newline, bom, subsystem) FROM STDIN BINARY"
            ))
            .await?;
        let writer = BinaryCopyInWriter::new(
            sink,
            &[
                Type::INT4,
                Type::INT4,
                Type::TEXT,
                Type::TEXT,
                Type::INT8,
                Type::INT8,
                Type::TEXT,
                Type::INT4,
                Type::INT4,
                Type::BOOL,
                Type::BOOL,
                Type::BOOL,
                Type::TEXT,
            ],
        );
        let mut writer = std::pin::pin!(writer);
        for (id, v) in arena.versions.iter().enumerate() {
            let alive = Alive::of(v.born, v.deaths.iter().copied());
            let (lo, hi) = alive.hull();
            let holes = alive.has_holes().then(|| alive.to_sql());
            writer
                .as_mut()
                .write(&[
                    &(id as i32),
                    &(v.lineage as i32),
                    &v.path,
                    &v.blob.to_string(),
                    &lo,
                    &hi,
                    &holes,
                    &v.size,
                    &v.line_count,
                    &v.binary,
                    &v.final_newline,
                    &v.bom,
                    &crate::subsystem_of(&v.path),
                ])
                .await?;
        }
        stored.versions = writer.finish().await?;
    }

    {
        let sink = client
            .copy_in(&format!(
                "COPY {s}.stage_meta (id, lineage, weave, lo, hi, alive) FROM STDIN BINARY"
            ))
            .await?;
        let writer = BinaryCopyInWriter::new(
            sink,
            &[
                Type::INT4,
                Type::INT4,
                Type::INT8,
                Type::INT8,
                Type::INT8,
                Type::TEXT,
            ],
        );
        let mut writer = std::pin::pin!(writer);
        for lineage in 0..arena.heads.len() as u32 {
            for (rank, id) in arena.weave(lineage).into_iter().enumerate() {
                let alive = Alive::of(arena.born[id as usize], arena.deaths(id));
                let (lo, hi) = alive.hull();
                let holes = alive.has_holes().then(|| alive.to_sql());
                if holes.is_some() {
                    stored.instances_with_holes += 1;
                }
                let weave = (rank as i64 + 1) * WEAVE_STEP;
                writer
                    .as_mut()
                    .write(&[&(id as i32), &(lineage as i32), &weave, &lo, &hi, &holes])
                    .await?;
            }
        }
        stored.instances = writer.finish().await?;
    }
    if stored.instances != arena.instances() as u64 {
        bail!(
            "wrote {} instances of {}: a weave is broken",
            stored.instances,
            arena.instances()
        );
    }
    Ok(stored)
}

/// Builds the final tables from the staged rows, then the indexes.
pub async fn finish(client: &Client, schema: &str) -> Result<()> {
    let s = schema_name(schema)?;
    let steps = [
        (
            "line instances",
            format!(
                "CREATE TABLE {s}.code_line_instances (
                     weave int8 NOT NULL,
                     lo int8 NOT NULL,
                     hi int8 NOT NULL,
                     id int4 NOT NULL,
                     lineage int4 NOT NULL,
                     bytes int4 NOT NULL,
                     alive int8multirange,
                     line text NOT NULL
                 );
                 INSERT INTO {s}.code_line_instances
                 SELECT m.weave, m.lo, m.hi, m.id, m.lineage, octet_length(t.text),
                        m.alive::int8multirange, t.text
                 FROM {s}.stage_meta m JOIN {s}.stage_text t USING (id)
                 ORDER BY m.lineage, m.hi, m.weave;
                 DROP TABLE {s}.stage_meta, {s}.stage_text;"
            ),
        ),
        (
            "file versions",
            format!(
                "CREATE TABLE {s}.code_file_versions (
                     id int4 PRIMARY KEY,
                     lineage int4 NOT NULL,
                     path text NOT NULL,
                     blob_sha text NOT NULL,
                     lo int8 NOT NULL,
                     hi int8 NOT NULL,
                     alive int8multirange,
                     size int4 NOT NULL,
                     line_count int4 NOT NULL,
                     is_binary bool NOT NULL,
                     final_newline bool NOT NULL,
                     bom bool NOT NULL,
                     subsystem text
                 );
                 INSERT INTO {s}.code_file_versions
                 SELECT id, lineage, path, blob_sha, lo, hi, alive::int8multirange, size,
                        line_count, is_binary, final_newline, bom, subsystem
                 FROM {s}.stage_versions ORDER BY lineage, lo;
                 DROP TABLE {s}.stage_versions;
                 CREATE INDEX ON {s}.code_file_versions (hi, lo);
                 CREATE INDEX ON {s}.code_file_versions (lineage, lo);
                 CREATE INDEX ON {s}.code_file_versions (path);"
            ),
        ),
        (
            "instance keys",
            format!(
                "ALTER TABLE {s}.code_line_instances ADD PRIMARY KEY (id);
                 CREATE INDEX code_line_instances_scope ON {s}.code_line_instances (lineage, hi);
                 CREATE INDEX code_line_instances_weave ON {s}.code_line_instances (lineage, weave);"
            ),
        ),
        (
            "trigram index",
            format!(
                "CREATE INDEX code_line_instances_text ON {s}.code_line_instances \
                 USING gin (line gin_trgm_ops);"
            ),
        ),
        (
            "statistics",
            format!(
                "VACUUM (ANALYZE) {s}.code_line_instances, {s}.code_file_versions, \
                     {s}.code_commits, {s}.code_lineages, {s}.code_lanes, {s}.code_refs"
            ),
        ),
        (
            "grants",
            format!(
                "DO $$ BEGIN
                     IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'pgh_reader') THEN
                         GRANT USAGE ON SCHEMA {s} TO pgh_reader;
                         GRANT SELECT ON ALL TABLES IN SCHEMA {s} TO pgh_reader;
                     END IF;
                 END $$;"
            ),
        ),
    ];
    for (name, sql) in steps {
        let started = std::time::Instant::now();
        client
            .batch_execute(&sql)
            .await
            .with_context(|| format!("cannot build the {name}"))?;
        tracing::info!(step = name, secs = started.elapsed().as_secs(), "built");
    }
    Ok(())
}
