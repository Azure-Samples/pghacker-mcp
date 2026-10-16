// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Patch series: which posted version of a patch set a patch file belongs
//! to, and its place in the set, from its name, its thread and its
//! message's subject.
//!
//! A version is kept as written, without the `v` that marks it: `3`, `3.1`,
//! `6a`, `01`, `20261003`, `12-pg13`, `pg18`. A part is a file's number in
//! its set: 1 for `v3-0001-Fix.patch`. The rules come from every patch name
//! in the archive (`examples/patch_series.rs` reports them over an export):
//! a form counts only where names use it reliably, and anything else gives
//! none.
//!
//! - A label before a part number, as `git format-patch -v` writes it:
//!   `v3-0001-…`, after a prefix (`nocfbot-v3-0001-…`), with any label of up
//!   to 24 characters (`v20220130-2-0001-…`, `v12-pg13-0001-…`,
//!   `vpg18-0001-…`), or with a two-digit part (`brin-inclusion-v07-01-…`).
//!   A longer label is a slug the part number ends, and its first token the
//!   version: 2 for `v2-Opclass-bitwise-equality-0001`.
//! - A part number, then `v1`–`v9` or a label with a leading zero:
//!   `0002-v4-feat-…`.
//! - A version first (`v0-backend-size`) or last (`fix-v3`, `fix_v3.1`,
//!   `fix.v1`, `fix-v6-heikki`: up to two words, numbers or a branch after
//!   it, only `wip`, `fix` and the like after a commit subject), or after the
//!   extension (`fix.patch.v2`), or the one version inside a name someone
//!   wrote (`brin-inclusion-v05-remove-assert-checking`). A decimal part has
//!   at most three digits, so a date after a version stays out of it (3 for
//!   `insert_on_dup.v3.2013_09_10`). Not a release current when the patch
//!   was posted (`v9.6` in 2016, `for-v13` in 2021), nor glued to a word
//!   (`uuidv7`).
//! - A date, first or last: `fix-20251114`, `0001-2021-11-17`,
//!   `20110218_fix`; a `yymmdd` last only within two weeks of the post.
//! - A `0.x` decimal last: `trgm_wildcard-0.4`.
//! - A bare number or `1.x`–`5.x` decimal last, only where the thread's
//!   posts of that name rise with it, one per post: `psql-dp-3` …
//!   `psql-dp-5`.
//! - Else the subject's `[PATCH v3 2/7]` tag, unless the message is a reply.
//!
//! A part comes from file names only: the part after a label, or a leading
//! number with a leading zero (`0001-…`, `07_readme`), or `0NNN` after up to
//! three words (`nocfbot-0001-…`, `REL_16_0002-…`).
//!
//! When the rules change, [`RULES`] goes up, and the indexer parses every
//! stored patch again once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use regex::{Captures, Regex};
use tokio_postgres::GenericClient;
use tokio_postgres::types::Type;

/// The rules' version. Bump it when the rules change: the leading indexer
/// then parses every stored patch again, once (the `patch_series` job, key
/// `rules-<N>`).
pub const RULES: u32 = 2;

/// A patch's series version and part.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Series {
    pub version: Option<String>,
    pub part: Option<i32>,
}

/// What a file name says on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameSeries {
    pub version: Option<String>,
    pub part: Option<i32>,
    /// The rule that gave the version: `label` (before a part), `part-label`
    /// (after a part), `first`, `last`, `middle`, `date` or `zero-decimal`;
    /// empty if none did.
    pub rule: &'static str,
    /// A number ending the name that is a version only if the thread's posts
    /// of the same name rise with it: the name before it, lowercased, and the
    /// number as written.
    pub sequence: Option<(String, String)>,
}

/// A patch of a thread, for [`thread_series`].
#[derive(Clone, Copy, Debug)]
pub struct ThreadPatch<'a> {
    /// Its message: patches of one message are one post.
    pub message: i64,
    pub sent_at: Option<DateTime<Utc>>,
    pub subject: &'a str,
    pub filename: &'a str,
}

/// Extensions that end patch names.
const EXTENSIONS: &[&str] = &[
    ".patch",
    ".diff",
    ".txt",
    ".nocfbot",
    ".no-cfbot",
    ".cfbotignorepatch",
    ".wip",
    ".patch_",
    ".patch-wip",
    ".orig",
    ".gz",
];
/// Labels without a digit that `git format-patch -v` is given.
const WORD_LABELS: &[&str] = &["final", "wip", "draft", "rc", "x", "xx", "xxx"];
/// The longest label taken whole before a part number. A longer one is a
/// slug the part number ends (`v2-Opclass-bitwise-equality-0001`): its
/// first token is the version.
const MAX_LABEL: usize = 24;
/// PostgreSQL's releases before 10 (and the development names 8.5 and 9.7)
/// with the year each came out. From 10 on, release N came out in 2007 + N.
const RELEASES: &[(&str, i32)] = &[
    ("6.0", 1997),
    ("6.1", 1997),
    ("6.2", 1997),
    ("6.3", 1998),
    ("6.4", 1998),
    ("6.5", 1999),
    ("7.0", 2000),
    ("7.1", 2001),
    ("7.2", 2002),
    ("7.3", 2002),
    ("7.4", 2003),
    ("8.0", 2005),
    ("8.1", 2005),
    ("8.2", 2006),
    ("8.3", 2008),
    ("8.4", 2009),
    ("8.5", 2010),
    ("9.0", 2010),
    ("9.1", 2011),
    ("9.2", 2012),
    ("9.3", 2013),
    ("9.4", 2014),
    ("9.5", 2016),
    ("9.6", 2016),
    ("9.7", 2017),
];

static INLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^inline-[0-9]+(?: \([0-9]+\))?$").unwrap());
static DUPLICATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" \([0-9]+\)$").unwrap());
/// `v<label>-<0NNN>-`, anywhere a separator precedes the `v`.
static LABEL_PART: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[-_.])v([0-9a-z](?:[0-9a-z._-]*?[0-9a-z])??)[-_.](0[0-9]{3})(?:[-_.]|$)")
        .unwrap()
});
/// `v<label>-<0NN or 0NNNN>-`, the label starting with a digit.
static LABEL_PART_35: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[-_.])v([0-9](?:[0-9a-z._-]*?[0-9a-z])??)[-_.](0[0-9]{2}|0[0-9]{4})(?:[-_.]|$)",
    )
    .unwrap()
});
/// `v<N>-<0N>-`: a simple label and a two-digit part.
static LABEL_PART_2: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[-_.])v([0-9][0-9a-z]*)[-_.](0[0-9])(?:[-_.]|$)").unwrap()
});
/// A label's first token: the version of a label that is a slug.
static LEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9][0-9a-z]*(?:\.[0-9]{1,3}(?:[a-z][0-9a-z]*)?)*").unwrap()
});
/// `v-<N>-<0NNN>-`: `-v` given a negative-looking number.
static LABEL_PART_DASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[-_.])v-([0-9]{1,3})[-_.](0[0-9]{3})(?:[-_.]|$)").unwrap()
});
/// `0NNN-v<label>-`: a part, then a version starting the commit subject.
static PART_LABEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(0[0-9]{2,3})[-_]v([0-9][0-9a-z]*)[-_]").unwrap());
/// `v<label>` starting the name.
static FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^v([0-9][0-9a-z]*(?:\.[0-9]{1,3}(?:[a-z][0-9a-z]*)?)*)(?:[-_. ]|$)").unwrap()
});
/// `v<label>` ending the name, with up to two words, numbers, dates or
/// branches after it.
static LAST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?:^|[-_. +]|[0-9])v([0-9][0-9a-z]*(?:\.[0-9]{1,3}(?:[a-z][0-9a-z]*)?)*)",
        r"((?:[-_. ](?:rel_?[0-9]+(?:_[0-9]+)?(?:_stable)?|pg_?[0-9]+",
        r"|(?:19|20)[0-9]{2}[-_.]?[0-9]{2}[-_.]?[0-9]{2}|[0-9]{1,2}",
        r"|[a-uw-z][a-z0-9]*|v[a-z][a-z0-9]*)){0,2})$",
    ))
    .unwrap()
});
/// What a version ending a commit subject may have after it.
static SUBJECT_QUALIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)^(?:[-_. ](?:(?:wip|fix|fixed|rebase|rebased|review|delta|draft|poc|rfc|tests?|alt)[0-9]*",
        r"|rel_?[0-9]+(?:_[0-9]+)?(?:_stable)?|pg_?[0-9]+",
        r"|(?:19|20)[0-9]{2}[-_.]?[0-9]{2}[-_.]?[0-9]{2}|[0-9]{1,2}))?$",
    ))
    .unwrap()
});
/// `v<label>` anywhere a separator, a digit or the start precedes it.
static V_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[-_. +]|[0-9])v([0-9][0-9a-z]*(?:\.[0-9]+[0-9a-z]*)*)").unwrap()
});
/// A decimal of four digits or more, which isn't a version's.
static LONG_DECIMAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[0-9]{4}").unwrap());
/// `fix.patch.v2`: a version after the extension.
static AFTER_EXTENSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(?:patch|diff)\.v([0-9][0-9a-z]*(?:\.[0-9]{1,3}(?:[a-z][0-9a-z]*)?)*)$")
        .unwrap()
});
/// Words after which `v<N>` may name a release: `since-v13`.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[-_. ])(?:for|to|from|and|since|until|till|thru|through|onto|into|against|after|before|vs|than|over|backpatch|backpatched|backport|backported)$",
    )
    .unwrap()
});
/// What after a `v<N>` makes it one end of a range: `v13-to-v9.6`.
static RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[-_. ](?:to|and|or|thru|through|until|till|vs|than)(?:[-_. ]|$)").unwrap()
});
static DATE_LAST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[-_. ])((?:19|20)[0-9]{2})([-_.]?)([0-9]{2})([-_.]?)([0-9]{2})$").unwrap()
});
static DATE_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^((?:19|20)[0-9]{2})([-_.]?)([0-9]{2})([-_.]?)([0-9]{2})(?:([-_])(0[0-9]{1,3}))?(?:[-_. ]|$)",
    )
    .unwrap()
});
static YYMMDD_LAST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[-_. ])([0-9]{2})([0-9]{2})([0-9]{2})$").unwrap());
static ZERO_DECIMAL_LAST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[-_ ])(0\.[0-9]{1,2}(?:\.[0-9]{1,2})?)$").unwrap());
static NUMBER_LAST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([^-_. ])([-_. ])([0-9]{1,2})$").unwrap());
static DECIMAL_LAST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[-_ ]([1-5]\.[0-9]{1,2}(?:\.[0-9]{1,2})?)$").unwrap());
static LEAD_PART: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(0[0-9]{1,4})([-_.])").unwrap());
static PREFIXED_PART: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:[a-z0-9.+]+[-_]){1,3}(0[0-9]{3})-[a-z]").unwrap());
/// A source file's name, as TAP tests are named: `080_pg_isready.pl`.
static SOURCE_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(?:pl|pm|c|h|sql|out|sgml|py|sh|y|l|xml|json|conf)$").unwrap()
});
static REPLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:re|aw|sv|antw|fwd?|fw)\s*(?:\[[0-9]+\])?\s*:").unwrap()
});
static SUBJECT_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[([^\]]*\bpatch(?:es)?\b[^\]]*)\]").unwrap());
static SUBJECT_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[\s,])v\.?([0-9][0-9a-z]*(?:\.[0-9]{1,3}(?:[a-z][0-9a-z]*)?)*)(?:$|[\s,])",
    )
    .unwrap()
});

/// What a patch file's name says: its version and part, and a number that
/// may be a version with its thread. `sent_at` dates releases and bounds
/// dates.
pub fn name_series(name: &str, sent_at: Option<DateTime<Utc>>) -> NameSeries {
    let mut out = NameSeries::default();
    if INLINE.is_match(name) {
        return out;
    }
    let name = name.trim();
    let name = DUPLICATE.find(name).map_or(name, |m| &name[..m.start()]);
    let stem = stem(name);
    let lead = lead_part(stem);
    // A label's part agrees with a leading part number, if any.
    let labelled = labelled(stem).filter(|(_, part)| lead.is_none_or(|lead| lead == *part));
    let (version, part, rule) = match labelled {
        Some((version, part)) => (Some(version), Some(part), "label"),
        None => match part_then_version(stem) {
            Some((version, part)) => (version, Some(part), "part-label"),
            None => (None, None, ""),
        },
    };
    out.part = lead.or(part).or_else(|| prefixed_part(stem));
    // The length of git's leading part number (`0001-`): a commit subject follows.
    let lead_len = lead
        .and_then(|_| LEAD_PART.find(stem))
        .filter(|m| m.as_str().len() == 5 && m.as_str().ends_with('-'))
        .map(|m| m.end());
    if let Some(version) = version {
        (out.version, out.rule) = (Some(version), rule);
    } else if let Some(version) = first(stem, sent_at) {
        (out.version, out.rule) = (Some(version), "first");
    } else if let Some(version) = after_extension(name, sent_at) {
        (out.version, out.rule) = (Some(version), "last");
    } else if let Some(version) = last(stem, sent_at, lead_len) {
        (out.version, out.rule) = (Some(version), "last");
    } else if let Some(version) = middle(stem, sent_at, lead_len) {
        (out.version, out.rule) = (Some(version), "middle");
    } else if let Some(version) = date(stem, sent_at, &mut out.part) {
        (out.version, out.rule) = (Some(version), "date");
    } else if let Some(version) = zero_decimal(stem) {
        (out.version, out.rule) = (Some(version), "zero-decimal");
    } else if versions(stem, sent_at).next().is_none() {
        out.sequence = sequence(stem);
    }
    out
}

/// The version a subject's `[PATCH v3 2/7]` tag gives, unless the message
/// replies to or forwards another.
pub fn subject_version(subject: &str) -> Option<String> {
    if REPLY.is_match(subject) {
        return None;
    }
    SUBJECT_TAG.captures_iter(subject).find_map(|tag| {
        let label = SUBJECT_VERSION.captures(&tag[1])?.get(1)?.as_str();
        (!release(label, None)).then(|| label.to_owned())
    })
}

/// The series of a thread's patches, in order: what each name says, then
/// numbers that rise across the posts of a name, then the subject's tag.
pub fn thread_series(patches: &[ThreadPatch<'_>]) -> Vec<Series> {
    let names: Vec<NameSeries> = patches
        .iter()
        .map(|p| name_series(p.filename, p.sent_at))
        .collect();
    let mut out: Vec<Series> = names
        .iter()
        .map(|n| Series {
            version: n.version.clone(),
            part: n.part,
        })
        .collect();
    let mut stems: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, name) in names.iter().enumerate() {
        if let Some((stem, _)) = &name.sequence {
            stems.entry(stem.as_str()).or_default().push(i);
        }
    }
    for indices in stems.values() {
        if rises(patches, &names, indices) {
            for &i in indices {
                out[i].version = names[i].sequence.as_ref().map(|(_, n)| n.clone());
            }
        }
    }
    let mut subjects: HashMap<i64, Option<String>> = HashMap::new();
    for (i, patch) in patches.iter().enumerate() {
        if out[i].version.is_none() {
            out[i].version = subjects
                .entry(patch.message)
                .or_insert_with(|| subject_version(patch.subject))
                .clone();
        }
    }
    out
}

/// What [`refresh`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Refreshed {
    pub threads: u64,
    pub patches: u64,
    /// Patches whose version or part it changed.
    pub changed: u64,
}

/// Recomputes the series of the patches of `threads`, updating those that
/// differ, unless another writer changed them meanwhile.
pub async fn refresh(client: &impl GenericClient, threads: &[i64]) -> Result<Refreshed> {
    let rows = client
        .query_typed(
            "SELECT m.thread_id, p.id, m.id, m.sent_at, m.subject, p.filename, p.series_version, p.part
             FROM patches p JOIN messages m ON m.id = p.message_id
             WHERE m.thread_id = ANY($1)
             ORDER BY m.thread_id, p.id",
            &[(&threads, Type::INT8_ARRAY)],
        )
        .await?;
    let mut by_thread: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        by_thread.entry(row.get(0)).or_default().push(i);
    }
    let mut ids = Vec::new();
    let (mut versions, mut parts) = (Vec::new(), Vec::new());
    let (mut old_versions, mut old_parts) = (Vec::new(), Vec::new());
    for indices in by_thread.values() {
        let subjects: Vec<String> = indices.iter().map(|&i| rows[i].get(4)).collect();
        let filenames: Vec<String> = indices.iter().map(|&i| rows[i].get(5)).collect();
        let patches: Vec<ThreadPatch<'_>> = indices
            .iter()
            .enumerate()
            .map(|(k, &i)| ThreadPatch {
                message: rows[i].get(2),
                sent_at: rows[i].get(3),
                subject: &subjects[k],
                filename: &filenames[k],
            })
            .collect();
        for (series, &i) in thread_series(&patches).into_iter().zip(indices) {
            let old_version: Option<String> = rows[i].get(6);
            let old_part: Option<i32> = rows[i].get(7);
            if (&series.version, series.part) != (&old_version, old_part) {
                ids.push(rows[i].get::<_, i64>(1));
                versions.push(series.version);
                parts.push(series.part);
                old_versions.push(old_version);
                old_parts.push(old_part);
            }
        }
    }
    let changed = if ids.is_empty() {
        0
    } else {
        client
            .execute_typed(
                "UPDATE patches p SET series_version = c.version, part = c.part
                 FROM unnest($1::int8[], $2::text[], $3::int4[], $4::text[], $5::int4[])
                      AS c (id, version, part, old_version, old_part)
                 WHERE p.id = c.id AND p.series_version IS NOT DISTINCT FROM c.old_version
                   AND p.part IS NOT DISTINCT FROM c.old_part",
                &[
                    (&ids, Type::INT8_ARRAY),
                    (&versions, Type::TEXT_ARRAY),
                    (&parts, Type::INT4_ARRAY),
                    (&old_versions, Type::TEXT_ARRAY),
                    (&old_parts, Type::INT4_ARRAY),
                ],
            )
            .await?
    };
    Ok(Refreshed {
        threads: by_thread.len() as u64,
        patches: rows.len() as u64,
        changed,
    })
}

/// A batch of [`reparse`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reparsed {
    pub refreshed: Refreshed,
    /// The last thread done: where the next batch starts.
    pub last_thread: Option<i64>,
    /// Threads with patches may remain after it.
    pub more: bool,
}

/// Refreshes the series of up to `limit` threads with patches after thread
/// `after`, in id order.
pub async fn reparse(client: &impl GenericClient, after: i64, limit: i64) -> Result<Reparsed> {
    let threads: Vec<i64> = client
        .query_typed(
            "SELECT DISTINCT thread_id FROM messages
             WHERE has_patch AND thread_id > $1 ORDER BY thread_id LIMIT $2",
            &[(&after, Type::INT8), (&limit, Type::INT8)],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let refreshed = refresh(client, &threads).await?;
    Ok(Reparsed {
        refreshed,
        last_thread: threads.last().copied(),
        more: threads.len() as i64 == limit,
    })
}

/// The name without its extensions, and without any description after an
/// inner extension (`fix-v14.diff.docs`).
fn stem(name: &str) -> &str {
    let mut stem = name;
    while let Some(ext) = EXTENSIONS.iter().find(|ext| {
        stem.len() > ext.len()
            && stem.as_bytes()[stem.len() - ext.len()..].eq_ignore_ascii_case(ext.as_bytes())
    }) {
        stem = &stem[..stem.len() - ext.len()];
    }
    let lower = stem.to_ascii_lowercase();
    if let Some(at) = [".patch.", ".diff."]
        .iter()
        .filter_map(|ext| lower.find(ext))
        .min()
        .filter(|&at| at > 0)
    {
        stem = &stem[..at];
    }
    stem.trim_end_matches(['-', '_', '.', ' '])
}

/// The label and part of `v<label>-<part>-`, the first that is one; of a
/// label longer than [`MAX_LABEL`], its first token.
fn labelled(stem: &str) -> Option<(String, i32)> {
    let take = |caps: Captures<'_>| -> Option<(String, i32)> {
        let label = caps.get(1)?.as_str();
        if !label_ok(label) {
            return None;
        }
        let label = if label.len() > MAX_LABEL {
            LEADING.find(label)?.as_str()
        } else {
            label
        };
        Some((label.to_owned(), caps[2].parse().ok()?))
    };
    LABEL_PART
        .captures_iter(stem)
        .find_map(take)
        .or_else(|| LABEL_PART_35.captures_iter(stem).find_map(take))
        .or_else(|| LABEL_PART_2.captures_iter(stem).find_map(take))
        .or_else(|| LABEL_PART_DASH.captures_iter(stem).find_map(take))
}

/// Whether a label before a part is one: starting with a digit, or a word
/// with a digit (`pg18`), or a word git users give (`final`); and not a
/// slug running into a later part number (`4-feat-add-x` in
/// `v4-feat-add-x-0015-…`).
fn label_ok(label: &str) -> bool {
    (label.starts_with(|c: char| c.is_ascii_digit())
        || label.bytes().any(|b| b.is_ascii_digit())
        || WORD_LABELS.contains(&label.to_ascii_lowercase().as_str()))
        && label.split('-').count() <= 4
}

/// `0NNN-v<label>-`: the part, and the label when it's `1`–`9` or has a
/// leading zero; a larger number there is often a release (`v15 docs`).
fn part_then_version(stem: &str) -> Option<(Option<String>, i32)> {
    let caps = PART_LABEL.captures(stem)?;
    let label = &caps[2];
    let digits: String = label.chars().take_while(char::is_ascii_digit).collect();
    let small = label.starts_with('0') || digits.parse::<u32>().is_ok_and(|n| n < 10);
    Some((small.then(|| label.to_owned()), caps[1].parse().ok()?))
}

/// `v<label>` starting the name, unless it's a release of the time.
fn first(stem: &str, sent_at: Option<DateTime<Utc>>) -> Option<String> {
    let label = FIRST.captures(stem)?.get(1)?.as_str();
    (!release(label, sent_at)).then(|| label.to_owned())
}

/// `v<label>` after the extension: `fix.patch.v2`.
fn after_extension(name: &str, sent_at: Option<DateTime<Utc>>) -> Option<String> {
    let label = AFTER_EXTENSION.captures(name)?.get(1)?.as_str();
    (!release(label, sent_at)).then(|| label.to_owned())
}

/// `v<label>` ending the name, before up to two words, numbers, dates or
/// branches; after a commit subject, before only `wip`, `fix`, a number, a
/// date or a branch. Not a release of the time.
fn last(stem: &str, sent_at: Option<DateTime<Utc>>, lead_len: Option<usize>) -> Option<String> {
    let caps = LAST.captures(stem)?;
    let label = caps.get(1)?.as_str();
    // Before the separator or digit the match starts with.
    let before = &stem[..caps.get(0)?.start()];
    if (after_subject(before, lead_len) && !SUBJECT_QUALIFIER.is_match(&caps[2]))
        || release(label, sent_at)
        || (REFERENCE.is_match(before) && numbered_release(label, sent_at))
    {
        return None;
    }
    Some(label.to_owned())
}

/// The one version inside a name someone wrote (not a commit subject, but
/// for its `[PATCH v3]`): `brin-inclusion-v05-remove-assert-checking`. None
/// if it has two.
fn middle(stem: &str, sent_at: Option<DateTime<Utc>>, lead_len: Option<usize>) -> Option<String> {
    let mut found = None;
    for (label, before, after) in versions(stem, sent_at) {
        // Right after the part number, it's the commit subject's first word;
        // before `-to-v9.6`, one end of a range.
        if lead_len.is_some_and(|len| before.len() < len) || RANGE.is_match(after) {
            continue;
        }
        // In a commit subject, only the version of `[PATCH v3 1/3]`:
        // `0001-PATCH-v3-1-3-…`.
        let after_patch = |len: usize| {
            before
                .get(len..)
                .is_some_and(|word| word.eq_ignore_ascii_case("patch"))
        };
        if lead_len.is_some_and(|len| !after_patch(len)) || found.is_some() {
            return None;
        }
        found = Some(label.to_owned());
    }
    found
}

/// The `v<label>`s of a name followed by a separator or its end, with what
/// precedes and follows each, but releases of the time.
fn versions(
    stem: &str,
    sent_at: Option<DateTime<Utc>>,
) -> impl Iterator<Item = (&str, &str, &str)> {
    V_TOKEN.captures_iter(stem).filter_map(move |caps| {
        let token = caps.get(1)?;
        // Up to a decimal of four digits: 3 of `v3.2013_09_10`.
        let len = LONG_DECIMAL
            .find(token.as_str())
            .map_or(token.len(), |m| m.start());
        let (label, after) = stem[token.start()..].split_at(len);
        let before = &stem[..caps.get(0)?.start()];
        let release = release(label, sent_at)
            || (REFERENCE.is_match(before) && numbered_release(label, sent_at));
        (after
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '-' | '_' | '.' | ' '))
            && !release)
            .then_some((label, before, after))
    })
}

/// Whether text before a version, in a name starting with a part number, is
/// a commit subject of more than one word (`0001-Fix-protocol-v3` but not
/// `0002-gtt-v52-regress`).
fn after_subject(before: &str, lead_len: Option<usize>) -> bool {
    lead_len.is_some_and(|len| {
        before
            .get(len..)
            .is_some_and(|subject| subject.contains(['-', '_', '.', ' ']))
    })
}

/// Whether `label` is a release, around when it was current: 6.x–9.x, or a
/// minor release of 10 or later (`12.8`).
fn release(label: &str, sent_at: Option<DateTime<Utc>>) -> bool {
    let mut numbers = label.split('.');
    let (Some(major), Some(minor)) = (numbers.next(), numbers.next()) else {
        return false;
    };
    if let Ok(major) = major.parse::<i32>()
        && major >= 10
        && minor.bytes().all(|b| b.is_ascii_digit())
    {
        return current(2007 + major, sent_at);
    }
    let key = format!("{major}.{minor}");
    RELEASES
        .iter()
        .find(|(release, _)| *release == key)
        .is_some_and(|(_, year)| current(*year, sent_at))
}

/// Whether `label` is release 10 or later, around when it was current.
fn numbered_release(label: &str, sent_at: Option<DateTime<Utc>>) -> bool {
    label
        .parse::<i32>()
        .is_ok_and(|n| n >= 10 && current(2007 + n, sent_at))
}

/// Whether a release of `year` was current at `sent_at`: in development from
/// two years before, supported for five years after. Undated: assume so.
fn current(year: i32, sent_at: Option<DateTime<Utc>>) -> bool {
    sent_at.is_none_or(|sent| (year - 2..=year + 5).contains(&sent.year()))
}

/// A date first or last, as written; with a part after a date first.
fn date(stem: &str, sent_at: Option<DateTime<Utc>>, part: &mut Option<i32>) -> Option<String> {
    let max_year = sent_at.unwrap_or_else(Utc::now).year() + 1;
    let valid = |caps: &Captures<'_>| {
        let (y, m, d) = (caps[1].parse(), caps[3].parse(), caps[5].parse());
        caps[2] == caps[4]
            && matches!((y, m, d), (Ok(y), Ok(m), Ok(d))
                if (1996..=max_year).contains(&y) && NaiveDate::from_ymd_opt(y, m, d).is_some())
    };
    if let Some(caps) = DATE_LAST.captures(stem).filter(valid) {
        return Some(
            caps.get(0)?
                .as_str()
                .trim_start_matches(['-', '_', '.', ' '])
                .to_owned(),
        );
    }
    if let Some(caps) = DATE_FIRST.captures(stem).filter(valid) {
        if part.is_none() {
            *part = caps.get(7).and_then(|p| p.as_str().parse().ok());
        }
        return Some(stem[..caps.get(5)?.end()].to_owned());
    }
    let sent = sent_at?.date_naive();
    let caps = YYMMDD_LAST.captures(stem)?;
    let (yy, m, d): (i32, u32, u32) = (
        caps[1].parse().ok()?,
        caps[2].parse().ok()?,
        caps[3].parse().ok()?,
    );
    let y = if yy < 90 { 2000 + yy } else { 1900 + yy };
    let days = (NaiveDate::from_ymd_opt(y, m, d)? - sent).num_days();
    (-14..=3)
        .contains(&days)
        .then(|| format!("{}{}{}", &caps[1], &caps[2], &caps[3]))
}

/// A `0.x` decimal ending the name, unless it numbers a part (`part-0.2`).
fn zero_decimal(stem: &str) -> Option<String> {
    let number = ZERO_DECIMAL_LAST.captures(stem)?.get(1)?;
    let before = stem[..number.start()].trim_end_matches(['-', '_', ' ']);
    (!before.to_ascii_lowercase().ends_with("part")).then(|| number.as_str().to_owned())
}

/// The part leading the name: `0001-…`, `07_readme`, `00001_…`; not a TAP
/// test's number (`080_pg_isready.pl`).
fn lead_part(stem: &str) -> Option<i32> {
    let caps = LEAD_PART.captures(stem)?;
    let digits = &caps[1];
    let ok = digits.len() == 4 || (&caps[2] != "." && !SOURCE_FILE.is_match(stem));
    ok.then(|| digits.parse().ok()).flatten()
}

/// `0NNN` after up to three words: `nocfbot-0001-…`, `REL_16_0002-…`.
fn prefixed_part(stem: &str) -> Option<i32> {
    PREFIXED_PART.captures(stem)?[1].parse().ok()
}

/// A bare number or a `1.x`–`5.x` decimal ending the name: the name before
/// it, lowercased, and the number.
fn sequence(stem: &str) -> Option<(String, String)> {
    if let Some(caps) = DECIMAL_LAST.captures(stem) {
        let number = caps.get(1)?;
        let before = &stem[..number.start() - 1];
        return Some((before.to_ascii_lowercase(), number.as_str().to_owned()));
    }
    let caps = NUMBER_LAST.captures(stem)?;
    let (prev, sep, number) = (caps.get(1)?, &caps[2], caps.get(3)?);
    // `9.4.1`: part of a dotted number.
    if sep == "." && prev.as_str().starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((
        stem[..prev.end()].to_ascii_lowercase(),
        number.as_str().to_owned(),
    ))
}

/// A post: when it was sent, and its message.
type Post = (Option<DateTime<Utc>>, i64);

/// Whether the posts of a name rise with its number: two or more posts, one
/// number each, each larger than the post before it.
fn rises(patches: &[ThreadPatch<'_>], names: &[NameSeries], indices: &[usize]) -> bool {
    // By post, in the order they were sent: the numbers it has.
    let mut posts: BTreeMap<Post, BTreeSet<Vec<u32>>> = BTreeMap::new();
    for &i in indices {
        let Some((_, number)) = &names[i].sequence else {
            return false;
        };
        let Some(value) = number
            .split('.')
            .map(|n| n.parse().ok())
            .collect::<Option<Vec<u32>>>()
        else {
            return false;
        };
        posts
            .entry((patches[i].sent_at, patches[i].message))
            .or_default()
            .insert(value);
    }
    if posts.len() < 2 {
        return false;
    }
    let mut previous: Option<&Vec<u32>> = None;
    for values in posts.values() {
        let mut values = values.iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return false;
        };
        if previous.is_some_and(|previous| value <= previous) {
            return false;
        }
        previous = Some(value);
    }
    true
}

#[cfg(test)]
mod tests;
