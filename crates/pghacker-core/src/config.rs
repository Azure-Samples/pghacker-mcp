//! Settings from the environment. The services read only their process
//! environment, which the launcher builds; tests and local tools also read
//! the repository's `.env`, under the process environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use url::Url;

/// Environment variables, by name.
#[derive(Clone, Debug, Default)]
pub struct Env {
    values: HashMap<String, String>,
}

impl Env {
    /// The process environment.
    pub fn process() -> Self {
        Self {
            values: std::env::vars().collect(),
        }
    }

    /// The process environment over the repository's `.env`: for tests and
    /// local tools, which run without the launcher.
    pub fn local() -> Result<Self> {
        let mut values = match repo_root() {
            Some(root) => read_env_file(&root.join(".env"))?,
            None => HashMap::new(),
        };
        values.extend(std::env::vars());
        Ok(Self { values })
    }

    /// An environment of exactly these pairs.
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// The variable, unless it's unset or blank.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
    }

    pub fn require(&self, key: &str) -> Result<&str> {
        self.get(key).ok_or_else(|| anyhow!("{key} must be set"))
    }

    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    /// The variable parsed, or `default` if it's unset.
    pub fn parse_or<T: FromStr>(&self, key: &str, default: T) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        match self.get(key) {
            Some(value) => value
                .trim()
                .parse()
                .map_err(|error| anyhow!("{key} is invalid: {error}")),
            None => Ok(default),
        }
    }

    /// The administrator's URL, for bootstrap only: PilotSwarm's own
    /// `DATABASE_URL` locally.
    pub fn admin_database_url(&self) -> Result<String> {
        self.get("PGHACKER_ADMIN_DATABASE_URL")
            .or_else(|| self.get("DATABASE_URL"))
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("PGHACKER_ADMIN_DATABASE_URL or DATABASE_URL must be set"))
    }

    /// pgh_indexer's URL: `PGHACKER_INDEXER_DATABASE_URL`, or the
    /// administrator's URL with pgh_indexer and `PGH_INDEXER_PASSWORD`.
    pub fn indexer_database_url(&self) -> Result<String> {
        self.role_url(
            "PGHACKER_INDEXER_DATABASE_URL",
            "pgh_indexer",
            "PGH_INDEXER_PASSWORD",
        )
    }

    /// pgh_reader's URL: `PGHACKER_READER_DATABASE_URL`, or the
    /// administrator's URL with pgh_reader and `PGH_READER_PASSWORD`.
    pub fn reader_database_url(&self) -> Result<String> {
        self.role_url(
            "PGHACKER_READER_DATABASE_URL",
            "pgh_reader",
            "PGH_READER_PASSWORD",
        )
    }

    /// The roles' passwords, for bootstrap; pgh_swarm's is optional.
    pub fn passwords(&self) -> Result<crate::migrate::Passwords> {
        Ok(crate::migrate::Passwords {
            indexer: self.require("PGH_INDEXER_PASSWORD")?.to_owned(),
            reader: self.require("PGH_READER_PASSWORD")?.to_owned(),
            swarm: self.get("PGH_SWARM_PASSWORD").map(str::to_owned),
        })
    }

    fn role_url(&self, key: &str, role: &str, password_key: &str) -> Result<String> {
        if let Some(url) = self.get(key) {
            return Ok(url.to_owned());
        }
        let admin = self.admin_database_url()?;
        let password = self
            .get(password_key)
            .ok_or_else(|| anyhow!("{key} or {password_key} must be set"))?;
        with_credentials(&admin, role, password)
    }
}

/// `url` with another user and password.
pub fn with_credentials(url: &str, user: &str, password: &str) -> Result<String> {
    let mut parsed = Url::parse(url).context("the database URL is invalid")?;
    parsed
        .set_username(user)
        .map_err(|()| anyhow!("cannot set the database URL's user"))?;
    parsed
        .set_password(Some(password))
        .map_err(|()| anyhow!("cannot set the database URL's password"))?;
    Ok(parsed.into())
}

/// `url` for another database on the same server.
pub fn with_database(url: &str, database: &str) -> Result<String> {
    let mut parsed = Url::parse(url).context("the database URL is invalid")?;
    parsed.set_path(&format!("/{database}"));
    Ok(parsed.into())
}

/// The repository's root: `PGHACKER_ROOT`, or the directory with compose.yaml
/// above the current directory or this crate.
pub fn repo_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("PGHACKER_ROOT") {
        return Some(PathBuf::from(root));
    }
    let starts = [
        std::env::current_dir().ok(),
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
    ];
    starts.into_iter().flatten().find_map(|start| {
        start
            .ancestors()
            .find(|dir| dir.join("compose.yaml").is_file() && dir.join("Cargo.toml").is_file())
            .map(Path::to_path_buf)
    })
}

/// A dotenv file's `KEY=VALUE` lines; a missing file is empty.
pub fn read_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    Ok(parse_env(&text))
}

fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            Some((key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_lines() {
        let values = parse_env("# comment\nA=1\nexport B = \"two\"\nC='3'\n\nD=x=y\n");
        assert_eq!(values["A"], "1");
        assert_eq!(values["B"], "two");
        assert_eq!(values["C"], "3");
        assert_eq!(values["D"], "x=y");
    }

    #[test]
    fn derives_role_urls_from_the_admin_url() {
        let env = Env::from_pairs([
            (
                "DATABASE_URL",
                "postgresql://pghacker:secret@127.0.0.1:55433/pghacker_mcp",
            ),
            ("PGH_INDEXER_PASSWORD", "a/b@c"),
            ("PGH_READER_PASSWORD", "r"),
        ]);
        let indexer = env.indexer_database_url().unwrap();
        assert_eq!(
            indexer,
            "postgresql://pgh_indexer:a%2Fb%40c@127.0.0.1:55433/pghacker_mcp"
        );
        let config: tokio_postgres::Config = indexer.parse().unwrap();
        assert_eq!(config.get_password(), Some(&b"a/b@c"[..]));
        assert_eq!(
            env.reader_database_url().unwrap(),
            "postgresql://pgh_reader:r@127.0.0.1:55433/pghacker_mcp"
        );
        assert_eq!(
            with_database(&indexer, "other").unwrap(),
            "postgresql://pgh_indexer:a%2Fb%40c@127.0.0.1:55433/other"
        );
    }
}
