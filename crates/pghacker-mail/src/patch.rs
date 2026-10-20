// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Patches: attachments that are patches (`.patch` and `.diff` files, patch
//! content types, and diffs under other names such as `.txt` or `.nocfbot`,
//! gzip'd or in tar archives) and unified or context diffs pasted into the
//! body. Each with its series version and part ([`crate::series`]), the paths
//! it touches, and its added and removed lines.

use std::collections::HashSet;
use std::io::Read;
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use crate::parse::{Attachment, ParsedMessage};
use crate::series::{ThreadPatch, thread_series};

/// A patch in a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatchFile {
    /// The attachment's name, or `inline-<n>` for a diff in the body.
    pub filename: String,
    /// The posted version it belongs to, as written: `3` for
    /// `v3-0001-foo.patch` ([`crate::series`]).
    pub series_version: Option<String>,
    /// Its number in the set: 1 for `v3-0001-foo.patch`.
    pub part: Option<i32>,
    /// The files it touches, as paths in the tree, in order.
    pub paths: Vec<String>,
    pub added: i32,
    pub removed: i32,
    pub content: String,
}

/// Decompressed attachments larger than this are skipped.
const MAX_DECOMPRESSED: usize = 64 << 20;

static HUNK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@+ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap());
static CONTEXT_RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\*\*\*|---) \d+(?:,\d+)? (?:\*\*\*\*|----)\s*$").unwrap());

/// The patches of a message: its patch attachments, then the diffs pasted
/// into its body outside quotes, each with the series its name and the
/// message's subject give.
pub fn extract(msg: &ParsedMessage) -> Vec<PatchFile> {
    let mut patches = Vec::new();
    for attachment in &msg.attachments {
        patches.extend(from_attachment(attachment));
    }
    let lines: Vec<&str> = msg.body.lines().collect();
    let masked: Vec<&str> = lines
        .iter()
        .map(|line| if is_quoted(line) { ">" } else { line })
        .collect();
    let mut inline = 0;
    for block in diff_blocks(&masked) {
        let content = lines[block].join("\n") + "\n";
        let stats = diff_stats(&content);
        if stats.paths.is_empty() {
            continue;
        }
        inline += 1;
        patches.push(PatchFile {
            filename: format!("inline-{inline}"),
            paths: stats.paths,
            added: stats.added,
            removed: stats.removed,
            content,
            ..Default::default()
        });
    }
    let mut names = HashSet::new();
    for patch in &mut patches {
        let mut name = patch.filename.clone();
        let mut n = 2;
        while !names.insert(name.clone()) {
            name = format!("{} ({n})", patch.filename);
            n += 1;
        }
        patch.filename = name;
    }
    let posted: Vec<ThreadPatch<'_>> = patches
        .iter()
        .map(|patch| ThreadPatch {
            message: 0,
            sent_at: msg.sent_at,
            subject: &msg.subject,
            filename: &patch.filename,
        })
        .collect();
    let series = thread_series(&posted);
    for (patch, series) in patches.iter_mut().zip(series) {
        (patch.series_version, patch.part) = (series.version, series.part);
    }
    patches
}

/// Whether a body line is quoted.
pub(crate) fn is_quoted(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    line.len() - trimmed.len() <= 3 && trimmed.starts_with('>')
}

fn from_attachment(attachment: &Attachment) -> Vec<PatchFile> {
    let name = attachment.filename.clone().unwrap_or_default();
    // ASCII lowercasing keeps byte offsets, so a suffix found in `lower`
    // ends `name` at a char boundary; full Unicode lowercasing doesn't
    // ('İ' becomes 3 bytes).
    let lower = name.to_ascii_lowercase();
    let gzip = attachment.data.starts_with(&[0x1f, 0x8b]);
    if gzip {
        let Some(inner) = gunzip(&attachment.data) else {
            return Vec::new();
        };
        if is_tar(&inner) {
            return from_tar(&inner);
        }
        let inner_name = if lower.ends_with(".tgz") {
            format!("{}.tar", &name[..name.len() - 4])
        } else if lower.ends_with(".gz") {
            name[..name.len() - 3].to_owned()
        } else {
            name.clone()
        };
        return candidate(&inner_name, "", text_of(&inner))
            .into_iter()
            .collect();
    }
    if is_tar(&attachment.data) {
        return from_tar(&attachment.data);
    }
    candidate(&name, &attachment.content_type, attachment.text())
        .into_iter()
        .collect()
}

fn from_tar(data: &[u8]) -> Vec<PatchFile> {
    tar_entries(data)
        .into_iter()
        .filter_map(|(name, bytes)| {
            let base = name.rsplit('/').next().unwrap_or(&name).to_owned();
            candidate(&base, "", text_of(bytes))
        })
        .collect()
}

/// A patch if the name or content type says so, or if the content is a diff
/// under a name that doesn't say otherwise.
fn candidate(name: &str, content_type: &str, text: Option<String>) -> Option<PatchFile> {
    let text = text?;
    let lower = name.to_ascii_lowercase();
    let named = lower.ends_with(".patch") || lower.ends_with(".diff");
    let typed = matches!(
        content_type,
        "text/x-patch"
            | "text/x-diff"
            | "application/x-patch"
            | "application/x-diff"
            | "text/x-diff-patch"
    );
    let not_a_patch = [
        ".diffs", ".out", ".log", ".sql", ".c", ".h", ".pl", ".py", ".sh", ".html", ".csv",
        ".json", ".md",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext));
    if !(named || typed || (!not_a_patch && looks_like_diff(&text))) {
        return None;
    }
    let stats = diff_stats(&text);
    Some(PatchFile {
        filename: if name.is_empty() {
            "attachment.patch".to_owned()
        } else {
            name.to_owned()
        },
        paths: stats.paths,
        added: stats.added,
        removed: stats.removed,
        content: text,
        ..Default::default()
    })
}

fn text_of(bytes: &[u8]) -> Option<String> {
    Attachment {
        data: bytes.to_vec(),
        ..Default::default()
    }
    .text()
}

/// Whether text contains a unified or context diff.
pub fn looks_like_diff(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().any(|l| l.starts_with("diff --git "))
        || lines.windows(3).any(|w| {
            (w[0].starts_with("--- ") && w[1].starts_with("+++ ") && w[2].starts_with("@@"))
                || (w[0].starts_with("*** ")
                    && w[1].starts_with("--- ")
                    && w[2].starts_with("***************"))
        })
}

/// What a diff touches.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub paths: Vec<String>,
    pub added: i32,
    pub removed: i32,
    pub hunks: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Old,
    New,
}

/// What a line of a patch is: a hunk's added, removed or context line, or
/// any other line (headers, hunk ranges, text around the diff).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    Added,
    Removed,
    Context,
    Other,
}

impl LineKind {
    /// The diff prefix the kind stands for; `h` for other lines.
    pub fn code(self) -> char {
        match self {
            LineKind::Added => '+',
            LineKind::Removed => '-',
            LineKind::Context => ' ',
            LineKind::Other => 'h',
        }
    }
}

/// A line of a patch: a hunk line without its prefix, or another line whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatchLine<'a> {
    pub kind: LineKind,
    pub text: &'a str,
    /// The file it is in, an index into [`DiffStats::paths`].
    pub file: Option<usize>,
}

/// The paths, added and removed lines and hunks of a diff, which may hold
/// several files, git format-patch mails and context diffs.
pub fn diff_stats(text: &str) -> DiffStats {
    parse_diff(text).0
}

/// A diff's stats and its lines, classified: unified and context diffs,
/// several files, and git format-patch mails.
pub fn parse_diff(text: &str) -> (DiffStats, Vec<PatchLine<'_>>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<PatchLine<'_>> = Vec::with_capacity(lines.len());
    let mut stats = DiffStats::default();
    let mut file: Option<usize> = None;
    let add_path = |path: Option<String>, stats: &mut DiffStats, file: &mut Option<usize>| {
        if let Some(path) = path {
            let index = match stats.paths.iter().position(|p| *p == path) {
                Some(index) => index,
                None => {
                    stats.paths.push(path);
                    stats.paths.len() - 1
                }
            };
            *file = Some(index);
        }
    };
    let mut hunk: Option<(i64, i64)> = None;
    let mut section: Option<Section> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let mut push = |kind: LineKind, text, file: Option<usize>| {
            out.push(PatchLine { kind, text, file });
        };
        if let Some((old, new)) = &mut hunk {
            if *old > 0 || *new > 0 {
                let kind = match line.as_bytes().first() {
                    Some(b'+') => {
                        stats.added += 1;
                        *new -= 1;
                        Some(LineKind::Added)
                    }
                    Some(b'-') => {
                        stats.removed += 1;
                        *old -= 1;
                        Some(LineKind::Removed)
                    }
                    Some(b' ') | None => {
                        *old -= 1;
                        *new -= 1;
                        Some(LineKind::Context)
                    }
                    Some(b'\\') => Some(LineKind::Other),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let text = if kind == LineKind::Other || line.is_empty() {
                        line
                    } else {
                        &line[1..]
                    };
                    push(kind, text, file);
                    i += 1;
                    continue;
                }
            }
            hunk = None;
        }
        let next = lines.get(i + 1).copied().unwrap_or("");
        let mut kind = LineKind::Other;
        let mut text = line;
        if let Some(caps) = HUNK.captures(line) {
            let count = |g: usize| caps.get(g).map_or(1, |m| m.as_str().parse().unwrap_or(1));
            hunk = Some((count(2), count(4)));
            section = None;
            stats.hunks += 1;
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            add_path(git_header_path(rest), &mut stats, &mut file);
            section = None;
        } else if CONTEXT_RANGE.is_match(line) {
            section = Some(if line.starts_with("***") {
                Section::Old
            } else {
                Section::New
            });
        } else if line.starts_with("***************") {
            stats.hunks += 1;
            section = None;
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            add_path(normalize_path(header_path(rest)), &mut stats, &mut file);
            section = None;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            // A deleted file's path, or a context diff's new path.
            let deleted = next.starts_with("+++ /dev/null");
            let context_header =
                !next.starts_with("+++ ") && i > 0 && lines[i - 1].starts_with("*** ");
            if deleted || context_header {
                add_path(normalize_path(header_path(rest)), &mut stats, &mut file);
            }
        } else if let Some(section) = section {
            let body = line.get(2..).unwrap_or("");
            if line.starts_with("+ ") || line == "+" {
                stats.added += 1;
                (kind, text) = (LineKind::Added, body);
            } else if line.starts_with("- ") || line == "-" {
                stats.removed += 1;
                (kind, text) = (LineKind::Removed, body);
            } else if line.starts_with("! ") || line == "!" {
                match section {
                    Section::Old => {
                        stats.removed += 1;
                        kind = LineKind::Removed;
                    }
                    Section::New => {
                        stats.added += 1;
                        kind = LineKind::Added;
                    }
                }
                text = body;
            } else if line.starts_with("  ") {
                (kind, text) = (LineKind::Context, body);
            }
        } else if let Some(rest) = line.strip_prefix("Index: ") {
            add_path(normalize_path(rest.trim()), &mut stats, &mut file);
        } else if let Some(rest) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            add_path(normalize_path(rest.trim()), &mut stats, &mut file);
        } else if let Some(rest) = line.strip_prefix("Binary files ")
            && let Some((_, new)) = rest.trim_end_matches(" differ").split_once(" and ")
        {
            add_path(normalize_path(new.trim()), &mut stats, &mut file);
        }
        push(kind, text, file);
        i += 1;
    }
    (stats, out)
}

/// The new path of `diff --git a/x b/x`.
fn git_header_path(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if rest.starts_with('"') {
        let parts: Vec<&str> = rest.split('"').filter(|p| !p.trim().is_empty()).collect();
        return parts.last().and_then(|p| normalize_path(p));
    }
    if let Some(index) = rest.rfind(" b/") {
        return normalize_path(&rest[index + 1..]);
    }
    let half = rest.len() / 2;
    if rest.len() % 2 == 1 && rest.is_char_boundary(half) && rest[..half] == rest[half + 1..] {
        return normalize_path(&rest[..half]);
    }
    rest.split_whitespace().last().and_then(normalize_path)
}

/// The path of a `---` or `+++` line, without a timestamp.
fn header_path(rest: &str) -> &str {
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    let path = path.trim_matches('"');
    match path.find(" 20").or_else(|| path.find(" 19")) {
        Some(index) if path[index + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
            path[..index].trim()
        }
        _ => path,
    }
}

/// The top-level directories and files of the PostgreSQL tree.
const TREE_DIRS: &[&str] = &["src", "contrib", "doc", "config", "ci", ".github"];
const TREE_FILES: &[&str] = &[
    "meson.build",
    "meson_options.txt",
    "configure",
    "configure.ac",
    "GNUmakefile.in",
    "Makefile",
    "aclocal.m4",
    "COPYRIGHT",
    "HISTORY",
    "README",
    "README.md",
    "README.git",
    ".cirrus.yml",
    ".cirrus.star",
    ".cirrus.tasks.yml",
    ".gitattributes",
    ".gitignore",
    ".editorconfig",
    ".dir-locals.el",
    ".git-blame-ignore-revs",
    ".mailmap",
    ".clang-format",
];

/// Whether `path` is a path in the PostgreSQL tree.
pub fn is_tree_path(path: &str) -> bool {
    match path.split_once('/') {
        Some((top, rest)) => TREE_DIRS.contains(&top) && !rest.is_empty(),
        None => TREE_FILES.contains(&path),
    }
}

/// A diff path as a tree path: without git's `a/`/`b/` prefixes, `./`, or the
/// checkout directory of a plain `diff -r`. None for `/dev/null`.
pub fn normalize_path(path: &str) -> Option<String> {
    let mut path = path.trim().trim_matches('"').trim();
    if path.is_empty() || path == "/dev/null" || path.contains(char::is_whitespace) {
        return None;
    }
    for prefix in ["a/", "b/", "i/", "w/", "c/", "o/"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && !rest.is_empty()
        {
            path = rest;
            break;
        }
    }
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    if !is_tree_path(path) {
        let parts: Vec<&str> = path.split('/').collect();
        if let Some(k) =
            (1..parts.len().saturating_sub(1).min(8)).find(|&k| TREE_DIRS[..4].contains(&parts[k]))
        {
            return Some(parts[k..].join("/"));
        }
    }
    let path = path.trim_start_matches('/');
    (!path.is_empty()).then(|| path.to_owned())
}

/// The line ranges of diffs in a body whose quoted lines were replaced by
/// `>`: what [`extract`] keeps as inline patches and cleaning drops.
pub(crate) fn diff_blocks(lines: &[&str]) -> Vec<Range<usize>> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if starts_diff(lines, i) {
            let end = scan_diff(lines, i);
            if end > i {
                blocks.push(i..end);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    blocks
}

fn starts_diff(lines: &[&str], i: usize) -> bool {
    let line = lines[i];
    let next = |k: usize| lines.get(i + k).copied().unwrap_or("");
    line.starts_with("diff --git ")
        || (line.starts_with("diff ")
            && (1..=4).any(|k| next(k).starts_with("--- ") || next(k).starts_with("*** ")))
        || (line.starts_with("Index: ") && next(1).starts_with("===="))
        || (line.starts_with("--- ") && next(1).starts_with("+++ ") && next(2).starts_with("@@"))
        || (line.starts_with("*** ")
            && next(1).starts_with("--- ")
            && next(2).starts_with("***************"))
}

const HEADER_PREFIXES: &[&str] = &[
    "diff ",
    "index ",
    "--- ",
    "+++ ",
    "new file mode",
    "deleted file mode",
    "old mode",
    "new mode",
    "similarity index",
    "dissimilarity index",
    "rename from",
    "rename to",
    "copy from",
    "copy to",
    "Binary files ",
    "GIT binary patch",
    "Index: ",
    "=====",
    "*** ",
    "Only in ",
];

fn is_diff_header(line: &str) -> bool {
    HEADER_PREFIXES.iter().any(|p| line.starts_with(p))
}

/// The end (exclusive) of the diff that starts at `start`, or `start` if
/// there is no hunk.
fn scan_diff(lines: &[&str], start: usize) -> usize {
    let mut i = start;
    let mut hunk: Option<(i64, i64)> = None;
    let mut loose = false;
    let mut context = false;
    let mut saw_hunk = false;
    while i < lines.len() {
        let line = lines[i];
        if let Some((old, new)) = &mut hunk {
            if *old > 0 || *new > 0 {
                match line.as_bytes().first() {
                    Some(b'+') => *new -= 1,
                    Some(b'-') => *old -= 1,
                    Some(b' ') | None => {
                        *old -= 1;
                        *new -= 1;
                    }
                    Some(b'\\') => {}
                    _ => break,
                }
                i += 1;
                continue;
            }
            hunk = None;
        }
        if line.starts_with("@@") {
            match HUNK.captures(line) {
                Some(caps) => {
                    let count =
                        |g: usize| caps.get(g).map_or(1, |m| m.as_str().parse().unwrap_or(1));
                    hunk = Some((count(2), count(4)));
                    loose = false;
                }
                None => loose = true,
            }
            context = false;
            saw_hunk = true;
        } else if line.starts_with("***************") {
            context = true;
            saw_hunk = true;
        } else if is_diff_header(line) || line.starts_with("Binary files ") {
            loose = false;
            if line.starts_with("Binary files ") {
                saw_hunk = true;
            }
        } else if is_context_line(line, context) || is_loose_line(line, loose) {
            // A line of a context diff's hunk, or of a hunk without counts.
        } else if line.trim().is_empty() {
            let next = lines[i + 1..].iter().find(|l| !l.trim().is_empty());
            let continues = next.is_some_and(|l| {
                is_diff_header(l)
                    || l.starts_with("@@")
                    || (context && (l.starts_with("  ") || l.starts_with("! ")))
            });
            if !continues {
                break;
            }
        } else {
            break;
        }
        i += 1;
    }
    if !saw_hunk {
        return start;
    }
    while i > start && lines[i - 1].trim().is_empty() {
        i -= 1;
    }
    i
}

fn is_context_line(line: &str, context: bool) -> bool {
    context
        && (line.starts_with("  ")
            || line.starts_with("+ ")
            || line.starts_with("- ")
            || line.starts_with("! ")
            || matches!(line, "+" | "-" | "!"))
}

fn is_loose_line(line: &str, loose: bool) -> bool {
    loose && matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ' | b'\\'))
}

fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let decoder = flate2::read::MultiGzDecoder::new(data);
    match decoder
        .take(MAX_DECOMPRESSED as u64 + 1)
        .read_to_end(&mut out)
    {
        Ok(n) if n <= MAX_DECOMPRESSED => Some(out),
        _ => None,
    }
}

fn is_tar(data: &[u8]) -> bool {
    data.len() >= 512 && &data[257..262] == b"ustar"
}

/// The regular files of a tar archive, by name.
fn tar_entries(data: &[u8]) -> Vec<(String, &[u8])> {
    let mut entries = Vec::new();
    let mut offset = 0;
    let mut long_name: Option<String> = None;
    while offset + 512 <= data.len() && entries.len() < 1_000 {
        let header = &data[offset..offset + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let field = |range: Range<usize>| {
            let bytes = &header[range];
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        };
        let size = usize::from_str_radix(field(124..136).trim(), 8).unwrap_or(0);
        let kind = header[156];
        let body_start = offset + 512;
        let body_end = (body_start + size).min(data.len());
        let body = &data[body_start..body_end];
        match kind {
            b'L' => {
                let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                long_name = Some(String::from_utf8_lossy(&body[..end]).into_owned());
            }
            b'0' | 0 => {
                let name = long_name.take().unwrap_or_else(|| {
                    let prefix = field(345..500);
                    let name = field(0..100);
                    if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    }
                });
                if !name.rsplit('/').next().unwrap_or("").starts_with("._") {
                    entries.push((name, body));
                }
            }
            _ => long_name = None,
        }
        offset = body_start + size.div_ceil(512) * 512;
    }
    entries
}

#[cfg(test)]
mod tests;
