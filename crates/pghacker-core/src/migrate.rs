//! The bootstrap, which the administrator runs, and the migrations, which
//! pgh_indexer runs: both on every start, both idempotent.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

static MIGRATIONS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../migrations");
const ROLES: &str = include_str!("../sql/roles.sql");
const DATABASE: &str = include_str!("../sql/database.sql");

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
        tx.commit().await
    })
    .await
    .context("cannot create the roles")
}

/// Runs `step` in its own transactions until it doesn't collide with a
/// concurrent bootstrap on the cluster's shared catalogs, up to 5 times.
async fn retry_races(
    admin: &mut Client,
    step: impl AsyncFn(&mut Client) -> std::result::Result<(), tokio_postgres::Error>,
) -> std::result::Result<(), tokio_postgres::Error> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match step(admin).await {
            Err(error) if attempt < 5 && is_race(&error) => {
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
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The database's extensions, schema, graph, labels and grants.
pub async fn bootstrap_database(admin: &mut Client) -> Result<()> {
    retry_races(admin, async |admin: &mut Client| {
        let tx = admin.transaction().await?;
        tx.batch_execute(DATABASE).await?;
        tx.commit().await
    })
    .await
    .context("cannot bootstrap the database")
}

/// Whether `error` is a collision with a concurrent bootstrap.
fn is_race(error: &tokio_postgres::Error) -> bool {
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
                sha256: hex::encode(Sha256::digest(sql.as_bytes())),
            }
        })
        .collect();
    list.sort_by(|a, b| a.version.cmp(&b.version));
    list
}

/// Applies the migrations that haven't been, as pgh_indexer, in one
/// transaction. Returns the versions applied. A migration that changed after
/// it was applied is an error.
pub async fn migrate(indexer: &mut Client) -> Result<Vec<String>> {
    let tx = indexer.transaction().await?;
    tx.batch_execute(
        "SELECT pg_advisory_xact_lock(hashtext('pghacker.migrate'));
         SET LOCAL search_path = pghacker, ag_catalog, public;
         CREATE TABLE IF NOT EXISTS schema_migrations (
             version    text PRIMARY KEY,
             sha256     text NOT NULL,
             applied_at timestamptz NOT NULL DEFAULT now()
         );",
    )
    .await
    .context("cannot prepare the migrations")?;
    let applied: HashMap<String, String> = tx
        .query("SELECT version, sha256 FROM schema_migrations", &[])
        .await?
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let mut done = Vec::new();
    for migration in migrations() {
        match applied.get(&migration.version) {
            Some(sha) if *sha == migration.sha256 => continue,
            Some(_) => bail!(
                "migration {} changed after it was applied; reset the local database with \
                 `npm run db:reset`",
                migration.version
            ),
            None => {
                tx.batch_execute(migration.sql)
                    .await
                    .with_context(|| format!("migration {} failed", migration.version))?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, sha256) VALUES ($1, $2)",
                    &[&migration.version, &migration.sha256],
                )
                .await?;
                done.push(migration.version);
            }
        }
    }
    tx.commit().await?;
    Ok(done)
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
}
