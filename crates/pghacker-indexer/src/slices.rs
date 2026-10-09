//! The test slices (`config/test-slices.toml`): which months and threads of a
//! mailing list, and which paths of a PostgreSQL version, a source retrieves
//! until wave 3 retrieves everything.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Slices {
    pub mail: MailSlice,
    pub code: CodeSlice,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MailSlice {
    pub list: String,
    /// YYYY-MM.
    pub months: Vec<String>,
    pub seed_threads: Vec<SeedThread>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SeedThread {
    pub topic: String,
    pub message_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CodeSlice {
    pub repo_url: String,
    pub branch: String,
    pub sha: String,
    pub version: String,
    pub paths: Vec<String>,
}

impl Slices {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the test slices {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let slices: Self = toml::from_str(text).context("the test slices are invalid")?;
        for month in &slices.mail.months {
            let valid = month.len() == 7
                && month.as_bytes()[4] == b'-'
                && month[..4].parse::<u16>().is_ok()
                && month[5..]
                    .parse::<u8>()
                    .is_ok_and(|m| (1..=12).contains(&m));
            if !valid {
                bail!("month {month} isn't YYYY-MM");
            }
        }
        if !is_full_sha(&slices.code.sha) {
            bail!("the code slice's sha must be a full 40-character SHA");
        }
        Ok(slices)
    }
}

pub fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repository_slices_parse() {
        let text = include_str!("../../../config/test-slices.toml");
        let slices = Slices::parse(text).unwrap();
        assert_eq!(slices.mail.list, "pgsql-hackers");
        assert_eq!(slices.mail.months, ["2026-06", "2026-07"]);
        assert!(slices.mail.seed_threads.len() >= 10);
        assert_eq!(slices.code.paths.len(), 4);
    }
}
