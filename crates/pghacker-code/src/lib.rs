// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! PostgreSQL's commit history, which the code tools read: every line of every
//! commit on every branch, stored once. [`history`] builds it from a clone
//! into a schema of its own and installs it in `pghacker`, where syncs apply
//! new commits, and checks it against git.
//!
//! Files are read by fixed rules, which the exactness replay relies on: every
//! tracked file under the configured path prefixes; symlinks and submodules
//! skipped; a blob with a NUL byte, or that isn't UTF-8, is binary to the
//! code tools (the history still keeps the lines of one without a NUL, read
//! as Latin-1); a leading UTF-8 BOM is set aside; lines split on `\n`, a
//! trailing `\r` staying in its line; the empty element after a final
//! newline is dropped; an empty file has no lines.

pub mod history;

/// A text file's lines and whether it ends with a newline, per the rules
/// above; `None` for binary content.
pub fn split_lines(data: &[u8]) -> Option<(Vec<&str>, bool)> {
    if data.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.is_empty() {
        return Some((Vec::new(), false));
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    let final_newline = lines.len() > 1 && lines.last() == Some(&"");
    if final_newline {
        lines.pop();
    }
    Some((lines, final_newline))
}

/// The subsystem a PostgreSQL path belongs to: `parser` for
/// `src/backend/parser/gram.y`, `utils/adt` for `src/backend/utils/adt/json.c`,
/// `nodes` for `src/include/nodes/parsenodes.h`, `contrib/pg_trgm`, and so on.
pub fn subsystem_of(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let dir = |n: usize| (parts.len() > n + 1).then(|| parts[n]);
    match parts.as_slice() {
        ["src", "backend" | "include", "utils", ..] => {
            Some(dir(3).map_or_else(|| "utils".to_owned(), |sub| format!("utils/{sub}")))
        }
        ["src", "backend" | "include", ..] => dir(2).map(str::to_owned),
        ["src", "bin" | "interfaces" | "pl", ..] => dir(2).map(|sub| format!("{}/{sub}", parts[1])),
        ["src", "test", ..] => Some("test".to_owned()),
        ["src", ..] => dir(1).map(str::to_owned),
        ["contrib", ..] => dir(1).map(|sub| format!("contrib/{sub}")),
        [top, _, ..] => Some((*top).to_owned()),
        _ => None,
    }
}

pub(crate) fn in_paths(path: &str, prefixes: &[String]) -> bool {
    prefixes.is_empty()
        || prefixes.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_like_the_study() {
        assert_eq!(split_lines(b""), Some((vec![], false)));
        assert_eq!(split_lines(b"a\nb\n"), Some((vec!["a", "b"], true)));
        assert_eq!(split_lines(b"a\nb"), Some((vec!["a", "b"], false)));
        assert_eq!(split_lines(b"\n"), Some((vec![""], true)));
        assert_eq!(split_lines(b"a\r\nb\r\n"), Some((vec!["a\r", "b\r"], true)));
        assert_eq!(
            split_lines("\u{feff}x\n".as_bytes()),
            Some((vec!["x"], true))
        );
        assert_eq!(split_lines(b"a\0b"), None);
        assert_eq!(split_lines(&[0xff, 0xfe, b'a']), None);
    }

    #[test]
    fn maps_paths_to_subsystems() {
        assert_eq!(
            subsystem_of("src/backend/parser/gram.y").as_deref(),
            Some("parser")
        );
        assert_eq!(
            subsystem_of("src/backend/utils/adt/jsonfuncs.c").as_deref(),
            Some("utils/adt")
        );
        assert_eq!(
            subsystem_of("src/include/nodes/parsenodes.h").as_deref(),
            Some("nodes")
        );
        assert_eq!(
            subsystem_of("src/include/utils/jsonb.h").as_deref(),
            Some("utils")
        );
        assert_eq!(
            subsystem_of("contrib/pg_trgm/trgm_op.c").as_deref(),
            Some("contrib/pg_trgm")
        );
        assert_eq!(
            subsystem_of("src/bin/psql/describe.c").as_deref(),
            Some("bin/psql")
        );
        assert_eq!(
            subsystem_of("doc/src/sgml/func.sgml").as_deref(),
            Some("doc")
        );
        assert_eq!(subsystem_of("README").as_deref(), None);
    }

    #[test]
    fn filters_by_path_prefix() {
        let prefixes = vec![
            "src/backend/parser".to_owned(),
            "contrib/pg_trgm/".to_owned(),
        ];
        assert!(in_paths("src/backend/parser/gram.y", &prefixes));
        assert!(in_paths("contrib/pg_trgm/trgm.h", &prefixes));
        assert!(!in_paths("src/backend/parser2/x.c", &prefixes));
        assert!(in_paths("anything", &[]));
    }
}
