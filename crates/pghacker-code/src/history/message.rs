// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Commit messages' trailers, such as `Discussion:` and `Reviewed-by:`.
//!
//! PostgreSQL's commits put them anywhere after the subject, often in a
//! final paragraph but sometimes with blank lines between, and spell some
//! keys several ways. Only known keys count, so lines such as `ERROR:` in a
//! quoted log aren't trailers.

use std::collections::BTreeMap;

/// A known key: its spellings, lowercase, and whether its values name
/// people (several per line, separated by commas).
struct Key {
    name: &'static str,
    spellings: &'static [&'static str],
    people: bool,
}

const KEYS: &[Key] = &[
    Key {
        name: "Discussion",
        spellings: &["discussion"],
        people: false,
    },
    Key {
        name: "Author",
        spellings: &["author", "authors"],
        people: true,
    },
    Key {
        name: "Co-authored-by",
        spellings: &["co-authored-by"],
        people: true,
    },
    Key {
        name: "Reviewed-by",
        spellings: &["reviewed-by", "reviewers"],
        people: true,
    },
    Key {
        name: "Reported-by",
        spellings: &["reported-by"],
        people: true,
    },
    Key {
        name: "Tested-by",
        spellings: &["tested-by"],
        people: true,
    },
    Key {
        name: "Suggested-by",
        spellings: &["suggested-by"],
        people: true,
    },
    Key {
        name: "Diagnosed-by",
        spellings: &["diagnosed-by"],
        people: true,
    },
    Key {
        name: "Analyzed-by",
        spellings: &["analyzed-by", "analysed-by"],
        people: true,
    },
    Key {
        name: "Backpatch-through",
        spellings: &["backpatch-through", "backpatch", "backpatch-to"],
        people: false,
    },
    Key {
        name: "Security",
        spellings: &["security"],
        people: false,
    },
    Key {
        name: "Bug",
        spellings: &["bug"],
        people: false,
    },
];

/// The longest value kept.
const MAX_VALUE: usize = 500;

fn key(line: &str) -> Option<(&'static Key, &str)> {
    let (k, rest) = line.split_once(':')?;
    if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-') {
        return None;
    }
    let k = k.to_ascii_lowercase();
    let found = KEYS
        .iter()
        .find(|key| key.spellings.contains(&k.as_str()))?;
    Some((found, rest.trim()))
}

/// A person as a trailer names them, without an email address.
fn person(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0u32;
    for c in raw.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace()
        .filter(|w| !w.contains('@'))
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == ',' || c == '.' || c == ';')
        .trim()
        .to_owned()
}

/// A people line's names: split at commas outside `<...>`.
fn people(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0u32;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
        .into_iter()
        .map(person)
        .filter(|p| !p.is_empty())
        .collect()
}

/// A Discussion line's references: URLs and `<message-id>`s.
fn references(value: &str) -> Vec<String> {
    value
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|t| t.trim_end_matches(['.', ';']))
        .filter(|t| t.contains('@') || t.starts_with("http"))
        .map(str::to_owned)
        .collect()
}

/// The message's trailers by canonical key, values in order: people by
/// name only, Discussion as written (URLs or message IDs), others whole.
pub fn trailers(message: &str) -> BTreeMap<String, Vec<String>> {
    // (key, value) with continuation lines joined.
    let mut raw: Vec<(&'static Key, String)> = Vec::new();
    let mut open = false;
    for line in message.lines().skip(1) {
        if let Some((k, value)) = key(line) {
            raw.push((k, value.to_owned()));
            open = true;
        } else if open && line.starts_with([' ', '\t']) && !line.trim().is_empty() {
            let last = &mut raw.last_mut().expect("an open trailer").1;
            last.push(' ');
            last.push_str(line.trim());
        } else {
            open = false;
        }
    }
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, value) in raw {
        let values = if k.people {
            people(&value)
        } else if k.name == "Discussion" {
            references(&value)
        } else {
            vec![value]
        };
        for mut v in values {
            if v.is_empty() {
                continue;
            }
            if v.len() > MAX_VALUE {
                let mut end = MAX_VALUE;
                while !v.is_char_boundary(end) {
                    end -= 1;
                }
                v.truncate(end);
            }
            let list = out.entry(k.name.to_owned()).or_default();
            if !list.contains(&v) {
                list.push(v);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_postgres_trailers() {
        let message = "Fix hash index scans that start during a bucket split.\n\
            \n\
            Body text.  ERROR:  not a trailer\n\
            Doc: not one either\n\
            \n\
            Author: Peter Geoghegan <pg@bowt.ie>\n\
            Author: Amit Kapila <amit.kapila16@gmail.com>\n\
            Reviewed-By: Vignesh C <vignesh@example.org>, Tom Lane <tom@example.org>,\n\
            \tRobert Haas\n\
            Reported-by: Kacper Kuras\n\
            \n\
            Discussion: https://postgr.es/m/CAH2-Wz=4CJK9@mail.gmail.com\n\
            Discussion: <20190101.123@example.org>\n\
            Backpatch: 9.6-\n\
            Backpatch-through: 14\n";
        let t = trailers(message);
        let get = |k: &str| t.get(k).cloned().unwrap_or_default();
        assert_eq!(get("Author"), ["Peter Geoghegan", "Amit Kapila"]);
        assert_eq!(get("Reviewed-by"), ["Vignesh C", "Tom Lane", "Robert Haas"]);
        assert_eq!(get("Reported-by"), ["Kacper Kuras"]);
        assert_eq!(
            get("Discussion"),
            [
                "https://postgr.es/m/CAH2-Wz=4CJK9@mail.gmail.com",
                "<20190101.123@example.org>"
            ]
        );
        assert_eq!(get("Backpatch-through"), ["9.6-", "14"]);
        assert_eq!(t.len(), 5, "{t:?}");
        // The subject is never a trailer.
        assert!(trailers("Author: fix\n").is_empty());
    }
}
