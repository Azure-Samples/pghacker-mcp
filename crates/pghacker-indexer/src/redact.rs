// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Redacting the email addresses of mail stored before ingest redacted them
//! ([`pghacker_mail::redact`]): the `redact_addresses` job, which migration
//! 0026 queues once (key `redact`). It runs in phases, recording where it is
//! in its payload, so a restart carries on:
//!
//! 1. `messages`: a batch of messages at a time, their subjects, senders'
//!    names, bodies and patches. Changed chunks are embedded again and
//!    changed patches indexed again; the graph follows the renamed authors
//!    and retitled threads.
//! 2. `extracts`: authors' names; features', decisions' and positions' text,
//!    whose embeddings are cleared to be embedded again; citations' quotes,
//!    which stay verbatim quotes of the redacted messages. The threads they
//!    come from get a new content version, to be extracted again.
//! 3. `patch_texts`: once every changed patch is indexed again, the lines with
//!    addresses that no patch uses any more are deleted.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use pghacker_mail::parse::display_name;
use pghacker_mail::redact::{self, redact_emails};
use serde_json::{Value, json};
use tokio_postgres::Transaction;
use tokio_postgres::types::Type;

use crate::Indexer;
use crate::jobs::{Job, Outcome};

/// patch_texts lines a run of the last phase reads.
const PATCH_TEXTS_BATCH: i64 = 5000;
/// How long the last phase waits for the patch_lines job to index changed
/// patches.
const PATCH_LINES_WAIT: Duration = Duration::from_secs(60);

/// Runs the job's next step.
pub async fn run(indexer: &Indexer, job: &Job) -> Result<Outcome> {
    let mut state = job.payload.clone();
    if !state.is_object() {
        state = json!({});
    }
    let phase = state
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("messages")
        .to_owned();
    let after = state.get("after").and_then(Value::as_i64).unwrap_or(0);
    let add = |state: &mut Value, name: &str, n: u64| {
        let total = state.get(name).and_then(Value::as_u64).unwrap_or(0) + n;
        state[name] = json!(total);
    };
    let mut client = indexer.pool.get().await?;
    let next = match phase.as_str() {
        "messages" => {
            let tx = client.transaction().await?;
            let done = redact::redact_messages(&tx, after, redact::BATCH).await?;
            tx.commit().await?;
            add(&mut state, "messages", done.messages);
            add(&mut state, "patches", done.patches);
            add(&mut state, "chunked", done.chunked);
            add(&mut state, "renamed", done.renamed);
            match (done.more, done.last_message) {
                (true, Some(last)) => ("messages", last),
                _ => ("extracts", 0),
            }
        }
        "extracts" => {
            let tx = client.transaction().await?;
            let done = redact_extracts(&tx).await?;
            tx.commit().await?;
            for (name, n) in [
                ("authors", done.authors),
                ("extracts", done.extracts),
                ("citations", done.citations),
                ("threads", done.threads),
            ] {
                add(&mut state, name, n);
            }
            ("patch_texts", 0)
        }
        _ => {
            let pending: bool = client
                .query_one(
                    "SELECT EXISTS (SELECT FROM patches WHERE text_ids IS NULL)",
                    &[],
                )
                .await?
                .get(0);
            if pending {
                return Ok(Outcome::Again {
                    after: PATCH_LINES_WAIT,
                    result: state,
                });
            }
            let (deleted, last) = purge_patch_texts(&**client, after).await?;
            add(&mut state, "patch_texts", deleted);
            match last {
                Some(last) => ("patch_texts", last),
                None => {
                    state["phase"] = json!("done");
                    return Ok(Outcome::Done(state));
                }
            }
        }
    };
    state["phase"] = json!(next.0);
    state["after"] = json!(next.1);
    client
        .execute(
            "UPDATE jobs SET payload = $2 WHERE id = $1",
            &[&job.id, &state],
        )
        .await?;
    Ok(Outcome::Again {
        after: Duration::ZERO,
        result: state,
    })
}

/// What [`redact_extracts`] changed.
#[derive(Debug, Default)]
pub struct Extracts {
    pub authors: u64,
    pub extracts: u64,
    pub citations: u64,
    /// Threads given a new content version.
    pub threads: u64,
}

/// Redacts what the messages' redaction leaves: authors' names, and the
/// extracts' text and quotes. Changed extracts are embedded again, and the
/// threads they come from extracted again.
pub async fn redact_extracts(tx: &Transaction<'_>) -> Result<Extracts> {
    let mut done = Extracts::default();
    let name = |text: &str| redact_emails(&display_name(text, None)).into_owned();

    // Names no stored message carries any more, such as merged authors'.
    let names = tx
        .query(
            "SELECT author_id, name, count FROM author_names WHERE name LIKE '%@%' FOR UPDATE",
            &[],
        )
        .await?;
    for row in names {
        let (author, old, count): (i64, String, i32) = (row.get(0), row.get(1), row.get(2));
        let new = name(&old);
        if new == old {
            continue;
        }
        tx.execute(
            "DELETE FROM author_names WHERE author_id = $1 AND name = $2",
            &[&author, &old],
        )
        .await?;
        tx.execute(
            "INSERT INTO author_names (author_id, name, count) VALUES ($1, $2, $3)
             ON CONFLICT (author_id, name) DO UPDATE SET count = author_names.count + EXCLUDED.count",
            &[&author, &new, &count],
        )
        .await?;
        // Committers are matched by name (COMMITTED).
        tx.execute(
            "INSERT INTO graph_changes (key) VALUES ('author:' || $1::bigint)",
            &[&author],
        )
        .await?;
    }
    let authors = tx
        .query(
            "SELECT id, display_name FROM authors WHERE display_name LIKE '%@%' FOR UPDATE",
            &[],
        )
        .await?;
    for row in authors {
        let (id, old): (i64, String) = (row.get(0), row.get(1));
        let new = name(&old);
        if new != old {
            tx.execute(
                "UPDATE authors SET display_name = $2, updated_at = now() WHERE id = $1",
                &[&id, &new],
            )
            .await?;
            done.authors += 1;
        }
    }

    let mut threads = BTreeSet::new();
    let redact_all = |fields: &[String]| -> Option<Vec<String>> {
        let new: Vec<String> = fields
            .iter()
            .map(|f| redact_emails(f).into_owned())
            .collect();
        (new != fields).then_some(new)
    };
    let features = tx
        .query(
            "SELECT id, name, description FROM features
             WHERE name LIKE '%@%' OR description LIKE '%@%' FOR UPDATE",
            &[],
        )
        .await?;
    for row in features {
        let id: i64 = row.get(0);
        if let Some(new) = redact_all(&[row.get(1), row.get(2)]) {
            tx.execute(
                "UPDATE features SET name = $2, description = $3, embedding = NULL, updated_at = now()
                 WHERE id = $1",
                &[&id, &new[0], &new[1]],
            )
            .await?;
            done.extracts += 1;
            for t in tx
                .query(
                    "SELECT thread_id FROM feature_threads WHERE feature_id = $1",
                    &[&id],
                )
                .await?
            {
                threads.insert(t.get::<_, i64>(0));
            }
        }
    }
    let decisions = tx
        .query(
            "SELECT id, thread_id, title, outcome, alternatives, rationale FROM decisions
             WHERE title LIKE '%@%' OR outcome LIKE '%@%' OR alternatives LIKE '%@%'
                OR rationale LIKE '%@%' FOR UPDATE",
            &[],
        )
        .await?;
    for row in decisions {
        let (id, thread): (i64, i64) = (row.get(0), row.get(1));
        if let Some(new) = redact_all(&[row.get(2), row.get(3), row.get(4), row.get(5)]) {
            tx.execute(
                "UPDATE decisions SET title = $2, outcome = $3, alternatives = $4, rationale = $5,
                     embedding = NULL, updated_at = now()
                 WHERE id = $1",
                &[&id, &new[0], &new[1], &new[2], &new[3]],
            )
            .await?;
            done.extracts += 1;
            threads.insert(thread);
        }
    }
    let positions = tx
        .query(
            "SELECT id, thread_id, summary FROM positions WHERE summary LIKE '%@%' FOR UPDATE",
            &[],
        )
        .await?;
    for row in positions {
        let (id, thread): (i64, i64) = (row.get(0), row.get(1));
        if let Some(new) = redact_all(&[row.get(2)]) {
            tx.execute(
                "UPDATE positions SET summary = $2, embedding = NULL WHERE id = $1",
                &[&id, &new[0]],
            )
            .await?;
            done.extracts += 1;
            threads.insert(thread);
        }
    }
    let citations = tx
        .query(
            "SELECT c.id, m.thread_id, c.quote FROM citations c JOIN messages m ON m.id = c.message_id
             WHERE c.quote LIKE '%@%' FOR UPDATE OF c",
            &[],
        )
        .await?;
    for row in citations {
        let (id, thread, quote): (i64, i64, String) = (row.get(0), row.get(1), row.get(2));
        let new = redact_emails(&quote);
        if new != quote {
            // The same quote, redacted, may already cite the message for the
            // extract: then this one goes.
            let updated = tx
                .execute(
                    "UPDATE citations c SET quote = $2 WHERE c.id = $1
                       AND NOT EXISTS (
                           SELECT FROM citations d
                           WHERE (d.extract_kind, d.extract_id, d.message_id) =
                                 (c.extract_kind, c.extract_id, c.message_id)
                             AND d.quote = $2)",
                    &[&id, &new.as_ref()],
                )
                .await?;
            if updated == 0 {
                tx.execute("DELETE FROM citations WHERE id = $1", &[&id])
                    .await?;
            }
            done.citations += 1;
            threads.insert(thread);
        }
    }

    let threads: Vec<i64> = threads.into_iter().collect();
    done.threads = tx
        .execute_typed(
            "UPDATE threads SET content_version = content_version + 1, updated_at = now()
             WHERE id = ANY($1) AND extracted_version IS NOT NULL",
            &[(&threads, Type::INT8_ARRAY)],
        )
        .await?;
    Ok(done)
}

/// Deletes the patch_texts lines after `after` with addresses that no patch
/// uses, a batch at a time; returns how many, and the last line read if more
/// may be left.
async fn purge_patch_texts(
    client: &impl tokio_postgres::GenericClient,
    after: i64,
) -> Result<(u64, Option<i64>)> {
    let rows = client
        .query(
            "SELECT id, line FROM patch_texts WHERE id > $1 AND line LIKE '%@%' ORDER BY id LIMIT $2",
            &[&(after as i32), &PATCH_TEXTS_BATCH],
        )
        .await?;
    let last = rows.last().map(|r| i64::from(r.get::<_, i32>(0)));
    let ids: Vec<i32> = rows
        .iter()
        .filter(|r| redact::has_email(r.get(1)))
        .map(|r| r.get(0))
        .collect();
    // Ingest stores no line with an address any more, so no patch starts
    // using these meanwhile.
    let deleted = client
        .execute(
            "DELETE FROM patch_texts t WHERE t.id = ANY($1)
               AND NOT EXISTS (SELECT FROM patches p WHERE p.text_ids @> ARRAY[t.id])",
            &[&ids],
        )
        .await?;
    let more = rows.len() as i64 == PATCH_TEXTS_BATCH;
    Ok((deleted, if more { last } else { None }))
}
