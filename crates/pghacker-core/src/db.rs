//! PostgreSQL connections and pools.

use anyhow::{Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, RecyclingMethod};
use tokio_postgres::{Client, Config, NoTls};

pub use deadpool_postgres::Pool;

/// A pool for `url`, named `application_name` in pg_stat_activity.
pub fn pool(url: &str, max_size: usize, application_name: &str) -> Result<Pool> {
    let mut config: Config = url.parse().context("the database URL is invalid")?;
    config.application_name(application_name);
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .context("cannot build the connection pool")
}

/// One connection to `url`; its connection task runs in the background.
pub async fn connect(url: &str, application_name: &str) -> Result<Client> {
    let mut config: Config = url.parse().context("the database URL is invalid")?;
    config.application_name(application_name);
    let (client, connection) = config
        .connect(NoTls)
        .await
        .with_context(|| format!("cannot connect to {}", redact(url)))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "a database connection ended");
        }
    });
    Ok(client)
}

/// `url` without its password, for messages.
pub fn redact(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            if parsed.password().is_some() {
                let _ = parsed.set_password(Some("***"));
            }
            parsed.into()
        }
        Err(_) => "<invalid database URL>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn redacts_passwords() {
        assert_eq!(
            super::redact("postgresql://u:secret@h:5432/d"),
            "postgresql://u:***@h:5432/d"
        );
    }
}
