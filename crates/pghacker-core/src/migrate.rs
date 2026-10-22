// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The bootstrap, which the administrator runs, and the migrations, which
//! pgh_indexer runs: both on every start, both idempotent.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

static MIGRATIONS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../migrations");
const ROLES: &str = include_str!("../sql/roles.sql");
const DATABASE: &str = include_str!("../sql/database.sql");

/// The license header that starts each migration, with the blank line after
/// it (scripts/lib/license-headers.mjs). A migration's checksum leaves it
/// out, so the header changed no checksum that a database had recorded.
const LICENSE_HEADER: &str =
    "-- Copyright (c) Microsoft Corporation.\n-- Licensed under the MIT license.\n\n";

/// A migration's checksum: the SHA-256 of its SQL with LF line ends, without
/// the license header. A checkout with CRLF line ends has the same.
fn checksum(sql: &str) -> String {
    let sql = sql.replace("\r\n", "\n");
    let body = sql.strip_prefix(LICENSE_HEADER).unwrap_or(&sql);
    hex::encode(Sha256::digest(body.as_bytes()))
}

/// The checksum that older binaries recorded from a checkout with CRLF line
/// ends, such as Windows's with core.autocrlf: of the body with CRLF ends.
fn crlf_checksum(sql: &str) -> String {
    let sql = sql.replace("\r\n", "\n");
    let body = sql.strip_prefix(LICENSE_HEADER).unwrap_or(&sql);
    hex::encode(Sha256::digest(body.replace('\n', "\r\n").as_bytes()))
}

/// The roles' passwords: pgh_indexer's and pgh_reader's, and pgh_swarm's
/// when PilotSwarm gets a role of its own rather than the administrator's.
#[derive(Clone)]
pub struct Passwords {
    pub indexer: String,
    pub reader: String,
    pub swarm: Option<String>,
}

impl std::fmt::Debug for Passwords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passwords { .. }")
    }
}

/// Creates or updates the roles, then the database's extensions, schema,
/// graph and grants. `admin` must be connected to the target database.
pub async fn bootstrap(admin: &mut Client, passwords: &Passwords) -> Result<()> {
    bootstrap_roles(admin, passwords).await?;
    bootstrap_database(admin).await
}

/// The cluster's roles. Concurrent bootstraps can collide on the roles'
/// catalog rows, so this retries.
pub async fn bootstrap_roles(admin: &mut Client, passwords: &Passwords) -> Result<()> {
    let swarm = passwords.swarm.clone().unwrap_or_default();
    retry_races(admin, async |admin: &mut Client| {
        let tx = admin.transaction().await?;
        tx.execute(
            "SELECT set_config('pghacker.indexer_password', $1, true), \
                    set_config('pghacker.reader_password', $2, true), \
                    set_config('pghacker.swarm_password', $3, true)",
            &[&passwords.indexer, &passwords.reader, &swarm],
        )
        .await?;
        tx.batch_execute(ROLES).await?;
        tx.commit().await?;
        Ok(())
    })
    .await
    .context("cannot create the roles")
}

/// Runs `step` in its own transactions until it doesn't collide with a
/// concurrent bootstrap on the cluster's shared catalogs, up to 5 times.
async fn retry_races(
    admin: &mut Client,
    step: impl AsyncFn(&mut Client) -> Result<()>,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match step(admin).await {
            Err(error)
                if attempt < 5
                    && error
                        .downcast_ref::<tokio_postgres::Error>()
                        .is_some_and(is_race) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100 * attempt)).await;
            }
            result => return result,
        }
    }
}

/// Creates the database of `admin_url` unless it exists, from the server's
/// `postgres` database. Returns whether it created it.
pub async fn create_database_if_missing(admin_url: &str) -> Result<bool> {
    let config: tokio_postgres::Config =
        admin_url.parse().context("the database URL is invalid")?;
    let name = config
        .get_dbname()
        .or(config.get_user())
        .context("the administrator's URL names no database")?
        .to_owned();
    match crate::db::connect(admin_url, "pghacker-bootstrap").await {
        Ok(_) => return Ok(false),
        Err(error) if !is_missing_database(&error) => return Err(error),
        Err(_) => {}
    }
    let maintenance = crate::db::connect(
        &crate::config::with_database(admin_url, "postgres")?,
        "pghacker-bootstrap",
    )
    .await?;
    match maintenance
        .batch_execute(&format!("CREATE DATABASE {}", quote_ident(&name)))
        .await
    {
        Ok(()) => {
            tracing::info!(database = %name, "created the database");
            Ok(true)
        }
        // Another bootstrap created it first.
        Err(error) if error.code() == Some(&SqlState::DUPLICATE_DATABASE) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot create the database {name}")),
    }
}

fn is_missing_database(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            == Some(&SqlState::INVALID_CATALOG_NAME)
    })
}

/// `name` as an SQL identifier.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The database's extensions, schema, graph, labels and grants.
pub async fn bootstrap_database(admin: &mut Client) -> Result<()> {
    bootstrap_database_with_labels(admin, crate::VERTEX_LABELS, crate::EDGE_LABELS).await
}

async fn bootstrap_database_with_labels(
    admin: &mut Client,
    vertices: &[&str],
    edges: &[&str],
) -> Result<()> {
    let vertices_json = serde_json::to_string(vertices)?;
    let edges_json = serde_json::to_string(edges)?;
    retry_races(admin, async |admin: &mut Client| {
        let tx = admin.transaction().await?;
        tx.execute(
            "SELECT set_config('pghacker.vertex_labels', $1, true),
                    set_config('pghacker.edge_labels', $2, true)",
            &[&vertices_json, &edges_json],
        )
        .await?;
        tx.batch_execute(DATABASE).await?;
        let labels: serde_json::Value = tx
            .query_one(
                "SELECT current_setting('pghacker.new_edge_labels')::jsonb",
                &[],
            )
            .await?
            .get(0);
        let labels: Vec<String> = serde_json::from_value(labels)?;
        crate::graph_indexes::index_new_labels(&tx, &labels).await?;
        crate::graph_indexes::check_age(&tx, vertices, edges).await?;
        tx.commit().await?;
        Ok(())
    })
    .await
    .context("cannot bootstrap the database")
}

/// Whether `error` is a collision with a concurrent bootstrap.
fn is_race(error: &tokio_postgres::Error) -> bool {
    if error.code() == Some(&SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE) {
        return false;
    }
    let text = format!("{error:?}");
    text.contains("concurrently")
        || text.contains("duplicate key")
        || text.contains("already exists")
}

/// Whether both roles exist.
pub async fn roles_exist(admin: &Client) -> Result<bool> {
    let row = admin
        .query_one(
            "SELECT count(*) = 2 FROM pg_roles WHERE rolname IN ('pgh_indexer', 'pgh_reader')",
            &[],
        )
        .await?;
    Ok(row.get(0))
}

/// A migration file.
pub struct Migration {
    pub version: String,
    pub sql: &'static str,
    pub sha256: String,
}

impl Migration {
    /// Whether `recorded`, the checksum a database recorded for this
    /// migration, is this one's: its checksum, or the one older binaries
    /// recorded from a checkout with CRLF line ends.
    fn recorded_as(&self, recorded: &str) -> bool {
        recorded == self.sha256 || recorded == crlf_checksum(self.sql)
    }
}

/// The embedded migrations, in order.
pub fn migrations() -> Vec<Migration> {
    let mut list: Vec<Migration> = MIGRATIONS
        .files()
        .filter(|file| file.path().extension().is_some_and(|ext| ext == "sql"))
        .map(|file| {
            let sql = file.contents_utf8().expect("migrations are UTF-8");
            Migration {
                version: file
                    .path()
                    .file_stem()
                    .expect("a migration has a name")
                    .to_string_lossy()
                    .into_owned(),
                sql,
                sha256: checksum(sql),
            }
        })
        .collect();
    list.sort_by(|a, b| a.version.cmp(&b.version));
    list
}

/// How long a migration waits for a lock, unless it sets its own
/// `lock_timeout`. While it waits, later requests for the table queue behind
/// it, and pgh_reader's queries time out after 10 s.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Serializes migrators, such as two deployments' bootstraps.
const MIGRATOR_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtext('pghacker.migrate'))";

const SEARCH_PATH: &str = "SET LOCAL search_path = pghacker, ag_catalog, public";

/// Applies the migrations that haven't been, as pgh_indexer: in name order,
/// each in its own transaction. Returns the versions applied.
///
/// A migration that changed after it was applied is an error before any
/// other is applied. A failed migration rolls back alone and stops the run:
/// those applied before it stay applied, and the next run starts with it.
/// One that waits longer than its `lock_timeout` for a lock, or deadlocks,
/// fails at once, and the error lists the sessions that hold locks.
pub async fn migrate(indexer: &mut Client) -> Result<Vec<String>> {
    migrate_with(indexer, &migrations(), LOCK_TIMEOUT).await
}

async fn migrate_with(
    indexer: &mut Client,
    migrations: &[Migration],
    lock_timeout: Duration,
) -> Result<Vec<String>> {
    let applied = prepare(indexer, migrations).await?;
    let mut done = Vec::new();
    for migration in migrations
        .iter()
        .filter(|m| !applied.contains_key(&m.version))
    {
        let started = Instant::now();
        match apply(indexer, migration, lock_timeout).await {
            Ok(true) => {
                tracing::info!(
                    version = %migration.version,
                    ms = started.elapsed().as_millis() as u64,
                    "applied a migration"
                );
                done.push(migration.version.clone());
            }
            // A concurrent migrator applied it first.
            Ok(false) => {}
            Err(error) => return Err(failure(indexer, migration, &done, error).await),
        }
    }
    let tx = indexer.transaction().await?;
    tx.batch_execute(SEARCH_PATH).await?;
    crate::graph_indexes::check(&tx).await.with_context(|| {
        format!(
            "graph indexes are not ready after migrations{}",
            kept(&done)
        )
    })?;
    tx.commit().await?;
    Ok(done)
}

/// Creates the ledger if needed and returns it, after checking that no
/// applied migration has changed.
async fn prepare(
    indexer: &mut Client,
    migrations: &[Migration],
) -> Result<HashMap<String, String>> {
    let tx = indexer.transaction().await?;
    tx.batch_execute(&format!(
        "{MIGRATOR_LOCK}; {SEARCH_PATH};
         CREATE TABLE IF NOT EXISTS schema_migrations (
             version    text PRIMARY KEY,
             sha256     text NOT NULL,
             applied_at timestamptz NOT NULL DEFAULT now()
         );"
    ))
    .await
    .context("cannot prepare the migrations")?;
    let applied: HashMap<String, String> = tx
        .query("SELECT version, sha256 FROM schema_migrations", &[])
        .await?
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    tx.commit().await?;
    for migration in migrations {
        if applied
            .get(&migration.version)
            .is_some_and(|sha| !migration.recorded_as(sha))
        {
            bail!(changed(migration));
        }
    }
    Ok(applied)
}

/// Applies `migration` in a transaction of its own. False when a concurrent
/// migrator applied it first.
async fn apply(
    indexer: &mut Client,
    migration: &Migration,
    lock_timeout: Duration,
) -> Result<bool> {
    let tx = indexer.transaction().await?;
    // The migrator lock first: waiting for another migrator isn't a lock
    // timeout. The migration may set its own lock_timeout.
    tx.batch_execute(&format!(
        "{MIGRATOR_LOCK}; {SEARCH_PATH}; SET LOCAL lock_timeout = {};",
        lock_timeout.as_millis()
    ))
    .await?;
    let recorded: Option<String> = tx
        .query_opt(
            "SELECT sha256 FROM schema_migrations WHERE version = $1",
            &[&migration.version],
        )
        .await?
        .map(|row| row.get(0));
    match recorded {
        Some(sha) if migration.recorded_as(&sha) => return Ok(false),
        Some(_) => bail!(changed(migration)),
        None => {}
    }
    tx.batch_execute(migration.sql).await?;
    tx.execute(
        "INSERT INTO schema_migrations (version, sha256) VALUES ($1, $2)",
        &[&migration.version, &migration.sha256],
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

fn changed(migration: &Migration) -> String {
    format!(
        "migration {} changed after it was applied; reset the local database with \
         `npm run db:reset`",
        migration.version
    )
}

/// What a failure leaves applied: the migrations this run applied first.
fn kept(done: &[String]) -> String {
    match done {
        [] => String::new(),
        [one] => format!("; this run applied {one}, which stays applied"),
        [first @ .., last] => format!(
            "; this run applied {} and {last}, which stay applied",
            first.join(", ")
        ),
    }
}

/// The error of `migration`, which failed after this run applied `done`.
/// When it waited too long for a lock or deadlocked, the error and the log
/// name the sessions that hold locks on the schema's tables, oldest first:
/// the query tool can't see other roles' sessions.
async fn failure(
    indexer: &Client,
    migration: &Migration,
    done: &[String],
    error: anyhow::Error,
) -> anyhow::Error {
    let failed = format!("migration {} failed{}", migration.version, kept(done));
    let Some(cause) = lock_failure(&error) else {
        return error.context(failed);
    };
    // The failed transaction's rollback is queued ahead of this query.
    let holders = match lock_holders(indexer).await {
        Ok(holders) if holders.is_empty() => {
            "no other session holds a lock on the schema's tables now".to_owned()
        }
        Ok(holders) => format!(
            "sessions holding locks on the schema's tables, oldest transaction first: {}",
            holders.join("; ")
        ),
        Err(snapshot) => format!("cannot list the sessions holding locks: {snapshot:#}"),
    };
    tracing::warn!(version = %migration.version, cause, %holders, "a migration failed on a lock");
    error
        .context(format!("it {cause}; {holders}"))
        .context(failed)
}

/// What kind of lock failure `error` is, if it is one.
fn lock_failure(error: &anyhow::Error) -> Option<&'static str> {
    let code = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
        .and_then(tokio_postgres::Error::code)?;
    if *code == SqlState::LOCK_NOT_AVAILABLE {
        Some("timed out waiting for a lock")
    } else if *code == SqlState::T_R_DEADLOCK_DETECTED {
        Some("deadlocked")
    } else {
        None
    }
}

/// The other sessions that hold locks on tables of the schemas migrations
/// change, oldest transaction first, at most 10. pgh_indexer sees all of its
/// own role's sessions; of other roles', only their pid, role, application
/// and locks.
async fn lock_holders(client: &Client) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT a.pid, a.usename::text, nullif(a.application_name, ''),
                    date_trunc('second', now() - a.backend_start)::text,
                    date_trunc('second', now() - a.xact_start)::text,
                    a.state, a.wait_event_type || ':' || a.wait_event,
                    left(regexp_replace(a.query, '\\s+', ' ', 'g'), 200),
                    string_agg(format('%I.%I %s', n.nspname, c.relname, l.mode), ', '
                               ORDER BY n.nspname, c.relname, l.mode)
             FROM pg_locks l
             JOIN pg_class c ON c.oid = l.relation
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_stat_activity a ON a.pid = l.pid
             WHERE l.locktype = 'relation' AND l.granted AND l.pid <> pg_backend_pid()
               AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
               AND c.relkind IN ('r', 'p', 'm')
               AND n.nspname IN ('pghacker', 'pghacker_graph', 'ag_catalog', 'public')
             GROUP BY a.pid, a.usename, a.application_name, a.backend_start, a.xact_start,
                      a.state, a.wait_event_type, a.wait_event, a.query
             ORDER BY a.xact_start NULLS LAST, a.pid
             LIMIT 10",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let field = |index: usize, label: &str| {
                row.get::<_, Option<String>>(index)
                    .map(|value| format!("{label}{value}"))
            };
            let about: Vec<String> = [
                field(1, ""),
                field(2, ""),
                field(3, "connected for "),
                field(4, "in its transaction for "),
                field(5, ""),
                field(6, "waiting on "),
            ]
            .into_iter()
            .flatten()
            .collect();
            let pid: i32 = row.get(0);
            let mut holder = format!("pid {pid} ({})", about.join(", "));
            if let Some(query) = row.get::<_, Option<String>>(7) {
                holder.push_str(&format!(", running {query:?}"));
            }
            let locks: String = row.get(8);
            format!("{holder}, holds {locks}")
        })
        .collect())
}

/// Drops schema pghacker and the graph, for a fresh local start. The
/// administrator must then bootstrap and migrate again.
pub async fn reset(admin: &Client) -> Result<()> {
    admin
        .batch_execute(
            "DROP SCHEMA IF EXISTS pghacker CASCADE;
             DO $$
             BEGIN
                 IF EXISTS (SELECT FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph') THEN
                     PERFORM ag_catalog.drop_graph('pghacker_graph', true);
                 END IF;
             END
             $$;",
        )
        .await
        .context("cannot reset the database")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Migration, checksum, migrate_with};
    use crate::{db, testing::TestDb};

    #[tokio::test]
    async fn a_new_registry_label_is_indexed_atomically() {
        let db = crate::testing::TestDb::create().await.unwrap();
        let mut admin = db.admin().await.unwrap();
        let mut edges = crate::EDGE_LABELS.to_vec();
        edges.push("FUTURE_EDGE_TEST");
        super::bootstrap_database_with_labels(&mut admin, crate::VERTEX_LABELS, &edges)
            .await
            .unwrap();
        super::bootstrap_database_with_labels(&mut admin, crate::VERTEX_LABELS, &edges)
            .await
            .unwrap();
        let requirement = crate::graph_indexes::inspect(&admin)
            .await
            .unwrap()
            .into_iter()
            .find(|r| {
                r.kind == crate::graph_indexes::Kind::EdgeId
                    && r.label.as_deref() == Some("FUTURE_EDGE_TEST")
            })
            .unwrap();
        assert_eq!(requirement.state, crate::graph_indexes::State::Ready);
        assert_eq!(requirement.usable_indexes, ["FUTURE_EDGE_TEST_id_idx"]);
    }

    #[tokio::test]
    async fn failed_readiness_does_not_publish_a_new_label() {
        let db = crate::testing::TestDb::create().await.unwrap();
        let mut admin = db.admin().await.unwrap();
        admin
            .batch_execute("DROP INDEX pghacker_graph.\"ABOUT_id_idx\"")
            .await
            .unwrap();
        let mut edges = crate::EDGE_LABELS.to_vec();
        edges.push("UNPUBLISHED_EDGE_TEST");
        assert!(
            super::bootstrap_database_with_labels(&mut admin, crate::VERTEX_LABELS, &edges)
                .await
                .is_err()
        );
        let exists: bool = admin
            .query_one(
                "SELECT EXISTS (SELECT FROM ag_catalog.ag_label WHERE name = 'UNPUBLISHED_EDGE_TEST')",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!exists);
    }

    #[test]
    fn migrations_are_ordered_and_named() {
        let list = super::migrations();
        assert!(list.len() >= 5);
        assert!(
            list.windows(2)
                .all(|pair| pair[0].version < pair[1].version)
        );
        assert_eq!(list[0].version, "0001_core");
    }

    #[test]
    fn a_checksum_leaves_out_the_license_header_and_line_ends() {
        use sha2::{Digest, Sha256};
        let body = "-- A migration.\nCREATE TABLE t (id int);\n";
        let plain = hex::encode(Sha256::digest(body.as_bytes()));
        assert_eq!(super::checksum(body), plain);
        let crlf = |text: &str| text.replace('\n', "\r\n");
        for header in [
            super::LICENSE_HEADER.to_owned(),
            crlf(super::LICENSE_HEADER),
        ] {
            assert_eq!(super::checksum(&format!("{header}{body}")), plain);
            // Only a header at the start is left out.
            assert_ne!(super::checksum(&format!("{body}{header}")), plain);
        }
        // A CRLF checkout has the same checksum, and the one older binaries
        // recorded from it is the migration's too.
        let sql: &'static str = Box::leak(crlf(&format!("{}{body}", super::LICENSE_HEADER)).into());
        assert_eq!(super::checksum(sql), plain);
        let migration = migration("9000_crlf", sql);
        let older = hex::encode(Sha256::digest(crlf(body).as_bytes()));
        assert!(migration.recorded_as(&plain));
        assert!(migration.recorded_as(&older));
        assert!(!migration.recorded_as(&super::checksum("SELECT 1;\n")));
    }

    /// The lock timeout of most tests: short, as nothing should wait.
    const SHORT: Duration = Duration::from_millis(200);

    fn migration(version: &str, sql: &'static str) -> Migration {
        Migration {
            version: version.to_owned(),
            sql,
            sha256: checksum(sql),
        }
    }

    /// The test migrations applied, which the real ones' names sort before.
    async fn applied(client: &tokio_postgres::Client) -> Vec<String> {
        client
            .query(
                "SELECT version FROM schema_migrations WHERE version LIKE '9%' ORDER BY version",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    #[tokio::test]
    async fn a_failed_migration_keeps_those_before_it() {
        let db = TestDb::create().await.unwrap();
        let mut indexer = db.indexer().await.unwrap();
        let failing = [
            migration("9001_test_first", "CREATE TABLE test_first (id int)"),
            migration("9002_test_second", "SELECT 1 / 0"),
            migration("9003_test_third", "CREATE TABLE test_third (id int)"),
        ];
        let error = migrate_with(&mut indexer, &failing, SHORT)
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.starts_with(
                "migration 9002_test_second failed; this run applied 9001_test_first, \
                 which stays applied: "
            ),
            "{message}"
        );
        assert!(message.contains("division by zero"), "{message}");
        assert_eq!(applied(&indexer).await, ["9001_test_first"]);
        let third: Option<String> = indexer
            .query_one("SELECT to_regclass('test_third')::text", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(third, None);

        let fixed = [
            migration("9001_test_first", "CREATE TABLE test_first (id int)"),
            migration("9002_test_second", "CREATE TABLE test_second (id int)"),
            migration("9003_test_third", "CREATE TABLE test_third (id int)"),
        ];
        assert_eq!(
            migrate_with(&mut indexer, &fixed, SHORT).await.unwrap(),
            ["9002_test_second", "9003_test_third"]
        );
    }

    /// Two tables, and a session that holds the second in ACCESS SHARE until
    /// its transaction ends: as an old revision's long query would.
    async fn blocked(db: &TestDb) -> tokio_postgres::Client {
        db.indexer()
            .await
            .unwrap()
            .batch_execute("CREATE TABLE test_one (id int); CREATE TABLE test_two (id int);")
            .await
            .unwrap();
        let blocker = db::connect(&db.indexer_url, "pghacker-test blocker")
            .await
            .unwrap();
        blocker
            .batch_execute("BEGIN; LOCK TABLE test_two IN ACCESS SHARE MODE;")
            .await
            .unwrap();
        blocker
    }

    fn alter_both() -> [Migration; 2] {
        [
            migration("9001_test_one", "ALTER TABLE test_one ADD COLUMN x int"),
            migration("9002_test_two", "ALTER TABLE test_two ADD COLUMN x int"),
        ]
    }

    #[tokio::test]
    async fn a_lock_timeout_fails_at_once_and_names_the_holders() {
        let db = TestDb::create().await.unwrap();
        let blocker = blocked(&db).await;
        let mut indexer = db.indexer().await.unwrap();
        let started = Instant::now();
        let error = migrate_with(&mut indexer, &alter_both(), SHORT)
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        let message = format!("{error:#}");
        for expected in [
            "migration 9002_test_two failed; this run applied 9001_test_one, which stays applied: ",
            "it timed out waiting for a lock; sessions holding locks",
            "(pgh_indexer, pghacker-test blocker, connected for ",
            "idle in transaction",
            "LOCK TABLE test_two IN ACCESS SHARE MODE",
            "holds pghacker.test_two AccessShareLock",
            "lock timeout",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
        assert_eq!(applied(&indexer).await, ["9001_test_one"]);
        blocker.batch_execute("ROLLBACK").await.unwrap();
        assert_eq!(
            migrate_with(&mut indexer, &alter_both(), SHORT)
                .await
                .unwrap(),
            ["9002_test_two"]
        );
    }

    #[tokio::test]
    async fn a_waiting_migration_holds_only_its_own_locks() {
        let db = TestDb::create().await.unwrap();
        let blocker = blocked(&db).await;
        let mut indexer = db.indexer().await.unwrap();
        let migrating = tokio::spawn(async move {
            migrate_with(&mut indexer, &alter_both(), Duration::from_secs(30)).await
        });
        let observer = db.indexer().await.unwrap();
        let mut waiting = false;
        for _ in 0..200 {
            waiting = observer
                .query_one(
                    "SELECT EXISTS (SELECT FROM pg_locks
                                    WHERE NOT granted AND relation = 'test_two'::regclass)",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(waiting, "9002 never waited for test_two");
        assert_eq!(applied(&observer).await, ["9001_test_one"]);
        // 9001 committed, so test_one is free while 9002 waits.
        observer
            .batch_execute("BEGIN; LOCK TABLE test_one IN ACCESS EXCLUSIVE MODE NOWAIT; ROLLBACK;")
            .await
            .unwrap();
        blocker.batch_execute("ROLLBACK").await.unwrap();
        assert_eq!(
            migrating.await.unwrap().unwrap(),
            ["9001_test_one", "9002_test_two"]
        );
    }

    #[tokio::test]
    async fn concurrent_migrators_apply_each_migration_once() {
        const SQL: [&str; 6] = [
            "CREATE TABLE test_c1 (id int)",
            "CREATE TABLE test_c2 (id int)",
            "CREATE TABLE test_c3 (id int)",
            "CREATE TABLE test_c4 (id int)",
            "CREATE TABLE test_c5 (id int)",
            "CREATE TABLE test_c6 (id int)",
        ];
        let list = || -> Vec<Migration> {
            SQL.into_iter()
                .enumerate()
                .map(|(n, sql)| migration(&format!("900{}_test_concurrent", n + 1), sql))
                .collect()
        };
        let db = TestDb::create().await.unwrap();
        let (mut a, mut b) = (db.indexer().await.unwrap(), db.indexer().await.unwrap());
        let (list_a, list_b) = (list(), list());
        let (by_a, by_b) = tokio::join!(
            migrate_with(&mut a, &list_a, SHORT),
            migrate_with(&mut b, &list_b, SHORT)
        );
        let mut both = by_a.unwrap();
        both.extend(by_b.unwrap());
        both.sort();
        let all: Vec<String> = list_a.into_iter().map(|m| m.version).collect();
        assert_eq!(both, all);
        assert_eq!(applied(&a).await, all);
    }

    #[tokio::test]
    async fn a_changed_migration_is_refused_before_any_is_applied() {
        let db = TestDb::create().await.unwrap();
        let mut indexer = db.indexer().await.unwrap();
        migrate_with(
            &mut indexer,
            &[migration(
                "9002_test_changed",
                "CREATE TABLE test_changed (id int)",
            )],
            SHORT,
        )
        .await
        .unwrap();
        let changed = [
            migration("9001_test_pending", "CREATE TABLE test_pending (id int)"),
            migration("9002_test_changed", "CREATE TABLE test_changed (id bigint)"),
        ];
        let error = migrate_with(&mut indexer, &changed, SHORT)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("migration 9002_test_changed changed after it was applied"),
            "{error:#}"
        );
        assert_eq!(applied(&indexer).await, ["9002_test_changed"]);
    }

    #[tokio::test]
    async fn unready_graph_indexes_fail_the_run_but_keep_its_migrations() {
        let db = TestDb::create().await.unwrap();
        db.admin()
            .await
            .unwrap()
            .batch_execute("DROP INDEX pghacker_graph.\"MENTIONS_id_idx\"")
            .await
            .unwrap();
        let mut indexer = db.indexer().await.unwrap();
        let error = migrate_with(
            &mut indexer,
            &[migration(
                "9001_test_kept",
                "CREATE TABLE test_kept (id int)",
            )],
            SHORT,
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.starts_with(
                "graph indexes are not ready after migrations; this run applied 9001_test_kept, \
                 which stays applied: "
            ),
            "{message}"
        );
        assert_eq!(applied(&indexer).await, ["9001_test_kept"]);
    }
}
