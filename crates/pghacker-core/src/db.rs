//! PostgreSQL connections and pools.
//!
//! Every connection offers TLS as the URL's `sslmode` asks: `prefer`,
//! PostgreSQL's default, uses it when the server does; `require` insists; and
//! `disable` connects in plain text, as the local database does. TLS is rustls
//! with Mozilla's roots, plus the PEM certificates in `PGHACKER_DB_CA_FILE`,
//! and always verifies the server's certificate and host name.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use deadpool_postgres::{Manager, ManagerConfig, RecyclingMethod};
use rustls::RootCertStore;
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use tokio_postgres::{CancelToken, Client, Config};
use tokio_postgres_rustls::MakeRustlsConnect;

pub use deadpool_postgres::Pool;

/// The TLS connector of every connection, built once.
pub fn tls() -> Result<MakeRustlsConnect> {
    static TLS: OnceLock<std::result::Result<MakeRustlsConnect, String>> = OnceLock::new();
    TLS.get_or_init(|| {
        let ca_file = std::env::var_os("PGHACKER_DB_CA_FILE").filter(|value| !value.is_empty());
        connector(ca_file.as_deref().map(Path::new)).map_err(|error| format!("{error:#}"))
    })
    .clone()
    .map_err(|error| anyhow!(error))
}

/// A connector trusting Mozilla's roots and the certificates in `ca_file`.
pub fn connector(ca_file: Option<&Path>) -> Result<MakeRustlsConnect> {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("cannot configure TLS")?
    .with_root_certificates(root_store(ca_file)?)
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// Mozilla's roots, plus the certificates in `ca_file`.
pub fn root_store(ca_file: Option<&Path>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_file {
        let certificates = CertificateDer::pem_file_iter(path)
            .with_context(|| format!("cannot read PGHACKER_DB_CA_FILE {}", path.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("PGHACKER_DB_CA_FILE {} isn't PEM", path.display()))?;
        let (added, _) = roots.add_parsable_certificates(certificates);
        if added == 0 {
            bail!(
                "PGHACKER_DB_CA_FILE {} has no usable certificate",
                path.display()
            );
        }
    }
    Ok(roots)
}

/// A pool for `url`, named `application_name` in pg_stat_activity.
pub fn pool(url: &str, max_size: usize, application_name: &str) -> Result<Pool> {
    let config: Config = url.parse().context("the database URL is invalid")?;
    build_pool(config, max_size, application_name)
}

/// A pool for `url` whose connections start with `options`, the startup
/// parameter, after any the URL gives: settings such as `-c
/// statement_timeout=30000`, which hold for the connection's life.
pub fn pool_with_options(
    url: &str,
    max_size: usize,
    application_name: &str,
    options: &str,
) -> Result<Pool> {
    let mut config: Config = url.parse().context("the database URL is invalid")?;
    let options = match config.get_options().map(str::trim) {
        Some(given) if !given.is_empty() => format!("{given} {options}"),
        _ => options.to_owned(),
    };
    config.options(options);
    build_pool(config, max_size, application_name)
}

fn build_pool(mut config: Config, max_size: usize, application_name: &str) -> Result<Pool> {
    config.application_name(application_name);
    let manager = Manager::from_config(
        config,
        tls()?,
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
        .connect(tls()?)
        .await
        .with_context(|| format!("cannot connect to {}", redact(url)))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "a database connection ended");
        }
    });
    Ok(client)
}

/// Cancels the statement running on `token`'s connection, over TLS like the
/// connection itself.
pub async fn cancel_query(token: &CancelToken) -> Result<()> {
    token
        .cancel_query(tls()?)
        .await
        .context("cannot cancel the statement")
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
    use super::*;

    /// A self-signed certificate, for the CA file.
    const TEST_CA: &str = include_str!("../tests/fixtures/test-ca.pem");

    #[test]
    fn redacts_passwords() {
        assert_eq!(
            super::redact("postgresql://u:secret@h:5432/d"),
            "postgresql://u:***@h:5432/d"
        );
    }

    #[test]
    fn trusts_mozillas_roots_and_the_ca_file() {
        let mozilla = root_store(None).unwrap().len();
        assert!(mozilla > 100);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ca.pem");
        std::fs::write(&file, TEST_CA).unwrap();
        assert_eq!(root_store(Some(&file)).unwrap().len(), mozilla + 1);
        std::fs::write(&file, "no certificates here\n").unwrap();
        assert!(root_store(Some(&file)).is_err());
        assert!(root_store(Some(&dir.path().join("missing.pem"))).is_err());
        assert!(connector(None).is_ok());
    }

    #[tokio::test]
    async fn require_refuses_a_server_without_tls() {
        // The local database has no TLS: prefer connects in plain text, require refuses.
        let env = crate::config::Env::local().unwrap();
        let admin = env.admin_database_url().unwrap();
        let separator = if admin.contains('?') { '&' } else { '?' };
        let client = connect(
            &format!("{admin}{separator}sslmode=prefer"),
            "pghacker-test",
        )
        .await
        .unwrap();
        let one: i32 = client.query_one("SELECT 1", &[]).await.unwrap().get(0);
        assert_eq!(one, 1);
        let error = connect(
            &format!("{admin}{separator}sslmode=require"),
            "pghacker-test",
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").to_lowercase().contains("tls"),
            "{error:#}"
        );
    }
}
