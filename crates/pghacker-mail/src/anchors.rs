//! Anchors: what a message mentions that exists outside the archive. Paths
//! in the PostgreSQL tree, commits, CommitFest entries and major releases,
//! from the subject, the cleaned body and the patches' paths. Precision over
//! recall: only clear forms count.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use chrono::{DateTime, Datelike, Utc};
use regex::Regex;

use crate::patch::{PatchFile, is_tree_path, subject_series};

/// A mention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorRef {
    /// `file`, `commit`, `commitfest` or `release`.
    pub kind: &'static str,
    /// A tree path, an abbreviated or full lowercase SHA, a CommitFest entry
    /// ID, or a major version such as `19` or `9.6`.
    pub value: String,
    /// `subject`, `body` or `patch`.
    pub origin: &'static str,
}

/// Directories under `src/` in the tree.
const SRC_DIRS: &[&str] = &[
    "backend",
    "bin",
    "common",
    "fe_utils",
    "include",
    "interfaces",
    "makefiles",
    "pl",
    "port",
    "template",
    "test",
    "timezone",
    "tools",
    "tutorial",
];

static PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s(\[{<'`=:,;*|>])((?:src|contrib|doc|config)/[A-Za-z0-9_.+\-]+(?:/[A-Za-z0-9_.+\-]+)*/?)",
    )
    .unwrap()
});
static TREE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:github\.com/postgres/postgres/(?:blob|tree|raw)/[^/\s]+/|git\.postgresql\.org/\S*?[;?&]f=)((?:src|contrib|doc|config)/[A-Za-z0-9_+\-]+(?:[./][A-Za-z0-9_+\-]+)*)",
    )
    .unwrap()
});
static COMMIT_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:git\.postgresql\.org/\S*?[;?&]h=|git\.postgresql\.org/(?:pg|gitweb)/(?:commit|commitdiff)/|github\.com/postgres/postgres/commit/|postgr\.es/c/)([0-9a-f]{7,40})\b",
    )
    .unwrap()
});
static COMMIT_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:commits?|committed\s+(?:as|in)|revert(?:s|ed|ing)?(?:\s+commit)?|cherry-pick(?:ed)?|git\s+(?:show|revert|log|checkout|cherry-pick))\s*:?\s+([0-9a-f]{7,40})\b",
    )
    .unwrap()
});
static COMMIT_MORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:,|\band\b|&|/|\+|,\s*and\b)\s*(?:commit\s+)?([0-9a-f]{7,40})\b").unwrap()
});
static COMMITFEST_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)commitfest\.postgresql\.org/(?:\d+/(\d+)|patch/(\d+))\b").unwrap()
});
static COMMITFEST_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:CF|commitfest)\s+(?:entry|item|patch)\s*#?\s*(\d{2,6})\b").unwrap()
});
/// `PG19`, `PG 9.6`, `PostgreSQL 18`, `Postgres version 17`, `release 19`.
static RELEASE_NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\bPG\s?v?|(?i:\bpostgres(?:ql)?\s+(?:version\s+)?|\breleases?\s+))(9\.\d|[1-9]\d)(?:\.\d+)?(?:beta\d*|rc\d*|devel)?\b",
    )
    .unwrap()
});
/// Branch and tag names: `REL_17_STABLE`, `REL9_6_STABLE`, `REL_18_1`.
static RELEASE_BRANCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bREL_?(?:(9)_(\d)|([1-9]\d))_(?:STABLE|BETA\d*|RC\d*|\d+)\b").unwrap()
});
/// `v17`, `v9.6`: checked against patch-version contexts.
static RELEASE_V: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^A-Za-z0-9_\-./])v(9\.\d|[1-9]\d)").unwrap());
/// What makes a bare `vNN` a release rather than a patch version: the word
/// before it ("back-patched to v17", "since v15", "introduced in v18") ...
static V_RELEASE_BEFORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:\b(?:to|through|thru|since|from|before|after|until|till|for|on|against|with|and|or|than|vs\.?|versus|pre|post|like|both|across|into|onto|between|of|in|at\s+least|as\s+of|branch|branches)|\bv\d{2}\s*,)\s*$",
    )
    .unwrap()
});
/// ... or what follows it: "v17+", "v16 and later", "the v17 branch", "v18 only".
static V_RELEASE_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:\+|~|-(?:\s|$|[.,;)])|\s+(?:and|or)\s+(?:later|earlier|newer|older|above|below|up|higher|lower|beyond|master|head)\b|\s+(?:branch|branches|only|onwards?|release|releases|cycle|development|devel|stable|back-?branch(?:es)?)\b|\s*(?:,|and|or|/|&)\s*(?:v|PG\s?)\d)",
    )
    .unwrap()
});
/// Patch-version contexts, which veto: "v17 of the patch", "attached v17".
static V_AFTER_PATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:of\s+(?:the|this|my|your|his|her|our|their|that)\s+)?(?:patch|patches|patchset|patch\s+set|series|version|set|attached|posted|rebased|is\s+attached)\b",
    )
    .unwrap()
});
static V_BEFORE_PATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:(?:attached|attach|attaching|rebased|posted|updated|new|here\s+is|here's|heres|sending|sent|submit|submitted|apply|applying|applied|review|reviewed|reviewing|tested|testing|thanks\s+for|patch|patches|\[)\s*(?:is|are|was)?\s*(?:the|a|an|my|your)?|(?:patch|patches|patchset|version|versions|comments?|feedback|reviews?|look|looking|looked)\s+(?:of|from|for|on|at|in))\s*$",
    )
    .unwrap()
});
static PATCH_NAME_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[^a-z0-9])v(\d{1,3})[a-z]?-\d{4}").unwrap());
/// "In v13, I added ...": a sentence that starts with "In vNN," describes a
/// patch version.
static V_SENTENCE_IN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[.!?:]\s+)In\s+$").unwrap());

/// The anchors of a message, deduplicated: subject first, then body, then
/// patch paths. Release mentions are bounded by the current date.
pub fn extract(subject: &str, body_clean: &str, patches: &[PatchFile]) -> Vec<AnchorRef> {
    extract_at(subject, body_clean, patches, None)
}

/// Like [`extract`], with release mentions bounded by what existed when the
/// message was sent: `v25` in 2026 is a patch version, not a release.
pub fn extract_at(
    subject: &str,
    body_clean: &str,
    patches: &[PatchFile],
    sent_at: Option<DateTime<Utc>>,
) -> Vec<AnchorRef> {
    let year = sent_at.unwrap_or_else(Utc::now).year();
    // PostgreSQL N+1 is in development from mid N's year: PG20 in 2026.
    let development = year - 2006;
    let patch_versions = patch_versions(subject, body_clean, patches);
    let mut seen = HashSet::new();
    let mut anchors = Vec::new();
    let mut push = |kind: &'static str, value: String, origin: &'static str| {
        if seen.insert((kind, value.clone())) {
            anchors.push(AnchorRef {
                kind,
                value,
                origin,
            });
        }
    };
    for (origin, text) in [("subject", subject), ("body", body_clean)] {
        for path in files(text) {
            push("file", path, origin);
        }
        for sha in commits(text) {
            push("commit", sha, origin);
        }
        for entry in commitfest_entries(text) {
            push("commitfest", entry, origin);
        }
        for release in releases(text, development, &patch_versions) {
            push("release", release, origin);
        }
    }
    for patch in patches {
        for path in &patch.paths {
            if is_tree_path(path) {
                push("file", path.clone(), "patch");
            }
        }
    }
    anchors
}

/// Paths in the PostgreSQL tree.
pub fn files(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for caps in TREE_URL.captures_iter(text) {
        if let Some(path) = tree_path(&caps[1]) {
            out.push(path);
        }
    }
    for caps in PATH.captures_iter(text) {
        if let Some(path) = tree_path(&caps[1]) {
            out.push(path);
        }
    }
    out
}

fn tree_path(raw: &str) -> Option<String> {
    let path = raw.trim_end_matches(['.', '/', '-', '+']);
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 2
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return None;
    }
    let second = parts[1];
    let is_file = |name: &str| name.contains('.') || name.starts_with("Makefile");
    let valid = match parts[0] {
        "src" => SRC_DIRS.contains(&second) || (parts.len() == 2 && is_file(second)),
        "contrib" => second
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
        "doc" => {
            second == "src"
                || (parts.len() == 2
                    && (is_file(second)
                        || second.chars().all(|c| c.is_ascii_uppercase() || c == '_')))
        }
        "config" => {
            parts.len() == 2
                && ([".m4", ".guess", ".sub", ".pl", ".sh", ".build"]
                    .iter()
                    .any(|ext| second.ends_with(ext))
                    || matches!(second, "install-sh" | "missing" | "prep_buildtree"))
        }
        _ => false,
    };
    valid.then(|| path.to_owned())
}

/// Commit SHAs in commit URLs and in phrases such as "commit 1a2b3c4".
pub fn commits(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for caps in COMMIT_URL.captures_iter(text) {
        out.push(caps[1].to_lowercase());
    }
    for caps in COMMIT_TEXT.captures_iter(text) {
        let first = caps.get(1).expect("a SHA");
        if !is_sha(first.as_str()) {
            continue;
        }
        out.push(first.as_str().to_owned());
        let mut end = first.end();
        while let Some(more) = COMMIT_MORE.captures(&text[end..]) {
            let sha = more.get(1).expect("a SHA");
            if !is_sha(sha.as_str()) {
                break;
            }
            out.push(sha.as_str().to_owned());
            end += sha.end();
        }
    }
    out
}

/// Lowercase hex with a digit and a letter: not a number or a word.
fn is_sha(text: &str) -> bool {
    (7..=40).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && text.bytes().any(|b| b.is_ascii_digit())
        && text.bytes().any(|b| b.is_ascii_alphabetic())
}

/// CommitFest entry IDs.
pub fn commitfest_entries(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for caps in COMMITFEST_URL.captures_iter(text) {
        if let Some(entry) = caps.get(1).or_else(|| caps.get(2)) {
            out.push(entry.as_str().trim_start_matches('0').to_owned());
        }
    }
    for caps in COMMITFEST_TEXT.captures_iter(text) {
        out.push(caps[1].trim_start_matches('0').to_owned());
    }
    out.retain(|entry| !entry.is_empty());
    out
}

/// The patch versions a message itself shows: its patches' series, `vNN-0001`
/// names and the subject's `[PATCH vNN]`. A bare `vNN` among them is a patch
/// version, not a release.
fn patch_versions(subject: &str, body: &str, patches: &[PatchFile]) -> BTreeSet<i32> {
    let mut versions: BTreeSet<i32> = patches.iter().filter_map(|p| p.series_version).collect();
    versions.extend(subject_series(subject).0);
    for caps in PATCH_NAME_VERSION.captures_iter(body) {
        if let Ok(version) = caps[1].parse() {
            versions.insert(version);
        }
    }
    versions
}

/// Major releases, at most `development + 2` when named, and `development`
/// as a bare `vNN` that isn't one of `patch_versions`.
pub fn releases(text: &str, development: i32, patch_versions: &BTreeSet<i32>) -> Vec<String> {
    let mut out = Vec::new();
    for caps in RELEASE_NAMED.captures_iter(text) {
        if let Some(release) = major(&caps[1], development + 2) {
            out.push(release);
        }
    }
    for caps in RELEASE_BRANCH.captures_iter(text) {
        let release = match (caps.get(1), caps.get(2), caps.get(3)) {
            (Some(_), Some(minor), _) => format!("9.{}", minor.as_str()),
            (_, _, Some(major)) => major.as_str().to_owned(),
            _ => continue,
        };
        if let Some(release) = major(&release, development + 2) {
            out.push(release);
        }
    }
    for caps in RELEASE_V.captures_iter(text) {
        let number = caps.get(1).expect("a number");
        let after = &text[number.end()..];
        let line_start = text[..number.start()].rfind('\n').map_or(0, |i| i + 1);
        let before = &text[line_start..number.start() - 1];
        let after_line = after.split('\n').next().unwrap_or("");
        let is_patch_version = number
            .as_str()
            .parse::<i32>()
            .is_ok_and(|n| patch_versions.contains(&n));
        if !is_patch_version
            && is_release_v(before, after_line)
            && let Some(release) = major(number.as_str(), development)
        {
            out.push(release);
        }
    }
    out
}

/// Whether `vNN`, between `before` and `after` on its line, names a release.
fn is_release_v(before: &str, after: &str) -> bool {
    let next = after.chars().next();
    let minor = after.starts_with('.') && after[1..].starts_with(|c: char| c.is_ascii_digit());
    if !minor
        && (matches!(next, Some(c) if c == '_' || c.is_ascii_alphanumeric())
            || (next == Some('-') && after[1..].starts_with(|c: char| c.is_ascii_alphanumeric()))
            || after.starts_with(".patch")
            || after.starts_with(".diff")
            || after.starts_with(".txt"))
    {
        return false;
    }
    if V_AFTER_PATCH.is_match(after) || V_BEFORE_PATCH.is_match(before) {
        return false;
    }
    if V_SENTENCE_IN.is_match(before) && after.starts_with(',') {
        return false;
    }
    minor || V_RELEASE_BEFORE.is_match(before) || V_RELEASE_AFTER.is_match(after)
}

/// A major version within bounds: 9.0–9.6, or 10 up to `max`.
fn major(text: &str, max: i32) -> Option<String> {
    if let Some(minor) = text.strip_prefix("9.") {
        let minor: i32 = minor.parse().ok()?;
        return ((0..=6).contains(&minor) && max >= 4).then(|| format!("9.{minor}"));
    }
    let major: i32 = text.parse().ok()?;
    (10..=max).contains(&major).then(|| major.to_string())
}

#[cfg(test)]
mod tests;
