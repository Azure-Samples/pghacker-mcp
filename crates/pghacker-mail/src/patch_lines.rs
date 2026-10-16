// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Indexes patches' lines for search_patches: each distinct line once in
//! patch_texts, by kind, and each patch's lines in order as text_ids.

use std::collections::HashMap;

use crate::patch::parse_diff;
use anyhow::{Result, bail};
use tokio_postgres::Client;

/// What a run indexed.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Indexed {
    pub patches: u64,
    pub lines: u64,
    /// More patches wait.
    pub more: bool,
}

/// Indexes up to `limit` patches that aren't yet, in one transaction.
pub async fn index(client: &mut Client, limit: i64) -> Result<Indexed> {
    let tx = client.transaction().await?;
    let rows = tx
        .query(
            "SELECT id, content FROM patches WHERE text_ids IS NULL \
             ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
            &[&(limit + 1)],
        )
        .await?;
    let mut out = Indexed {
        more: rows.len() as i64 > limit,
        ..Indexed::default()
    };
    let rows = &rows[..rows.len().min(limit as usize)];
    if rows.is_empty() {
        return Ok(out);
    }
    // Every patch's lines as (kind, text).
    let patches: Vec<(i64, Vec<(char, &str)>)> = rows
        .iter()
        .map(|r| {
            let lines = parse_diff(r.get(1))
                .1
                .into_iter()
                .map(|l| (l.kind.code(), l.text))
                .collect();
            (r.get(0), lines)
        })
        .collect();
    let mut distinct: Vec<(char, &str)> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for (_, lines) in &patches {
            for &line in lines {
                if seen.insert(line) {
                    distinct.push(line);
                }
                out.lines += 1;
            }
        }
    }
    let kinds: Vec<String> = distinct.iter().map(|(k, _)| k.to_string()).collect();
    let texts: Vec<&str> = distinct.iter().map(|(_, t)| *t).collect();
    tx.execute(
        "INSERT INTO patch_texts (kind, line) SELECT * FROM unnest($1::text[], $2::text[]) \
         ON CONFLICT (kind, md5(line)) DO NOTHING",
        &[&kinds, &texts],
    )
    .await?;
    let mut ids = vec![0i32; distinct.len()];
    for r in tx
        .query(
            "SELECT w.n, t.id, t.line = w.line FROM unnest($1::text[], $2::text[]) \
             WITH ORDINALITY AS w(kind, line, n) \
             JOIN patch_texts t ON t.kind = w.kind AND md5(t.line) = md5(w.line)",
            &[&kinds, &texts],
        )
        .await?
    {
        // Another line with the same MD5: never seen, but never wrong.
        if !r.get::<_, bool>(2) {
            bail!(
                "patch_texts {} collides with a new line's hash",
                r.get::<_, i32>(1)
            );
        }
        ids[(r.get::<_, i64>(0) - 1) as usize] = r.get(1);
    }
    let index: HashMap<(char, &str), i32> = distinct.iter().copied().zip(ids).collect();
    for (id, lines) in &patches {
        let ids: Vec<i32> = lines.iter().map(|line| index[line]).collect();
        if ids.contains(&0) {
            bail!("a line of patch {id} has no patch_texts row");
        }
        tx.execute(
            "UPDATE patches SET text_ids = $2 WHERE id = $1",
            &[id, &ids],
        )
        .await?;
        out.patches += 1;
    }
    tx.commit().await?;
    Ok(out)
}
