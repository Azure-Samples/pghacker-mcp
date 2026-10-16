// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Monthly mbox archives, as the archive serves them.
//!
//! Each message starts with a `From <sender> <asctime date>` line and messages
//! are separated by a blank line. The archive does not quote body lines that
//! start with `From ` (no `>From` escaping, so nothing is unescaped either),
//! and patches made by `git format-patch` carry their own
//! `From <sha> Mon Sep 17 00:00:00 2001` lines. So a line starts a message only
//! if it starts the file or follows a blank line, has the `From_` form with a
//! date, isn't git's, and is followed by a header line.

use std::ops::Range;
use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, Utc};
use regex::bytes::Regex;

static FROM_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^From (\S+) +(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) +(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) +(\d{1,2}) +(\d{1,2}):(\d{2})(?::(\d{2}))?(?: +(?:[A-Z]{3,5}|[+-]\d{4}))? +(\d{4})(?: +(?:[A-Z]{3,5}|[+-]\d{4}))?\s*$",
    )
    .unwrap()
});

/// git format-patch's marker date.
const GIT_MAGIC_DATE: &[u8] = b"Mon Sep 17 00:00:00 2001";

/// The messages of an mbox, each from its `From ` line through its last line,
/// without the blank line that separates it from the next.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    ranges(data).into_iter().map(|range| &data[range]).collect()
}

/// The byte ranges of [`split`].
pub fn ranges(data: &[u8]) -> Vec<Range<usize>> {
    let mut starts = Vec::new();
    let mut line_start = 0;
    let mut previous_blank = true;
    while line_start < data.len() {
        let line_end = memchr(b'\n', &data[line_start..]).map_or(data.len(), |i| line_start + i);
        let line = trim_cr(&data[line_start..line_end]);
        if previous_blank && is_message_start(line, &data[(line_end + 1).min(data.len())..]) {
            starts.push(line_start);
        }
        previous_blank = line.iter().all(u8::is_ascii_whitespace);
        line_start = line_end + 1;
    }
    let mut ranges = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = match starts.get(i + 1) {
            // The separating blank line belongs to neither message.
            Some(&next) => next - 1,
            None if data.ends_with(b"\n\n") => data.len() - 1,
            None => data.len(),
        };
        ranges.push(start..end.max(start));
    }
    ranges
}

fn is_message_start(line: &[u8], rest: &[u8]) -> bool {
    if !line.starts_with(b"From ") || !FROM_LINE.is_match(line) {
        return false;
    }
    if line.ends_with(GIT_MAGIC_DATE) {
        return false;
    }
    let next_end = memchr(b'\n', rest).unwrap_or(rest.len());
    is_header_line(trim_cr(&rest[..next_end]))
}

/// A header field's first line: a name of printable characters but `:`, then
/// `:`.
fn is_header_line(line: &[u8]) -> bool {
    match line.iter().position(|&b| b == b':') {
        Some(0) | None => false,
        Some(colon) => line[..colon].iter().all(|&b| (33..=126).contains(&b)),
    }
}

/// The date of an mbox `From ` line, in UTC as the archive writes it.
pub fn from_line_date(line: &[u8]) -> Option<DateTime<Utc>> {
    let caps = FROM_LINE.captures(trim_cr(line))?;
    let text = |i: usize| std::str::from_utf8(caps.get(i)?.as_bytes()).ok();
    let month = match text(2)? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u32 = text(3)?.parse().ok()?;
    let hour: u32 = text(4)?.parse().ok()?;
    let minute: u32 = text(5)?.parse().ok()?;
    let second: u32 = text(6).map_or(Some(0), |s| s.parse().ok())?;
    let year: i32 = text(7)?.parse().ok()?;
    let date =
        NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, second.min(59))?;
    Some(date.and_utc())
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBOX: &[u8] = b"From list-owner+archive@lists.example.org Mon Jun 01 00:41:00 2026
Received: from a by b
From: Alice Example <alice@example.org>
Subject: first

From what I can tell, this line is body text.

From 1a2b3c4d5e6f7890abcdef1234567890abcdef12 Mon Sep 17 00:00:00 2001
From: Alice Example <alice@example.org>
Subject: [PATCH] a patch inside the first message

From list-owner+archive@lists.example.org Tue Jun  2 10:00:00 2026
Message-ID: <second@example.org>

>From the archive's point of view this stays as written.
From list-owner+archive@lists.example.org Tue Jun 02 10:00:01 2026
Not a header line, so still the second message.

From list-owner+archive@lists.example.org Wed Jun 03 12:30:45 2026
Message-ID: <third@example.org>

last
";

    #[test]
    fn splits_only_on_real_from_lines() {
        let messages = split(MBOX);
        assert_eq!(messages.len(), 3);
        let first = std::str::from_utf8(messages[0]).unwrap();
        assert!(first.starts_with("From list-owner"));
        assert!(first.contains("From what I can tell"));
        assert!(first.contains("Subject: [PATCH] a patch inside the first message\n"));
        assert!(first.ends_with("message\n"), "{first:?}");
        let second = std::str::from_utf8(messages[1]).unwrap();
        assert!(second.contains("\n>From the archive's point of view"));
        assert!(second.contains("still the second message.\n"));
        assert!(!second.ends_with("\n\n"));
        assert!(
            std::str::from_utf8(messages[2])
                .unwrap()
                .ends_with("last\n")
        );
    }

    #[test]
    fn handles_edges() {
        assert!(split(b"").is_empty());
        assert!(split(b"no mbox here\n").is_empty());
        let one = b"From a@example.org Mon Jun 01 00:41:00 2026\nSubject: x\n\nbody";
        assert_eq!(split(one), vec![&one[..]]);
    }

    #[test]
    fn parses_from_line_dates() {
        let date = from_line_date(b"From a@example.org Tue Jun  2 10:00:00 2026").unwrap();
        assert_eq!(date.to_rfc3339(), "2026-06-02T10:00:00+00:00");
        assert!(from_line_date(b"From what I can tell").is_none());
    }
}
