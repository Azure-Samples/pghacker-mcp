// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! SQL helpers the commit and patch searches share: numbered parameters,
//! path filters (paths, folders or ripgrep-style globs) and dates.

use anyhow::{Result, anyhow};
use chrono::{DateTime, NaiveDate, Utc};
use tokio_postgres::types::ToSql;

use crate::pattern::are::escape_like;

/// Parameters numbered as conditions use them.
#[derive(Default)]
pub(crate) struct Params(Vec<Box<dyn ToSql + Sync + Send>>);

impl Params {
    pub(crate) fn add(&mut self, value: impl ToSql + Sync + Send + 'static) -> String {
        self.0.push(Box::new(value));
        format!("${}", self.0.len())
    }

    pub(crate) fn refs(&self) -> Vec<&(dyn ToSql + Sync)> {
        self.0
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect()
    }
}

/// A ripgrep-style glob as a PostgreSQL regular expression on whole paths:
/// without a slash it matches a name at any depth; a match also covers
/// everything below it.
pub(crate) fn glob_regex(glob: &str) -> String {
    let g = glob.trim_start_matches('/');
    let mut re = String::from(if glob.contains('/') { "^" } else { "(^|/)" });
    let chars: Vec<char> = g.chars().collect();
    let mut braces = 0;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => {
                let mut j = i + 1;
                re.push('[');
                if matches!(chars.get(j), Some('!' | '^')) {
                    re.push('^');
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    if chars[j] == '\\' {
                        re.push('\\');
                    }
                    re.push(chars[j]);
                    j += 1;
                }
                re.push(']');
                i = j + 1;
                continue;
            }
            '{' => {
                braces += 1;
                re.push('(');
            }
            '}' if braces > 0 => {
                braces -= 1;
                re.push(')');
            }
            ',' if braces > 0 => re.push('|'),
            c if "\\.+()|^$}".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
        i += 1;
    }
    re.push_str("($|/)");
    re
}

/// SQL that `col` (a path column) is at or below a path, or matches a glob.
pub(crate) fn path_sql(paths: &[String], col: &str, params: &mut Params) -> Option<String> {
    let mut parts = Vec::new();
    for raw in paths.iter().map(|p| p.trim().trim_start_matches('/')) {
        if raw.is_empty() {
            continue;
        }
        if raw.contains(['*', '?', '[', '{']) {
            let p = params.add(glob_regex(raw));
            parts.push(format!("{col} ~ {p}"));
        } else {
            let raw = raw.trim_end_matches('/');
            let exact = params.add(raw.to_owned());
            let below = params.add(format!("{}/%", escape_like(raw)));
            parts.push(format!("({col} = {exact} OR {col} LIKE {below})"));
        }
    }
    (!parts.is_empty()).then(|| format!("({})", parts.join(" OR ")))
}

/// A day or a time; a day's end with `end`.
pub(crate) fn when(raw: &str, end: bool) -> Result<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Ok(t.with_timezone(&Utc));
    }
    let day = NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| anyhow!("{raw:?} is not a date (YYYY-MM-DD) or an RFC 3339 time"))?;
    let day = if end {
        day.succ_opt().unwrap_or(day)
    } else {
        day
    };
    Ok(day.and_hms_opt(0, 0, 0).expect("midnight").and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_become_path_regexes() {
        let re = |g: &str| regex::Regex::new(&glob_regex(g)).unwrap();
        assert!(re("*.y").is_match("src/backend/parser/gram.y"));
        assert!(!re("*.y").is_match("src/backend/parser/gram.yy"));
        assert!(re("src/*/parser").is_match("src/backend/parser/scan.l"));
        assert!(!re("src/*/parser").is_match("src/a/b/parser/x"));
        assert!(re("src/**/*.{c,h}").is_match("src/backend/a.c"));
        assert!(re("src/**/*.{c,h}").is_match("src/x.h"));
        assert!(re("[gs]ram.y").is_match("a/gram.y"));
    }
}
