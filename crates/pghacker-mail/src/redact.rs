// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Email addresses out of the text pghacker-mcp serves. The archive's web
//! pages obfuscate addresses; mail is retrieved raw, so ingest replaces each
//! address in subjects, bodies and patches with `[email]`, and senders' names
//! lose theirs ([`crate::parse::display_name`]). Message-IDs in URLs
//! (`postgr.es/m/<id>`, `/message-id/<id>`), which look like addresses, stay.
//! `author_emails`, which matches senders to authors, keeps addresses, and
//! pghacker-mcp can't read it.
//!
//! Mail stored before ingest redacted is redacted in place, a batch of
//! messages at a time ([`redact_messages`]), by the indexer's
//! `redact_addresses` job (migration 0026).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use tokio_postgres::Transaction;
use tokio_postgres::types::Type;

use crate::{chunk, parse, series, store};

/// What replaces an address.
pub const REDACTED: &str = "[email]";

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+=-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});

/// `text` with each email address replaced by [`REDACTED`]; Message-IDs in
/// URLs (`/message-id/<id>`, `postgr.es/m/<id>`) and after `message-id:`
/// kept.
pub fn redact_emails(text: &str) -> Cow<'_, str> {
    EMAIL.replace_all(text, |caps: &regex::Captures<'_>| {
        let m = caps.get(0).unwrap();
        let before = &text[..m.start()];
        if before.ends_with('/')
            || before.ends_with("message-id:")
            || before.ends_with("Message-ID: <")
        {
            m.as_str().to_owned()
        } else {
            REDACTED.to_owned()
        }
    })
}

/// Whether `text` has an address [`redact_emails`] replaces.
pub fn has_email(text: &str) -> bool {
    matches!(redact_emails(text), Cow::Owned(_))
}

/// Messages a [`redact_messages`] batch reads.
pub const BATCH: i64 = 500;

/// What a batch of [`redact_messages`] did.
#[derive(Debug, Default, Serialize)]
pub struct Redacted {
    /// The last message read: the next batch starts after it.
    pub last_message: Option<i64>,
    /// Whether messages may be left after it.
    pub more: bool,
    /// Messages, patches and chunks changed, and senders renamed.
    pub messages: u64,
    pub patches: u64,
    pub chunked: u64,
    pub renamed: u64,
}

/// Redacts the stored messages after `after`, by id, `limit` of them: their
/// subjects, senders' names, bodies and patches. Changed chunks are made
/// again, for the embed job to embed; changed patches are indexed again by
/// the patch_lines job; a renamed sender's names are counted again; threads
/// take their roots' new subjects. Running it again changes nothing.
pub async fn redact_messages(tx: &Transaction<'_>, after: i64, limit: i64) -> Result<Redacted> {
    // No key changes: extractions citing the messages don't wait on them.
    let rows = tx
        .query_typed(
            "SELECT id, thread_id, author_id, subject, from_name, body, body_clean
             FROM messages WHERE id > $1 ORDER BY id LIMIT $2 FOR NO KEY UPDATE",
            &[(&after, Type::INT8), (&limit, Type::INT8)],
        )
        .await?;
    let mut done = Redacted {
        last_message: rows.last().map(|r| r.get(0)),
        more: rows.len() as i64 == limit,
        ..Redacted::default()
    };
    let mut retitled = BTreeSet::new();
    let mut reseries = BTreeSet::new();
    for row in &rows {
        let (id, thread_id, author_id): (i64, i64, Option<i64>) =
            (row.get(0), row.get(1), row.get(2));
        let (subject, from_name, body, body_clean): (String, String, String, String) =
            (row.get(3), row.get(4), row.get(5), row.get(6));
        let new_subject = redact_emails(&subject).into_owned();
        let new_name = redact_emails(&parse::display_name(&from_name, None)).into_owned();
        let new_body = redact_emails(&body).into_owned();
        let new_clean = redact_emails(&body_clean).into_owned();
        let changed = new_subject != subject
            || new_name != from_name
            || new_body != body
            || new_clean != body_clean;
        if changed {
            tx.execute_typed(
                "UPDATE messages SET subject = $2, from_name = $3, body = $4, body_clean = $5
                 WHERE id = $1",
                &[
                    (&id, Type::INT8),
                    (&new_subject, Type::TEXT),
                    (&new_name, Type::TEXT),
                    (&new_body, Type::TEXT),
                    (&new_clean, Type::TEXT),
                ],
            )
            .await?;
            done.messages += 1;
        }
        if new_subject != subject {
            retitled.insert(thread_id);
            reseries.insert(thread_id);
        }

        let patches = tx
            .query_typed(
                "SELECT id, filename, content FROM patches WHERE message_id = $1",
                &[(&id, Type::INT8)],
            )
            .await?;
        for patch in patches {
            let (patch_id, filename, content): (i64, String, String) =
                (patch.get(0), patch.get(1), patch.get(2));
            let new_filename = redact_emails(&filename);
            let new_content = redact_emails(&content);
            if matches!(new_filename, Cow::Owned(_)) || matches!(new_content, Cow::Owned(_)) {
                // Indexed again by the patch_lines job.
                tx.execute_typed(
                    "UPDATE patches SET filename = $2, content = $3, text_ids = NULL WHERE id = $1",
                    &[
                        (&patch_id, Type::INT8),
                        (&new_filename.as_ref(), Type::TEXT),
                        (&new_content.as_ref(), Type::TEXT),
                    ],
                )
                .await?;
                done.patches += 1;
                if matches!(new_filename, Cow::Owned(_)) {
                    reseries.insert(thread_id);
                }
            }
        }

        // The chunks follow the subject and the cleaned body; the embed job
        // embeds new ones.
        if new_subject != subject || new_clean != body_clean {
            let chunks = chunk::chunk(&new_subject, &new_clean);
            let stored: Vec<String> = tx
                .query_typed(
                    "SELECT content FROM message_chunks WHERE message_id = $1 ORDER BY ord",
                    &[(&id, Type::INT8)],
                )
                .await?
                .into_iter()
                .map(|r| r.get(0))
                .collect();
            if chunks != stored {
                let ords: Vec<i32> = (0..chunks.len() as i32).collect();
                tx.execute_typed(
                    "DELETE FROM message_chunks WHERE message_id = $1",
                    &[(&id, Type::INT8)],
                )
                .await?;
                tx.execute_typed(
                    "INSERT INTO message_chunks (message_id, thread_id, ord, content)
                     SELECT $1, $2, ord, content
                     FROM unnest($3::int4[], $4::text[]) AS c (ord, content)",
                    &[
                        (&id, Type::INT8),
                        (&thread_id, Type::INT8),
                        (&ords, Type::INT4_ARRAY),
                        (&chunks, Type::TEXT_ARRAY),
                    ],
                )
                .await?;
                done.chunked += 1;
            }
        }
        if new_name != from_name
            && let Some(author_id) = author_id
        {
            rename_author(tx, author_id, &from_name, &new_name).await?;
            done.renamed += 1;
        }
    }
    if !reseries.is_empty() {
        let threads: Vec<i64> = reseries.into_iter().collect();
        series::refresh(tx, &threads).await?;
    }
    if !retitled.is_empty() {
        let threads: Vec<i64> = retitled.into_iter().collect();
        store::refresh_threads(tx, &threads).await?;
    }
    Ok(done)
}

/// Counts a message of `author_id` under its sender's name `to` rather than
/// `from`, and makes the most frequent name the display name. The names
/// decide who committed a commit in the graph (COMMITTED), so the graph sync
/// is told that the author changed.
async fn rename_author(tx: &Transaction<'_>, author_id: i64, from: &str, to: &str) -> Result<()> {
    tx.execute_typed(
        "UPDATE author_names SET count = count - 1 WHERE author_id = $1 AND name = $2",
        &[(&author_id, Type::INT8), (&from, Type::TEXT)],
    )
    .await?;
    tx.execute_typed(
        "DELETE FROM author_names WHERE author_id = $1 AND name = $2 AND count <= 0",
        &[(&author_id, Type::INT8), (&from, Type::TEXT)],
    )
    .await?;
    tx.execute_typed(
        "INSERT INTO author_names (author_id, name, count) VALUES ($1, $2, 1)
         ON CONFLICT (author_id, name) DO UPDATE SET count = author_names.count + 1",
        &[(&author_id, Type::INT8), (&to, Type::TEXT)],
    )
    .await?;
    tx.execute_typed(
        "UPDATE authors a
         SET display_name = coalesce(
                 (SELECT n.name FROM author_names n WHERE n.author_id = a.id
                  ORDER BY n.count DESC, (n.name = a.display_name) DESC, n.name LIMIT 1),
                 a.display_name),
             updated_at = now()
         WHERE a.id = $1",
        &[(&author_id, Type::INT8)],
    )
    .await?;
    tx.execute_typed(
        "INSERT INTO graph_changes (key) VALUES ('author:' || $1::text)",
        &[(&author_id, Type::INT8)],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_addresses_but_not_message_id_urls() {
        let text =
            "Tom Lane <tom@example.org> wrote. See https://postgr.es/m/CA+abc=d@mail.gmail.com";
        assert_eq!(
            redact_emails(text),
            "Tom Lane <[email]> wrote. See https://postgr.es/m/CA+abc=d@mail.gmail.com"
        );
        assert_eq!(
            redact_emails("https://www.postgresql.org/message-id/20190101.1@example.org"),
            "https://www.postgresql.org/message-id/20190101.1@example.org"
        );
        assert_eq!(
            redact_emails("Reported-by: Ann <ann.oneil+pg@mail.example.co.uk>"),
            "Reported-by: Ann <[email]>"
        );
        assert_eq!(
            redact_emails("Signed-off-by: x@y.org"),
            "Signed-off-by: [email]"
        );
    }

    #[test]
    fn leaves_text_without_addresses_borrowed() {
        assert!(matches!(
            redact_emails("a @ b, x@y, @home"),
            Cow::Borrowed(_)
        ));
        assert!(!has_email("ping @tom on x@localhost"));
        assert!(has_email("mail me at me@example.org"));
    }
}
