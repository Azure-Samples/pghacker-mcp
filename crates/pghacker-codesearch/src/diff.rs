// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Unified diffs rendered from the weave, formatted as git diff formats
//! them: within each change block the removed lines come before the added
//! ones, hunks closer than twice the context merge, and each hunk header
//! names git's default function context (the nearest line above the hunk,
//! in the old file, starting with a letter, `_` or `$`).

use std::borrow::Cow;

/// A line of a file's old version, new version or both, in weave order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeaveLine<'a> {
    pub old: bool,
    pub new: bool,
    pub text: Cow<'a, str>,
}

/// One side of a file's diff.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub final_newline: bool,
    pub bom: bool,
}

const BOM: &str = "\u{feff}";
const NO_NEWLINE: &str = "\\ No newline at end of file";

/// git's default function line: one starting with a letter, `_` or `$`, cut
/// to 80 bytes, trailing whitespace trimmed.
fn func_name(line: &str) -> Option<&str> {
    let first = *line.as_bytes().first()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut end = line.len().min(80);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(line[..end].trim_end_matches(|c: char| c.is_ascii_whitespace()))
}

/// The lines in diff order: each change block's removed lines, then its
/// added ones. Lines exclude a BOM, but git sees a first line that gained or
/// lost one as changed, so such a line kept by the weave becomes a removal
/// and an addition.
fn ordered<'a>(lines: &[WeaveLine<'a>], old: Side, new: Side) -> Vec<WeaveLine<'a>> {
    let mut lines: Vec<WeaveLine<'a>> = lines.iter().filter(|l| l.old || l.new).cloned().collect();
    if old.bom != new.bom {
        let first = if old.bom {
            lines.iter().position(|l| l.old)
        } else {
            lines.iter().position(|l| l.new)
        };
        if let Some(k) = first.filter(|&k| lines[k].old && lines[k].new) {
            let text = lines[k].text.clone();
            lines[k].new = false;
            lines.insert(
                k + 1,
                WeaveLine {
                    old: false,
                    new: true,
                    text,
                },
            );
        }
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].old && lines[i].new {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && !(lines[i].old && lines[i].new) {
            i += 1;
        }
        out.extend(lines[start..i].iter().filter(|l| l.old).cloned());
        out.extend(lines[start..i].iter().filter(|l| l.new).cloned());
    }
    out
}

/// One file's hunks with `context` lines around each change, as `git diff`
/// prints them after the file's header.
pub fn hunks(lines: &[WeaveLine<'_>], old: Side, new: Side, context: usize) -> Vec<String> {
    let seq = ordered(lines, old, new);
    let n = seq.len();
    let changed = |l: &WeaveLine<'_>| !(l.old && l.new);
    // Changes closer than 2 * context lines share a hunk.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for k in (0..n).filter(|&k| changed(&seq[k])) {
        match groups.last_mut() {
            Some((_, last)) if k - *last - 1 <= 2 * context => *last = k,
            _ => groups.push((k, k)),
        }
    }
    if groups.is_empty() {
        return Vec::new();
    }
    // Lines of each side before each position.
    let mut old_before = Vec::with_capacity(n + 1);
    let mut new_before = Vec::with_capacity(n + 1);
    let (mut o, mut w) = (0usize, 0usize);
    for l in &seq {
        old_before.push(o);
        new_before.push(w);
        o += usize::from(l.old);
        w += usize::from(l.new);
    }
    old_before.push(o);
    new_before.push(w);
    let (old_len, new_len) = (o, w);
    let old_lines: Vec<&str> = seq.iter().filter(|l| l.old).map(|l| &*l.text).collect();

    let mut out = Vec::new();
    // The function line persists from hunk to hunk until a nearer one.
    let mut func: Option<&str> = None;
    let mut searched_to: i64 = -1;
    for (first, last) in groups {
        let start = first.saturating_sub(context);
        let end = (last + 1 + context).min(n);
        let (o0, n0) = (old_before[start], new_before[start]);
        let (oc, nc) = (old_before[end] - o0, new_before[end] - n0);
        let from = o0 as i64 - 1;
        let mut l = from;
        while l != searched_to && l >= 0 {
            let candidate = old_lines[l as usize];
            if !(l == 0 && old.bom)
                && let Some(f) = func_name(candidate)
            {
                func = Some(f);
                break;
            }
            l -= 1;
        }
        searched_to = from;
        let range = |start: usize, count: usize| match count {
            0 => format!("{start},0"),
            1 => format!("{}", start + 1),
            _ => format!("{},{count}", start + 1),
        };
        let mut header = format!("@@ -{} +{} @@", range(o0, oc), range(n0, nc));
        if let Some(f) = func.filter(|f| !f.is_empty()) {
            header.push(' ');
            header.push_str(f);
        }
        out.push(header);
        for k in start..end {
            let l = &seq[k];
            let sign = match (l.old, l.new) {
                (true, true) => ' ',
                (true, false) => '-',
                _ => '+',
            };
            let old_no = old_before[k];
            let new_no = new_before[k];
            let bom = (l.old && old_no == 0 && old.bom && sign != '+')
                || (l.new && new_no == 0 && new.bom && sign != '-');
            out.push(format!("{sign}{}{}", if bom { BOM } else { "" }, l.text));
            let last_old = l.old && old_no + 1 == old_len && !old.final_newline;
            let last_new = l.new && new_no + 1 == new_len && !new.final_newline;
            if last_old || last_new {
                out.push(NO_NEWLINE.to_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weave(spec: &[(&str, &'static str)]) -> Vec<WeaveLine<'static>> {
        spec.iter()
            .map(|&(side, text)| WeaveLine {
                old: side.contains('o'),
                new: side.contains('n'),
                text: Cow::Borrowed(text),
            })
            .collect()
    }

    const PLAIN: Side = Side {
        final_newline: true,
        bom: false,
    };

    #[test]
    fn hunks_as_git_prints_them() {
        let mut spec: Vec<(&str, &'static str)> = vec![("on", "static int"), ("on", "f(void)")];
        spec.push(("on", "{"));
        for _ in 0..3 {
            spec.push(("on", "\tx;"));
        }
        // An insertion placed after the line it replaces, in weave order.
        spec.push(("o", "\treturn 1;"));
        spec.push(("n", "\treturn 2;"));
        spec.push(("n", "\treturn 3;"));
        for _ in 0..10 {
            spec.push(("on", "\ty;"));
        }
        spec.push(("o", "\tgone;"));
        spec.push(("on", "}"));
        let out = hunks(&weave(&spec), PLAIN, PLAIN, 3);
        assert_eq!(
            out,
            [
                "@@ -4,7 +4,8 @@ f(void)",
                " \tx;",
                " \tx;",
                " \tx;",
                "-\treturn 1;",
                "+\treturn 2;",
                "+\treturn 3;",
                " \ty;",
                " \ty;",
                " \ty;",
                "@@ -15,5 +16,4 @@ f(void)",
                " \ty;",
                " \ty;",
                " \ty;",
                "-\tgone;",
                " }",
            ]
        );
        // Close changes share a hunk.
        assert_eq!(
            hunks(&weave(&spec), PLAIN, PLAIN, 5)[0],
            "@@ -2,18 +2,18 @@ static int"
        );
    }

    #[test]
    fn new_and_deleted_files_and_newlines() {
        let added = weave(&[("n", "a"), ("n", "b")]);
        assert_eq!(
            hunks(&added, PLAIN, PLAIN, 3),
            ["@@ -0,0 +1,2 @@", "+a", "+b"]
        );
        let no_nl = Side {
            final_newline: false,
            bom: false,
        };
        let deleted = weave(&[("o", "only")]);
        assert_eq!(
            hunks(&deleted, no_nl, PLAIN, 3),
            ["@@ -1 +0,0 @@", "-only", NO_NEWLINE]
        );
        // The last line gains its newline.
        let fixed = weave(&[("on", "a"), ("o", "b"), ("n", "b")]);
        assert_eq!(
            hunks(&fixed, no_nl, PLAIN, 3),
            ["@@ -1,2 +1,2 @@", " a", "-b", NO_NEWLINE, "+b"]
        );
        // A BOM removed from a kept first line.
        let bom = Side {
            final_newline: true,
            bom: true,
        };
        let lines = weave(&[("on", "x"), ("on", "y")]);
        assert_eq!(
            hunks(&lines, bom, PLAIN, 3),
            ["@@ -1,2 +1,2 @@", "-\u{feff}x", "+x", " y"]
        );
        assert!(hunks(&lines, PLAIN, PLAIN, 3).is_empty());
    }
}
