// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The test slices (`config/test-slices.toml`): which months and threads of a
//! mailing list a source retrieves under `PGHACKER_SOURCES=test-slices`, the
//! default; `open` takes any months.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Slices {
    pub mail: MailSlice,
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
        Ok(slices)
    }
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
    }
}
