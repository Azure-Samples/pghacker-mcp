//! Code snapshots: which of several is the latest, as the code tools and the
//! ontology both choose it. Every loaded snapshot is kept; the latest is only
//! the default.

use std::cmp::Ordering;
use std::sync::LazyLock;

/// Names the schema of a history build to read code from instead of the
/// snapshots, such as `pghacker_history`, or `pghacker` once installed.
pub const HISTORY_ENV: &str = "PGHACKER_CODE_HISTORY";

/// The history schema the flag names, if any: the code tools then read the
/// commit history, and anchors resolve to its virtual paths.
pub fn history_schema() -> Option<&'static str> {
    static SCHEMA: LazyLock<Option<String>> = LazyLock::new(|| {
        std::env::var(HISTORY_ENV)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    });
    SCHEMA.as_deref()
}

/// `ORDER BY` for snapshots, latest first: the newest commit, so a release
/// branch's head comes before its releases, and a later release before an
/// earlier one.
pub const LATEST_FIRST: &str = "committed_at DESC NULLS LAST, loaded_at DESC NULLS LAST, id DESC";

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
