//! Links from commits to the pgsql-hackers threads that discussed them: the
//! threads whose messages the commit's Discussion trailers name, and the
//! threads whose messages cite the commit by SHA (mail's commit anchors).
//! Mail keeps arriving and cites old commits, so the links are recomputed
//! whole after each build and sync, and the ones that changed are written;
//! without mail tables there are none.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use tokio_postgres::GenericClient;

use pghacker_core::text::discussion_message_id;

/// The shortest abbreviated SHA that a citation can name a commit by.
const MIN_PREFIX: usize = 7;

/// What a refresh linked.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Linked {
    pub by_trailer: u64,
    pub by_anchor: u64,
}

/// Recomputes the commits' thread links in `schema`.
pub async fn refresh(client: &impl GenericClient, schema: &str) -> Result<Linked> {
    let s = super::store::readable_schema(schema)?;
    // (sha, thread, via, message)
    let mut rows: Vec<(String, i64, &'static str, i64)> = Vec::new();
    let has_mail: bool = client
        .query_one(
            "SELECT to_regclass('pghacker.messages') IS NOT NULL \
                AND to_regclass('pghacker.anchors') IS NOT NULL",
            &[],
        )
        .await?
        .get(0);
    if has_mail {
        // Discussion trailers.
        let mut named: HashMap<String, Vec<String>> = HashMap::new();
        for r in client
            .query(
                &format!(
                    "SELECT c.sha, d.value FROM {s}.code_commits c, \
                     jsonb_array_elements_text(c.trailers -> 'Discussion') d(value) \
                     WHERE c.trailers ? 'Discussion'"
                ),
                &[],
            )
            .await?
        {
            if let Some(id) = discussion_message_id(r.get(1)) {
                named.entry(id).or_default().push(r.get(0));
            }
        }
        let ids: Vec<&str> = named.keys().map(String::as_str).collect();
        for m in client
            .query(
                "SELECT message_id, thread_id, id FROM pghacker.messages \
                 WHERE message_id = ANY($1)",
                &[&ids],
            )
            .await?
        {
            for sha in &named[m.get::<_, &str>(0)] {
                rows.push((sha.clone(), m.get(1), "trailer", m.get(2)));
            }
        }

        // Messages citing a commit: an abbreviated SHA naming exactly one
        // commit made before the message (a day's leeway for clocks).
        let mut commits: Vec<(String, DateTime<Utc>)> = client
            .query(
                &format!("SELECT sha, committed_at FROM {s}.code_commits"),
                &[],
            )
            .await?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        commits.sort_unstable();
        for a in client
            .query(
                "SELECT lower(a.value), m.thread_id, m.id, m.sent_at FROM pghacker.anchors a \
                 JOIN pghacker.messages m ON m.id = a.message_id WHERE a.kind = 'commit'",
                &[],
            )
            .await?
        {
            let prefix: String = a.get(0);
            if prefix.len() < MIN_PREFIX || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let start = commits.partition_point(|(sha, _)| sha.as_str() < prefix.as_str());
            let mut matches = commits[start..]
                .iter()
                .take_while(|(sha, _)| sha.starts_with(&prefix));
            let (Some((sha, at)), None) = (matches.next(), matches.next()) else {
                continue;
            };
            let sent: Option<DateTime<Utc>> = a.get(3);
            if sent.is_some_and(|sent| sent + TimeDelta::days(1) < *at) {
                continue;
            }
            rows.push((sha.clone(), a.get(1), "anchor", a.get(2)));
        }
    }

    // A message can cite a commit twice, by a prefix and in full, and a
    // commit can name a discussion twice. A build's tables have no keys yet
    // for ON CONFLICT to skip the second row, so drop it here.
    rows.sort_unstable();
    rows.dedup();
    let linked = Linked {
        by_trailer: rows.iter().filter(|r| r.2 == "trailer").count() as u64,
        by_anchor: rows.iter().filter(|r| r.2 == "anchor").count() as u64,
    };
    let shas: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let threads: Vec<i64> = rows.iter().map(|r| r.1).collect();
    let via: Vec<&str> = rows.iter().map(|r| r.2).collect();
    let messages: Vec<i64> = rows.iter().map(|r| r.3).collect();
    // Only the links that changed are written; in pghacker, they're recorded
    // for the graph too (DISCUSSED_IN).
    let changes = if super::store::captured(s) {
        "INSERT INTO pghacker.graph_changes (label, key, end_key) \
         SELECT DISTINCT e.* FROM (SELECT * FROM gone UNION SELECT * FROM added) x \
         CROSS JOIN LATERAL pghacker.graph_commit_thread_entries(x.sha, x.thread_id) e"
    } else {
        "SELECT"
    };
    client
        .execute(
            &format!(
                "WITH new AS (
                     SELECT * FROM unnest($1::text[], $2::int8[], $3::text[], $4::int8[])
                         AS n (sha, thread_id, via, message_id)
                 ), gone AS (
                     DELETE FROM {s}.code_commit_threads t
                     WHERE NOT EXISTS (SELECT FROM new n WHERE n.sha = t.sha AND n.thread_id = t.thread_id
                                                          AND n.via = t.via AND n.message_id = t.message_id)
                     RETURNING t.sha, t.thread_id
                 ), added AS (
                     INSERT INTO {s}.code_commit_threads (sha, thread_id, via, message_id)
                     SELECT * FROM new ON CONFLICT DO NOTHING
                     RETURNING sha, thread_id
                 )
                 {changes}"
            ),
            &[&shas, &threads, &via, &messages],
        )
        .await?;
    Ok(linked)
}
