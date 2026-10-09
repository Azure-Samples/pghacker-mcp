//! Databases for integration tests: each [`TestDb`] is a fresh database on
//! the local PostgreSQL, bootstrapped and migrated, and dropped with it.
//!
//! Tests read the repository's `.env` for the administrator's URL and the
//! roles' passwords, so `npm run setup` and a running database are needed.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::config::{Env, with_database};
use crate::db::{self, Pool};
use crate::migrate;

/// A fresh, migrated database, dropped when this is.
pub struct TestDb {
    pub name: String,
    pub admin_url: String,
    pub indexer_url: String,
    pub reader_url: String,
    maintenance_url: String,
}

impl TestDb {
    pub async fn create() -> Result<Self> {
        let env = Env::local()?;
        let admin = env.admin_database_url()?;
        let maintenance_url = with_database(&admin, "postgres")?;
        let maintenance = db::connect(&maintenance_url, "pghacker-test")
            .await
            .context(
                "the tests need the local database: run `npm run setup` and `npm run db:up`",
            )?;
        let name = format!("pgh_test_{}", uuid::Uuid::new_v4().simple());
        maintenance
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .context("cannot create the test database")?;
        let admin_url = with_database(&admin, &name)?;
        let indexer_url = with_database(&env.indexer_database_url()?, &name)?;
        let reader_url = with_database(&env.reader_database_url()?, &name)?;
        let test = Self {
            name,
            admin_url,
            indexer_url,
            reader_url,
            maintenance_url,
        };
        let mut admin = db::connect(&test.admin_url, "pghacker-test").await?;
        if !migrate::roles_exist(&admin).await? {
            migrate::bootstrap_roles(&mut admin, &env.passwords()?).await?;
        }
        migrate::bootstrap_database(&mut admin).await?;
        let mut indexer = test.indexer().await?;
        migrate::migrate(&mut indexer).await?;
        Ok(test)
    }

    pub async fn admin(&self) -> Result<Client> {
        db::connect(&self.admin_url, "pghacker-test").await
    }

    pub async fn indexer(&self) -> Result<Client> {
        db::connect(&self.indexer_url, "pghacker-test").await
    }

    pub async fn reader(&self) -> Result<Client> {
        db::connect(&self.reader_url, "pghacker-test").await
    }

    pub fn indexer_pool(&self, size: usize) -> Result<Pool> {
        db::pool(&self.indexer_url, size, "pghacker-test")
    }

    pub fn reader_pool(&self, size: usize) -> Result<Pool> {
        db::pool(&self.reader_url, size, "pghacker-test")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let url = self.maintenance_url.clone();
        let name = self.name.clone();
        // Drop may run inside an async test: drop the database from a thread
        // with a runtime of its own.
        let handle = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                if let Ok(client) = db::connect(&url, "pghacker-test").await {
                    let _ = client
                        .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .await;
                }
            });
        });
        let _ = handle.join();
    }
}
