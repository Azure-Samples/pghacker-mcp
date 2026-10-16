// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Resolves anchors against the commit history that the code tools read: a
//! path mentioned in mail becomes a virtual path the tools read
//! (`/branch/<newest release line>/<path>`, else `/branch/master/<path>`,
//! else `/commit/<sha>/<path>` where its newest version was made), and an
//! abbreviated commit SHA the commit's full SHA.

use anyhow::Result;
use tokio_postgres::GenericClient;

/// The shortest abbreviated SHA that names a commit.
const MIN_PREFIX: usize = 7;

/// Resolves the anchors that the code now covers; returns how many.
pub async fn resolve(client: &impl GenericClient) -> Result<u64> {
    resolve_in_history(client, pghacker_core::code::history_schema()).await
}

/// Resolves file and commit anchors to the commit history in `schema`.
pub async fn resolve_in_history(client: &impl GenericClient, schema: &str) -> Result<u64> {
    let s = pghacker_code::history::store::readable_schema(schema)?;
    let alive =
        |at: &str| format!("v.lo <= {at} AND v.hi > {at} AND (v.alive IS NULL OR v.alive @> {at})");
    let files = client
        .execute(
            &format!(
                "WITH release AS (
                     SELECT name, addr FROM {s}.code_refs
                     WHERE kind = 'branch' AND name ~ '^REL_?[0-9]+(_[0-9]+)?_STABLE$'
                     ORDER BY (regexp_match(name, '^REL_?([0-9]+)'))[1]::int DESC,
                              coalesce((regexp_match(name, '^REL_?[0-9]+_([0-9]+)_'))[1]::int, 0) DESC
                     LIMIT 1),
                 master AS (
                     SELECT name, addr FROM {s}.code_refs
                     WHERE kind = 'branch' AND name = (SELECT master FROM {s}.code_history)),
                 r AS (
                     SELECT DISTINCT ON (v.path) v.path,
                            CASE WHEN rel.addr IS NOT NULL AND {on_release}
                                     THEN '/branch/' || rel.name || '/' || v.path
                                 WHEN m.addr IS NOT NULL AND {on_master}
                                     THEN '/branch/' || m.name || '/' || v.path
                                 ELSE '/commit/' || c.sha || '/' || v.path END AS resolved
                     FROM {s}.code_file_versions v
                     JOIN {s}.code_commits c ON c.addr = v.lo
                     LEFT JOIN release rel ON true LEFT JOIN master m ON true
                     WHERE v.path IN (SELECT value FROM anchors WHERE kind = 'file')
                     ORDER BY v.path,
                              (rel.addr IS NOT NULL AND {on_release}) DESC,
                              (m.addr IS NOT NULL AND {on_master}) DESC,
                              c.committed_at DESC, v.lo DESC)
             UPDATE anchors a SET resolved = r.resolved FROM r
             WHERE a.kind = 'file' AND a.value = r.path
               AND a.resolved IS DISTINCT FROM r.resolved",
                on_release = alive("rel.addr"),
                on_master = alive("m.addr"),
            ),
            &[],
        )
        .await?;

    // Abbreviated commit SHAs that name exactly one commit.
    let mut shas: Vec<String> = client
        .query(&format!("SELECT sha FROM {s}.code_commits"), &[])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    shas.sort_unstable();
    let mut ids: Vec<i64> = Vec::new();
    let mut full: Vec<String> = Vec::new();
    for a in client
        .query(
            "SELECT id, lower(value) FROM anchors WHERE kind = 'commit'",
            &[],
        )
        .await?
    {
        let prefix: String = a.get(1);
        if prefix.len() < MIN_PREFIX || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let start = shas.partition_point(|sha| sha.as_str() < prefix.as_str());
        let mut matches = shas[start..]
            .iter()
            .take_while(|sha| sha.starts_with(&prefix));
        if let (Some(sha), None) = (matches.next(), matches.next()) {
            ids.push(a.get(0));
            full.push(sha.clone());
        }
    }
    let commits = client
        .execute(
            "UPDATE anchors a SET resolved = u.sha FROM unnest($1::int8[], $2::text[]) AS u(id, sha)
             WHERE a.id = u.id AND a.resolved IS DISTINCT FROM u.sha",
            &[&ids, &full],
        )
        .await?;
    Ok(files + commits)
}
