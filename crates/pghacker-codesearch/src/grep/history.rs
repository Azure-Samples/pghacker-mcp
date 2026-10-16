// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! grep over /history: every line that some commit in a range had, once,
//! printed as GNU grep prints a file's lines, at the first commit in the
//! range that has it: `/commit/<sha12>/<path>:<line>:<text>`, a path the
//! other tools read. `/history/<path>` is all of history,
//! `/history/<range>/<path>` a range: a branch, tag or commit with its
//! history, or `A..B`.

use std::collections::HashMap;

use anyhow::{Result, bail};
use tokio_postgres::GenericClient;

use super::format::Printer;
use super::options::{Mode, Options};
use crate::engine::Budget;
use crate::history::{alive_at, checked, keys};
use crate::history_search::{lineages, matching, range_of};
use crate::pattern::Compiled;
use crate::snapshot::{FileTable, Snapshot, tree_cmp};

/// Whether a grep operand is in /history.
pub fn is_history(op: &str) -> bool {
    let t = op.trim().trim_end_matches('/');
    t == "/history" || t.starts_with("/history/")
}

/// Lines found in a file at a commit.
struct Found {
    path: String,
    addr: i64,
    /// The file's last line number.
    last: i32,
    lines: Vec<(i32, String)>,
}

/// A /history operand's range (None for all of history) and path.
async fn operand(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    table: &FileTable,
    op: &str,
) -> Result<(Option<String>, String)> {
    let rest = op
        .trim()
        .trim_start_matches("/history")
        .trim_matches('/')
        .to_owned();
    let (first, tail) = rest.split_once('/').unwrap_or((rest.as_str(), ""));
    if first.is_empty() {
        return Ok((None, String::new()));
    }
    // A first component the tree has is a path; one that names commits is
    // a range.
    let range_like = first.contains("..") || first.contains(['@', '~', '^']);
    if !range_like && table.exists(first) {
        return Ok((None, rest.clone()));
    }
    let h = &snapshot.history;
    let names = match first.split_once("..") {
        Some((a, b)) => vec![a.to_owned(), b.to_owned()],
        None => vec![first.to_owned()],
    };
    for name in &names {
        if crate::rev::resolve(client, &h.schema, &snapshot.repo, Some(name))
            .await
            .is_err()
        {
            return Ok((None, rest.clone()));
        }
    }
    Ok((Some(first.to_owned()), tail.to_owned()))
}

/// Runs a grep whose operands are all in /history: its stdout, stderr and
/// status, or None when the search stopped early.
pub(crate) async fn grep(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    opts: &Options,
    compiled: &Compiled,
    budget: &Budget,
) -> Result<Option<(Vec<u8>, Vec<String>, i32)>> {
    let h = &snapshot.history;
    if opts.invert || opts.before > 0 || opts.after > 0 || opts.mode == Mode::FilesWithoutMatch {
        bail!("grep: in /history, -v, -L and context lines aren't supported");
    }
    let s = checked(&h.schema)?;
    let table = FileTable::load(client, snapshot).await?;
    // Every file found: (path, commit) → its lines.
    let mut files: Vec<Found> = Vec::new();
    for op in &opts.operands {
        let (spec, path) = operand(client, snapshot, &table, op).await?;
        let range = match &spec {
            Some(spec) => range_of(client, snapshot, h, spec).await?,
            None => None,
        };
        let scope = if path.is_empty() {
            None
        } else {
            Some(lineages(client, s, std::slice::from_ref(&path)).await?)
        };
        let (rows, found) = matching(
            client,
            h,
            vec![&compiled.filter],
            Vec::new(),
            scope,
            range.as_ref(),
            budget,
        )
        .await?;
        if found.capped || found.timed_out {
            return Ok(None);
        }
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|m| compiled.matcher.is_match(&m.line))
            .collect();
        // Each line at the first commit in the range that has it.
        let firsts: Vec<i64> = rows
            .iter()
            .map(|m| match &range {
                Some(r) => m
                    .alive
                    .0
                    .iter()
                    .filter_map(|&(a, b)| r.first_in(a, b))
                    .min()
                    .unwrap_or(m.lo),
                None => m.lo,
            })
            .collect();
        let lineage: Vec<i32> = rows.iter().map(|m| m.lineage).collect();
        let at: HashMap<usize, (i32, String)> = client
            .query(
                &format!(
                    "SELECT q.n::int8, v.id, v.path FROM unnest($1::int4[], $2::int8[]) \
                     WITH ORDINALITY AS q(lineage, addr, n) \
                     JOIN {s}.code_file_versions v ON v.lineage = q.lineage AND {}",
                    alive_at("v", "q.addr")
                ),
                &[&lineage, &firsts],
            )
            .await?
            .iter()
            .map(|r| (r.get::<_, i64>(0) as usize - 1, (r.get(1), r.get(2))))
            .collect();
        let versions: Vec<i32> = at.values().map(|v| v.0).collect();
        let numbers = keys(client, h, &versions).await?;
        let mut by_file: HashMap<(String, i64), Found> = HashMap::new();
        for (k, m) in rows.into_iter().enumerate() {
            let Some((version, path)) = at.get(&k) else {
                continue;
            };
            let base = path.rsplit('/').next().unwrap_or(path);
            if opts.files.excluded(base, true) {
                continue;
            }
            let Some(list) = numbers.get(version) else {
                continue;
            };
            let Ok(n) = list.binary_search(&m.weave) else {
                continue;
            };
            by_file
                .entry((path.clone(), firsts[k]))
                .or_insert_with(|| Found {
                    path: path.clone(),
                    addr: firsts[k],
                    last: list.len() as i32,
                    lines: Vec::new(),
                })
                .lines
                .push((n as i32 + 1, m.line));
        }
        let mut found: Vec<Found> = by_file.into_values().collect();
        for f in &mut found {
            f.lines.sort();
            f.lines.dedup_by_key(|l| l.0);
        }
        found.sort_by(|a, b| tree_cmp(&a.path, &b.path).then(a.addr.cmp(&b.addr)));
        files.extend(found);
    }
    let addrs: Vec<i64> = files.iter().map(|f| f.addr).collect();
    let shas: HashMap<i64, String> = client
        .query(
            &format!("SELECT addr, left(sha, 12) FROM {s}.code_commits WHERE addr = ANY($1)"),
            &[&addrs],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let mut printer = Printer::new(opts, &compiled.matcher, opts.with_filename != Some(false));
    for f in &files {
        let selected: Vec<i32> = f.lines.iter().map(|l| l.0).collect();
        let by_line: HashMap<i32, &str> = f.lines.iter().map(|(n, t)| (*n, t.as_str())).collect();
        let text = |n: i32| by_line.get(&n).map(|t| (*t).to_owned());
        let sha = shas.get(&f.addr).map_or("?", String::as_str);
        let name = format!("/commit/{sha}/{}", f.path);
        let found = printer.file(&name, &selected, None, f.last, &text);
        if opts.mode == Mode::Quiet && found > 0 {
            break;
        }
    }
    let status = i32::from(printer.selected == 0);
    Ok(Some((printer.out, Vec::new(), status)))
}
