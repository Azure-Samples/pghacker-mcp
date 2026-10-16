// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Search chunks: a message's subject and cleaned body cut at paragraph
//! boundaries into pieces of about 1,200–1,800 characters, for BM25 and
//! embeddings.

/// A chunk grows past this many characters only if nothing else fits.
pub const MIN_CHARS: usize = 1_200;
/// Chunks never grow past this many characters.
pub const MAX_CHARS: usize = 1_800;
/// Where oversized paragraphs and lines are cut.
const CUT_CHARS: usize = 1_500;

/// The chunks of a message: the subject leads the first; at least one chunk,
/// even for an empty message.
pub fn chunk(subject: &str, text: &str) -> Vec<String> {
    let subject = subject.trim();
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    if !subject.is_empty() {
        current.push_str(subject);
        current_len = char_len(subject);
    }
    // The subject always leads the first piece, even past MAX_CHARS.
    let mut subject_only = current_len > 0;
    for piece in pieces(text) {
        let len = char_len(&piece);
        if current_len == 0 {
            current = piece;
            current_len = len;
        } else if subject_only || current_len + 2 + len <= MAX_CHARS {
            current.push_str("\n\n");
            current.push_str(&piece);
            current_len += 2 + len;
        } else {
            chunks.push(std::mem::take(&mut current));
            current = piece;
            current_len = len;
        }
        subject_only = false;
        if current_len >= MIN_CHARS {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
    }
    if current_len > 0 || chunks.is_empty() {
        chunks.push(current);
    }
    // A short tail joins the chunk before it when both fit.
    if chunks.len() >= 2 {
        let last = char_len(&chunks[chunks.len() - 1]);
        let previous = char_len(&chunks[chunks.len() - 2]);
        if last < MIN_CHARS / 4 && previous + 2 + last <= MAX_CHARS {
            let tail = chunks.pop().unwrap_or_default();
            let previous = chunks.last_mut().expect("two chunks");
            previous.push_str("\n\n");
            previous.push_str(&tail);
        }
    }
    chunks
}

/// Paragraphs, with paragraphs over [`MAX_CHARS`] cut into line groups and
/// lines over it cut at spaces.
fn pieces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in paragraphs(text) {
        if char_len(&paragraph) <= MAX_CHARS {
            out.push(paragraph);
            continue;
        }
        let mut group = String::new();
        let mut group_len = 0;
        for line in paragraph.lines() {
            for part in cut_line(line) {
                let len = char_len(&part);
                if group_len > 0 && group_len + 1 + len > CUT_CHARS {
                    out.push(std::mem::take(&mut group));
                    group_len = 0;
                }
                if group_len > 0 {
                    group.push('\n');
                    group_len += 1;
                }
                group.push_str(&part);
                group_len += len;
            }
        }
        if group_len > 0 {
            out.push(group);
        }
    }
    out
}

fn paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if !lines.is_empty() {
                out.push(lines.join("\n"));
                lines.clear();
            }
        } else {
            lines.push(line);
        }
    }
    if !lines.is_empty() {
        out.push(lines.join("\n"));
    }
    out
}

/// A line cut into parts of at most [`CUT_CHARS`] characters, at the last
/// space before the limit when there is one.
fn cut_line(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = line;
    while char_len(rest) > CUT_CHARS {
        let limit = rest
            .char_indices()
            .nth(CUT_CHARS)
            .map_or(rest.len(), |(i, _)| i);
        let cut = match rest[..limit].rfind(' ') {
            Some(space) if space > limit / 2 => space,
            _ => limit,
        };
        parts.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        parts.push(rest.to_owned());
    }
    parts
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(words: usize, word: &str) -> String {
        vec![word; words].join(" ")
    }

    #[test]
    fn short_messages_are_one_chunk_with_the_subject() {
        assert_eq!(
            chunk("Subject", "Hello.\n\nBye."),
            ["Subject\n\nHello.\n\nBye."]
        );
        assert_eq!(chunk("Subject", ""), ["Subject"]);
        assert_eq!(chunk("", ""), [""]);
        assert_eq!(chunk("", "  \n\n"), [""]);
    }

    #[test]
    fn long_messages_split_at_paragraphs_within_bounds() {
        let text = (0..24)
            .map(|i| paragraph(60, &format!("w{i:02}")))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk("A subject", &text);
        assert!(chunks.len() >= 3, "{}", chunks.len());
        assert!(chunks[0].starts_with("A subject\n\nw00 w00"));
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_CHARS, "{}", chunk.len());
        }
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.chars().count() >= MIN_CHARS, "{}", chunk.len());
        }
        // Every paragraph is in exactly one chunk, whole.
        for i in 0..24 {
            let word = format!("w{i:02}");
            let holding: Vec<_> = chunks.iter().filter(|c| c.contains(&word)).collect();
            assert_eq!(holding.len(), 1, "{word}");
            assert_eq!(holding[0].matches(&word).count(), 60);
        }
    }

    #[test]
    fn the_subject_leads_even_a_full_first_chunk() {
        let chunks = chunk("A subject", &paragraph(440, "big"));
        assert!(chunks[0].starts_with("A subject\n\nbig"));
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn oversized_paragraphs_and_lines_are_cut() {
        let line = "x".repeat(4_000);
        let chunks = chunk("S", &line);
        assert!(chunks.iter().all(|c| c.chars().count() <= MAX_CHARS));
        assert_eq!(
            chunks.iter().map(|c| c.matches('x').count()).sum::<usize>(),
            4_000
        );
        let words = paragraph(1_000, "word");
        let chunks = chunk("", &words);
        assert!(chunks.iter().all(|c| c.chars().count() <= MAX_CHARS));
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.matches("word").count())
                .sum::<usize>(),
            1_000
        );
        let multibyte = "é".repeat(3_000);
        assert!(
            chunk("", &multibyte)
                .iter()
                .all(|c| c.chars().count() <= MAX_CHARS)
        );
    }
}
