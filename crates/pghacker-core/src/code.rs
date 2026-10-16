// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The commit history that the code tools read, and how versions sort.

use std::cmp::Ordering;
use std::sync::LazyLock;

/// Overrides the schema of the commit history the code tools read, such as
/// a build's own `pghacker_history` before it is installed.
pub const HISTORY_ENV: &str = "PGHACKER_CODE_HISTORY";

/// Where an install puts the commit history.
pub const DEFAULT_HISTORY_SCHEMA: &str = "pghacker";

/// The schema of the commit history that the code tools read and anchors
/// resolve to: [`HISTORY_ENV`]'s, else [`DEFAULT_HISTORY_SCHEMA`].
pub fn history_schema() -> &'static str {
    static SCHEMA: LazyLock<String> = LazyLock::new(|| {
        std::env::var(HISTORY_ENV)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_HISTORY_SCHEMA.to_owned())
    });
    &SCHEMA
}

/// A numbered release line, such as 19, or 9 for 9.0 to 9.6; None for names
/// such as master.
pub fn release_line(version: &str) -> Option<u32> {
    version.trim().parse().ok()
}

/// Versions newest first: release lines by number, then names such as master.
pub fn newest_first(a: &str, b: &str) -> Ordering {
    match (release_line(a), release_line(b)) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_newest_release_line_first_then_names() {
        let mut versions = vec!["9", "master", "19", "6", "10", "18"];
        versions.sort_by(|a, b| newest_first(a, b));
        assert_eq!(versions, ["19", "18", "10", "9", "6", "master"]);
        assert_eq!(release_line(" 9 "), Some(9));
        assert_eq!(release_line("master"), None);
    }
}
