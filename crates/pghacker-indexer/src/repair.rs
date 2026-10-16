// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Repairs derived counts that a failed refresh left stale: concurrent
//! ingests can deadlock a thread's refresh, which the next ingest of that
//! thread would otherwise be the only one to fix.

use anyhow::Result;
use tokio_postgres::GenericClient;

/// Recomputes threads' and authors' counts and dates where they differ from
/// the messages; returns how many rows changed.
pub async fn thread_and_author_stats(client: &impl GenericClient) -> Result<u64> {
    let threads = client
        .execute(
            "UPDATE threads t SET message_count = s.n, participant_count = s.p, started_at = s.first,
                 last_at = s.last, updated_at = now()
             FROM (SELECT thread_id, count(*)::int AS n, count(DISTINCT author_id)::int AS p,
                          min(sent_at) AS first, max(sent_at) AS last
                   FROM messages GROUP BY thread_id) s
             WHERE s.thread_id = t.id
               AND (t.message_count, t.participant_count, t.started_at, t.last_at)
                   IS DISTINCT FROM (s.n, s.p, s.first, s.last)",
            &[],
        )
        .await?;
    let authors = client
        .execute(
            "UPDATE authors a SET message_count = coalesce(s.n, 0), first_seen = s.first, last_seen = s.last,
                 updated_at = now()
             FROM authors x LEFT JOIN (
                 SELECT author_id, count(*)::int AS n, min(sent_at) AS first, max(sent_at) AS last
                 FROM messages GROUP BY author_id) s ON s.author_id = x.id
             WHERE a.id = x.id AND a.merged_into IS NULL
               AND (a.message_count, a.first_seen, a.last_seen) IS DISTINCT FROM (coalesce(s.n, 0), s.first, s.last)",
            &[],
        )
        .await?;
    if threads + authors > 0 {
        tracing::info!(threads, authors, "repaired stale counts");
    }
    Ok(threads + authors)
}
