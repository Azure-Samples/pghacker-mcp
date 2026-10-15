//! Writes a build into a schema of its own, then swaps it into `pghacker`
//! or keeps it there (the engine benchmark reads it in place).
//!
//! The tables' columns, constraints and indexes come from `pghacker`'s, as
//! migration 0011 defines them: staging tables are created `LIKE` them
//! without indexes, loaded, then given the same constraints and indexes.
//! A row is alive at address X when `lo <= X AND X < hi AND (alive IS NULL
//! OR alive @> X)`.

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use tokio::sync::mpsc;
use tokio_postgres::Client;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::Type;

use super::addr::Alive;
use super::graph::{Commit, Graph};
use super::walk::{Arena, CommitChanges};

/// An instance link as stored: NULL for none.
fn replaces(id: u32) -> Option<i32> {
    (id != super::walk::NONE).then_some(id as i32)
}

/// The gap between consecutive weave keys, so syncs can insert lines.
pub const WEAVE_STEP: i64 = 1 << 32;

/// A build's schema name, checked: lower-case letters, digits and
/// underscores, and never one of the application's or the system's.
pub fn schema_name(name: &str) -> Result<&str> {
    let s = readable_schema(name)?;
    let reserved = ["public", "pghacker", "ag_catalog", "information_schema"].contains(&s)
        || s.starts_with("pg_");
    if reserved {
        bail!("{name:?} can't be a history build's schema");
    }
    Ok(s)
}

/// A schema to read the history from: a build's, or `pghacker` once a build
/// is installed.
pub fn readable_schema(name: &str) -> Result<&str> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        bail!("{name:?} isn't a schema name");
    }
    Ok(name)
}

/// Whether writes to the history in `schema` are recorded for the graph:
/// they are in `pghacker`, whose tables the graph projects. The history's
/// tables have no triggers, so its writers record in `graph_changes`, in the
/// writing transaction, what they change, with migration 0014's functions.
pub(crate) fn captured(schema: &str) -> bool {
    schema == "pghacker"
}

/// The history's tables, from migration 0011.
pub const TABLES: [&str; 9] = [
    "code_history",
    "code_lanes",
    "code_commits",
    "code_commit_files",
    "code_commit_threads",
    "code_refs",
    "code_lineages",
    "code_file_versions",
    "code_line_instances",
];

/// Creates the build's schema afresh, with tables like `pghacker`'s but
/// without their indexes, dropping one an earlier build made. A schema that
/// a build didn't make is refused.
pub async fn create(client: &Client, schema: &str) -> Result<()> {
    let s = schema_name(schema)?;
    let canonical: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_tables WHERE schemaname = 'pghacker' AND tablename = ANY($1)",
            &[&TABLES.to_vec()],
        )
        .await?
        .get(0);
    if canonical != TABLES.len() as i64 {
        bail!("the history's tables are missing from schema pghacker: migrate first");
    }
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
                "SELECT EXISTS (SELECT FROM pg_tables WHERE schemaname = $1 AND tablename = 'code_history')",
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
    let mut sql = format!("CREATE SCHEMA {s};");
    for t in TABLES {
        sql.push_str(&format!(
            "CREATE TABLE {s}.{t} (LIKE pghacker.{t} INCLUDING DEFAULTS INCLUDING CONSTRAINTS);"
        ));
    }
    sql.push_str(&format!(
        "CREATE UNLOGGED TABLE {s}.stage_text (id int4 NOT NULL, text text NOT NULL, bytes int4 NOT NULL);
         CREATE UNLOGGED TABLE {s}.stage_versions (
             id int4, lineage int4, path text, blob_sha text, lo int8, hi int8,
             alive text, size int4, line_count int4, is_binary bool,
             final_newline bool, bom bool, subsystem text
         );
         CREATE UNLOGGED TABLE {s}.stage_meta (
             id int4, lineage int4, weave int8, lo int8, hi int8, alive text, replaces int4
         );"
    ));
    client
        .batch_execute(&sql)
        .await
        .with_context(|| format!("cannot create schema {s}"))?;
    Ok(())
}

/// Copies each new line's text and length as the walk produces them.
pub async fn copy_texts(
    client: &Client,
    schema: &str,
    mut rx: mpsc::Receiver<Vec<(u32, String, u32)>>,
) -> Result<u64> {
    let s = schema_name(schema)?;
    let sink = client
        .copy_in(&format!(
            "COPY {s}.stage_text (id, text, bytes) FROM STDIN BINARY"
        ))
        .await?;
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::TEXT, Type::INT4]);
    let mut writer = std::pin::pin!(writer);
    while let Some(batch) = rx.recv().await {
        for (id, text, bytes) in &batch {
            writer
                .as_mut()
                .write(&[&(*id as i32), text, &(*bytes as i32)])
                .await?;
        }
    }
    Ok(writer.finish().await?)
}

/// code_commits' columns as builds and syncs write them; `backpatch` is
/// set afterwards by [`super::backpatch::regroup`].
pub(crate) const COMMIT_COLUMNS: &str = "addr, sha, lane, committed_at, authored_at, author_name, \
     committer_name, subject, message, trailers, patch_id";
const COMMIT_TYPES: [Type; 11] = [
    Type::INT8,
    Type::TEXT,
    Type::INT2,
    Type::TIMESTAMPTZ,
    Type::TIMESTAMPTZ,
    Type::TEXT,
    Type::TEXT,
    Type::TEXT,
    Type::TEXT,
    Type::JSONB,
    Type::TEXT,
];

/// A code_commits row.
pub(crate) struct CommitRow {
    addr: i64,
    sha: String,
    lane: i16,
    committed_at: DateTime<chrono::Utc>,
    authored_at: DateTime<chrono::Utc>,
    author: String,
    committer: String,
    subject: String,
    message: String,
    trailers: serde_json::Value,
    patch_id: Option<String>,
}

impl CommitRow {
    pub(crate) fn new(c: &Commit, changes: Option<&CommitChanges>) -> Self {
        let time = |t: i64| DateTime::from_timestamp(t, 0).unwrap_or_default();
        Self {
            addr: c.addr,
            sha: c.id.to_string(),
            lane: c.lane as i16,
            committed_at: time(c.committed_at),
            authored_at: time(c.authored_at),
            author: c.author.clone(),
            committer: c.committer.clone(),
            subject: c.subject.clone(),
            message: c.message.clone(),
            trailers: serde_json::to_value(super::message::trailers(&c.message))
                .unwrap_or_default(),
            patch_id: changes.and_then(|c| c.patch_id.clone()),
        }
    }

    pub(crate) fn params(&self) -> [&(dyn tokio_postgres::types::ToSql + Sync); 11] {
        [
            &self.addr,
            &self.sha,
            &self.lane,
            &self.committed_at,
            &self.authored_at,
            &self.author,
            &self.committer,
            &self.subject,
            &self.message,
            &self.trailers,
            &self.patch_id,
        ]
    }
}

pub(crate) const FILE_COLUMNS: &str =
    "addr, path, old_path, status, added, removed, is_binary, version, old_version";
const FILE_TYPES: [Type; 9] = [
    Type::INT8,
    Type::TEXT,
    Type::TEXT,
    Type::TEXT,
    Type::INT4,
    Type::INT4,
    Type::BOOL,
    Type::INT4,
    Type::INT4,
];

/// A code_commit_files row.
pub(crate) struct FileRow<'a> {
    addr: i64,
    path: &'a str,
    old_path: Option<&'a str>,
    status: String,
    added: i32,
    removed: i32,
    binary: bool,
    version: Option<i32>,
    old_version: Option<i32>,
}

impl FileRow<'_> {
    pub(crate) fn params(&self) -> [&(dyn tokio_postgres::types::ToSql + Sync); 9] {
        [
            &self.addr,
            &self.path,
            &self.old_path,
            &self.status,
            &self.added,
            &self.removed,
            &self.binary,
            &self.version,
            &self.old_version,
        ]
    }
}

/// A commit's code_commit_files rows: one per path, the last change to it.
pub(crate) fn file_rows(addr: i64, changes: &CommitChanges) -> Vec<FileRow<'_>> {
    let mut by_path: std::collections::BTreeMap<&str, FileRow<'_>> = Default::default();
    for f in &changes.files {
        by_path.insert(
            &f.path,
            FileRow {
                addr,
                path: &f.path,
                old_path: f.old_path.as_deref(),
                status: f.status.to_string(),
                added: f.added as i32,
                removed: f.removed as i32,
                binary: f.binary,
                version: f.version.map(|v| v as i32),
                old_version: f.old_version.map(|v| v as i32),
            },
        );
    }
    by_path.into_values().collect()
}

/// What the store wrote.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Stored {
    pub commits: u64,
    pub commit_files: u64,
    /// Commits in back-patch groups.
    pub backpatched: u64,
    pub links: super::links::Linked,
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

    let changes: std::collections::HashMap<i64, &CommitChanges> =
        arena.changes.iter().map(|(a, c)| (*a, c)).collect();
    {
        let sink = client
            .copy_in(&format!(
                "COPY {s}.code_commits ({COMMIT_COLUMNS}) FROM STDIN BINARY"
            ))
            .await?;
        let writer = BinaryCopyInWriter::new(sink, &COMMIT_TYPES);
        let mut writer = std::pin::pin!(writer);
        for c in &graph.commits {
            let row = CommitRow::new(c, changes.get(&c.addr).copied());
            writer.as_mut().write(&row.params()).await?;
        }
        stored.commits = writer.finish().await?;
    }
    {
        let sink = client
            .copy_in(&format!(
                "COPY {s}.code_commit_files ({FILE_COLUMNS}) FROM STDIN BINARY"
            ))
            .await?;
        let writer = BinaryCopyInWriter::new(sink, &FILE_TYPES);
        let mut writer = std::pin::pin!(writer);
        for (addr, c) in &arena.changes {
            for row in file_rows(*addr, c) {
                writer.as_mut().write(&row.params()).await?;
            }
        }
        stored.commit_files = writer.finish().await?;
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
                "COPY {s}.stage_meta (id, lineage, weave, lo, hi, alive, replaces) \
                 FROM STDIN BINARY"
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
                Type::INT4,
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
                    .write(&[
                        &(id as i32),
                        &(lineage as i32),
                        &weave,
                        &lo,
                        &hi,
                        &holes,
                        &replaces(arena.replaces[id as usize]),
                    ])
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

/// What the build was: written to `code_history`.
pub struct Built<'a> {
    pub master: &'a str,
    pub url: Option<&'a str>,
    pub paths: &'a [String],
    pub head: &'a str,
    pub stats: serde_json::Value,
}

/// Builds the final tables from the staged rows, then `pghacker`'s
/// constraints and indexes on them.
pub async fn finish(client: &Client, schema: &str, built: &Built<'_>) -> Result<()> {
    let s = schema_name(schema)?;
    let steps = [
        (
            "line instances",
            format!(
                "SET maintenance_work_mem = '1GB';
                 SET work_mem = '256MB';
                 INSERT INTO {s}.code_line_instances
                     (weave, lo, hi, id, lineage, bytes, alive, replaces, line)
                 SELECT m.weave, m.lo, m.hi, m.id, m.lineage, t.bytes,
                        m.alive::int8multirange, m.replaces, t.text
                 FROM {s}.stage_meta m JOIN {s}.stage_text t USING (id)
                 ORDER BY m.lineage, m.hi, m.weave;
                 DROP TABLE {s}.stage_meta, {s}.stage_text;"
            ),
        ),
        (
            "file versions",
            format!(
                "INSERT INTO {s}.code_file_versions (id, lineage, path, blob_sha, lo, hi, alive, size,
                     line_count, is_binary, final_newline, bom, subsystem)
                 SELECT id, lineage, path, blob_sha, lo, hi, alive::int8multirange, size,
                        line_count, is_binary, final_newline, bom, subsystem
                 FROM {s}.stage_versions ORDER BY lineage, lo;
                 DROP TABLE {s}.stage_versions;"
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
    client
        .execute(
            &format!(
                "INSERT INTO {s}.code_history (master, url, paths, head, stats) \
                 VALUES ($1, $2, $3, $4, $5)"
            ),
            &[
                &built.master,
                &built.url,
                &built.paths,
                &built.head,
                &built.stats,
            ],
        )
        .await?;

    // pghacker's constraints and indexes, written with qualified names.
    client.batch_execute("SET search_path = ''").await?;
    let constraints = client
        .query(
            "SELECT c.relname, con.conname, pg_get_constraintdef(con.oid) \
             FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'pghacker' AND c.relname = ANY($1) AND con.contype IN ('p', 'u') \
             ORDER BY 1, 2",
            &[&TABLES.to_vec()],
        )
        .await?;
    let indexes = client
        .query(
            "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'pghacker' AND c.relname = ANY($1) \
               AND NOT EXISTS (SELECT FROM pg_constraint WHERE conindid = i.indexrelid) \
             ORDER BY 1",
            &[&TABLES.to_vec()],
        )
        .await?;
    let restore = client.batch_execute("RESET search_path").await;
    for row in constraints {
        let (table, name, def): (String, String, String) = (row.get(0), row.get(1), row.get(2));
        let started = std::time::Instant::now();
        client
            .batch_execute(&format!(
                "ALTER TABLE {s}.{table} ADD CONSTRAINT {name} {def}"
            ))
            .await
            .with_context(|| format!("cannot add {name}"))?;
        tracing::info!(constraint = %name, secs = started.elapsed().as_secs(), "built");
    }
    for row in indexes {
        let def: String = row.get(0);
        let Some((head, tail)) = def.split_once(" ON pghacker.") else {
            bail!("unexpected index definition: {def}");
        };
        let started = std::time::Instant::now();
        client
            .batch_execute(&format!(
                "SET maintenance_work_mem = '1GB'; {head} ON {s}.{tail}"
            ))
            .await
            .with_context(|| format!("cannot build {head}"))?;
        tracing::info!(index = %head, secs = started.elapsed().as_secs(), "built");
    }
    restore?;
    let tables: Vec<String> = TABLES.iter().map(|t| format!("{s}.{t}")).collect();
    client
        .batch_execute(&format!("VACUUM (ANALYZE) {}", tables.join(", ")))
        .await
        .context("cannot analyze the history")?;
    client
        .batch_execute(&format!(
            "DO $$ BEGIN
                 IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'pgh_reader') THEN
                     GRANT USAGE ON SCHEMA {s} TO pgh_reader;
                     GRANT SELECT ON ALL TABLES IN SCHEMA {s} TO pgh_reader;
                 END IF;
             END $$;"
        ))
        .await?;
    Ok(())
}

/// Replaces `pghacker`'s history tables with the build's, in one
/// transaction, and drops the build's schema. The tables go to pgh_indexer,
/// which syncs them. The graph projects them: a full sync is requested.
pub async fn swap(client: &mut Client, schema: &str) -> Result<()> {
    let s = schema_name(schema)?;
    let canonical: Vec<String> = TABLES.iter().map(|t| format!("pghacker.{t}")).collect();
    let tx = client.transaction().await?;
    // The graph sync reads these tables, a full sync for minutes. Waiting for
    // it on its lock, rather than queueing the exclusive lock behind it,
    // leaves the code tools reading meanwhile; and no sync starts until the
    // swap is done.
    let mut sql = format!(
        "SELECT pg_advisory_xact_lock(hashtext('pghacker.graph_sync'));
         LOCK TABLE {} IN ACCESS EXCLUSIVE MODE; DROP TABLE {};",
        canonical.join(", "),
        canonical.join(", ")
    );
    for t in TABLES {
        sql.push_str(&format!("ALTER TABLE {s}.{t} SET SCHEMA pghacker;"));
    }
    sql.push_str(&format!(
        "DROP SCHEMA {s};
         DO $$ BEGIN
             IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'pgh_indexer') THEN
                 {}
             END IF;
             IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'pgh_reader') THEN
                 GRANT SELECT ON {} TO pgh_reader;
             END IF;
         END $$;
         SELECT pghacker.graph_request_full('a commit history was installed');",
        TABLES
            .iter()
            .map(|t| format!("ALTER TABLE pghacker.{t} OWNER TO pgh_indexer;"))
            .collect::<String>(),
        canonical.join(", ")
    ));
    tx.batch_execute(&sql)
        .await
        .context("cannot swap the history into pghacker")?;
    tx.commit().await?;
    Ok(())
}
