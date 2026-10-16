// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The author's own words: a body without quoted lines and their "On ...
//! wrote:" attributions, forwarded or Outlook-quoted messages, inline diffs
//! and diffstats, signatures after `-- `, and list footers. What search
//! indexes and chunks.

use std::sync::LazyLock;

use regex::Regex;

use crate::patch::{diff_blocks, is_quoted};

static ATTRIBUTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:\bwrote\b|\bwrites\b|\bhas written\b|a écrit|schrieb|escribió|ha scritto|scrisse|napisał|написал|kirjoitti|\bskrev\b|schreef|escreveu|写道|寫道|のメッセージ|のメール)",
    )
    .unwrap()
});

/// How attributions start when a client wraps them over two lines.
static OPENER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:On|At|Le|Am|El|Il|Op|Em|W dniu|Den|Dne|Päivänä|於|在)\b").unwrap()
});

static ORIGINAL_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:-{3,}\s*original message\s*-{3,}|-{5,}\s*forwarded message\s*-{5,}|begin forwarded message:|-{3,}\s*\(end of broadcast\)\s*-{3,}|sent via pgsql-[a-z]+ mailing list\b)",
    )
    .unwrap()
});

static OUTLOOK_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\*?(?:Sent|To|Date|Subject|Cc)\*?:\s").unwrap());

static DIFFSTAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s(?:\S.*\s\|\s+(?:\d+ ?[+-]*|Bin(?: \d+ -> \d+ bytes)?)|\d+ files? changed(?:, \d+ insertions?\(\+\))?(?:, \d+ deletions?\(-\))?)\s*$",
    )
    .unwrap()
});

static GIT_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d+\.\d+(?:\.\d+)*(?:[.-][A-Za-z0-9.]+)?(?: \(Apple Git-\d+\))?\s*$").unwrap()
});

static GIT_MAGIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^From [0-9a-f]{40} Mon Sep 17 00:00:00 2001$").unwrap());

/// A body without what its author didn't write. Whitespace inside lines is
/// kept, so quotes of the result match the original.
pub fn clean_body(body: &str) -> String {
    let body = body.replace("\r\n", "\n");
    let mut lines: Vec<&str> = body.lines().collect();
    if let Some(cut) = cut_point(&lines) {
        lines.truncate(cut);
    }
    let mut keep = vec![true; lines.len()];

    // Inline diffs, diffstats and git format-patch headers.
    let masked: Vec<&str> = lines
        .iter()
        .map(|line| if is_quoted(line) { ">" } else { line })
        .collect();
    for block in diff_blocks(&masked) {
        for k in block {
            keep[k] = false;
        }
    }
    let mut k = 0;
    while k < lines.len() {
        if GIT_MAGIC.is_match(lines[k]) {
            keep[k] = false;
            k += 1;
            while k < lines.len() && !lines[k].trim().is_empty() {
                keep[k] = false;
                k += 1;
            }
            continue;
        }
        // git format-patch's trailer: "-- " and the git version.
        if lines[k] == "-- " && lines.get(k + 1).is_some_and(|l| GIT_VERSION.is_match(l)) {
            keep[k] = false;
            keep[k + 1] = false;
            k += 2;
            continue;
        }
        if DIFFSTAT.is_match(lines[k]) {
            keep[k] = false;
            if k > 0 && lines[k - 1].trim() == "---" {
                keep[k - 1] = false;
            }
        }
        k += 1;
    }

    // Quotes and their attributions.
    for k in 0..lines.len() {
        if !is_quoted(lines[k]) || (k > 0 && is_quoted(lines[k - 1])) {
            continue;
        }
        for line in attribution_before(&lines, k) {
            keep[line] = false;
        }
    }
    for (k, line) in lines.iter().enumerate() {
        if is_quoted(line) {
            keep[k] = false;
        }
    }

    // The signature.
    if let Some(start) = signature_start(&lines, &keep) {
        for flag in &mut keep[start..] {
            *flag = false;
        }
    }

    let mut out = String::with_capacity(body.len());
    let mut blank = 0;
    for (k, line) in lines.iter().enumerate() {
        if !keep[k] {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    out
}

/// Where a forwarded message, an Outlook-style quote or a list footer
/// starts: everything from there on goes.
fn cut_point(lines: &[&str]) -> Option<usize> {
    for (k, line) in lines.iter().enumerate() {
        if is_quoted(line) {
            continue;
        }
        if ORIGINAL_MESSAGE.is_match(line) {
            // "-- " before a list footer goes too.
            if k > 0 && matches!(lines[k - 1].trim_end(), "--" | "") {
                return Some(k - 1);
            }
            return Some(k);
        }
        let outlook_from = line.starts_with("From: ") || line.starts_with("*From:*");
        let outlook_rule = line.len() >= 10 && line.chars().all(|c| c == '_');
        if outlook_from || outlook_rule {
            let fields = lines[k + 1..]
                .iter()
                .take(5)
                .filter(|l| OUTLOOK_FIELD.is_match(l))
                .count();
            let has_to_or_sent = lines[k + 1..].iter().take(5).any(|l| {
                let l = l.trim_start_matches('*');
                l.starts_with("Sent") || l.starts_with("To")
            });
            if (outlook_from && fields >= 2 && has_to_or_sent)
                || (outlook_rule
                    && lines
                        .get(k + 1)
                        .is_some_and(|l| l.starts_with("From: ") || l.starts_with("*From:*")))
            {
                return Some(k);
            }
        }
    }
    None
}

/// The lines of the attribution right before the quote that starts at
/// `quote`: "On ... wrote:", possibly wrapped, possibly followed by the
/// quoted message's Message-ID, possibly separated from the quote by a blank
/// line.
fn attribution_before(lines: &[&str], quote: usize) -> Vec<usize> {
    let mut k = quote;
    if k > 0 && lines[k - 1].trim().is_empty() {
        k -= 1;
    }
    let mut candidates = Vec::new();
    while k > 0 && candidates.len() < 3 {
        let line = lines[k - 1];
        if line.trim().is_empty() || is_quoted(line) {
            break;
        }
        candidates.push(k - 1);
        k -= 1;
    }
    let Some(&last) = candidates.first() else {
        return Vec::new();
    };
    let is_attribution = |i: usize| lines[i].len() < 400 && ATTRIBUTION.is_match(lines[i]);
    let is_message_id = |i: usize| {
        let t = lines[i].trim();
        t.starts_with('<') && t.ends_with('>') && !t.contains(' ')
    };
    let mut drop = Vec::new();
    let mut anchor = None;
    if is_attribution(last) {
        drop.push(last);
        anchor = Some(1);
    } else if is_message_id(last) && candidates.get(1).is_some_and(|&i| is_attribution(i)) {
        drop.push(last);
        drop.push(candidates[1]);
        anchor = Some(2);
    } else if OPENER.is_match(lines[last]) && lines[last].trim_end().ends_with(':') {
        drop.push(last);
        anchor = Some(1);
    }
    if let Some(n) = anchor {
        let attribution_line = candidates[n - 1];
        if !OPENER.is_match(lines[attribution_line])
            && let Some(&before) = candidates.get(n)
            && OPENER.is_match(lines[before])
        {
            drop.push(before);
        }
    }
    drop
}

/// The first line of a signature: `-- ` with at most a dozen lines after it,
/// or a bare `--` with at most six.
fn signature_start(lines: &[&str], keep: &[bool]) -> Option<usize> {
    for (k, line) in lines.iter().enumerate() {
        if !keep[k] {
            continue;
        }
        let limit = match *line {
            "-- " => 12,
            "--" => 6,
            _ => continue,
        };
        let after = (k + 1..lines.len())
            .filter(|&i| keep[i] && !lines[i].trim().is_empty())
            .count();
        if after <= limit {
            return Some(k);
        }
    }
    None
}

#[cfg(test)]
mod tests;
