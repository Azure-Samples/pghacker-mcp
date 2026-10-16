// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Writing parsed messages in the caller's transaction: threading, authors,
//! patches, anchors and chunks, idempotently.
//!
//! Threads keep a stable identity, their root Message-ID, whichever order
//! messages arrive in (months out of order, mbox and thread pages mixed):
//!
//! 1. A message joins the thread of its closest stored ancestor (In-Reply-To,
//!    then References from the newest).
//! 2. Else the thread rooted at its first References entry (or In-Reply-To),
//!    else the thread of a message that shares an ancestor with it (stored or
//!    not) or replies to it, else one rooted at another of its ancestors,
//!    else the thread its own replies started (rooted at its own Message-ID).
//! 3. Else, for a reply without threading headers, a thread with the same
//!    distinctive subject key active within 60 days of it.
//! 4. Else a new thread rooted at its first References entry, else
//!    In-Reply-To, else itself. Threads that headerless replies with the same
//!    subject key started before it existed merge into it.
//!
//! Other threads the message connects (replies that arrived before it under
//! a later root) merge into the chosen one, unless extracts already cite
//! them, and a thread rooted at a later message of the chain takes the
//! earlier root.
//!
//! Authors match by lowercased address in `author_emails`. Text is stored as
//! written, email addresses included; mail stored before ingest kept them
//! has `[email]` in their place until [`restore_addresses`] gives them back.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use pghacker_core::text::redact_emails;
use tokio_postgres::Transaction;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;

use crate::anchors::{self, AnchorRef};
use crate::parse::{
    ParsedMessage, display_name, display_name_without_addresses, normalize_message_id, sanitize,
};
use crate::patch::{self, PatchFile};
use crate::{chunk, clean, series, subject};

/// Where messages come from.
#[derive(Clone, Debug)]
pub struct StoreContext {
    pub list: String,
    pub source_id: Option<i64>,
    /// YYYY-MM of the monthly mbox, if the message came from one.
    pub archive_month: Option<String>,
}

/// What [`upsert_message`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
    pub message_row_id: i64,
    pub thread_id: i64,
    /// False if the message was already stored; nothing changed then.
    pub inserted: bool,
}

/// How long a thread stays open to replies that lost their headers.
const SUBJECT_WINDOW_DAYS: i32 = 60;

/// Stores a message with its thread, author, patches, anchors and chunks,
/// or returns the stored one. On error, nothing of the message is written
/// and `tx` stays usable.
pub async fn upsert_message(
    tx: &Transaction<'_>,
    ctx: &StoreContext,
    msg: &ParsedMessage,
) -> Result<Stored> {
    let message_id = normalize_message_id(&msg.message_id);
    ensure!(!message_id.is_empty(), "the message has no Message-ID");
    if let Some(stored) = existing(tx, &message_id).await? {
        return Ok(stored);
    }
    let derived = Derived::new(msg);
    tx.batch_execute("SAVEPOINT pghacker_upsert_message")
        .await?;
    match insert(tx, ctx, msg, &message_id, &derived).await {
        Ok(Some(stored)) => {
            tx.batch_execute("RELEASE SAVEPOINT pghacker_upsert_message")
                .await?;
            Ok(stored)
        }
        Ok(None) => {
            // A concurrent transaction stored it first.
            rollback_to(tx, "pghacker_upsert_message").await?;
            existing(tx, &message_id)
                .await?
                .ok_or_else(|| anyhow!("message {message_id} conflicted but is not stored"))
        }
        Err(error) => match rollback_to(tx, "pghacker_upsert_message").await {
            Ok(()) => Err(error),
            Err(rollback) => Err(error.context(format!("and cannot roll back: {rollback}"))),
        },
    }
}

/// Recomputes the threads' message and participant counts, first and last
/// dates and subject (the root's if stored, else the earliest message's),
/// and bumps `content_version` if the message count changed.
pub async fn refresh_threads(tx: &Transaction<'_>, thread_ids: &[i64]) -> Result<()> {
    if thread_ids.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = thread_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // Lock in id order: concurrent refreshes then wait instead of deadlocking.
    // No key changes, so the lock leaves foreign-key checks free: a
    // transaction that stored or restored a message of the thread holds a
    // KEY SHARE lock on it from inserting the message or its chunks, and two
    // such transactions refreshing the thread would otherwise wait on each
    // other's. Inserts' count updates still wait on this lock.
    tx.execute_typed(
        "SELECT FROM threads WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE",
        &[(&ids, Type::INT8_ARRAY)],
    )
    .await?;
    let rows = tx
        .query_typed(
            "SELECT DISTINCT ON (m.thread_id) m.thread_id, m.subject
             FROM messages m JOIN threads t ON t.id = m.thread_id
             WHERE m.thread_id = ANY($1)
             ORDER BY m.thread_id, (m.message_id = t.root_message_id) DESC, m.sent_at NULLS LAST, m.id",
            &[(&ids, Type::INT8_ARRAY)],
        )
        .await?;
    let mut subject_ids = Vec::with_capacity(rows.len());
    let mut subjects = Vec::with_capacity(rows.len());
    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        let text: String = row.get(1);
        subject_ids.push(row.get::<_, i64>(0));
        subjects.push(subject::display(&text));
        keys.push(subject::key(&text));
    }
    tx.execute_typed(
        "WITH s AS (
             SELECT thread_id, count(*)::int AS n, count(DISTINCT author_id)::int AS p,
                    min(sent_at) AS first_at, max(sent_at) AS last_at
             FROM messages WHERE thread_id = ANY($1) GROUP BY thread_id
         ), subj AS (
             SELECT * FROM unnest($2::int8[], $3::text[], $4::text[]) AS x (id, subject, key)
         )
         UPDATE threads t
         SET message_count = s.n,
             participant_count = s.p,
             started_at = s.first_at,
             last_at = s.last_at,
             subject = COALESCE(subj.subject, t.subject),
             subject_key = COALESCE(subj.key, t.subject_key),
             content_version = t.content_version + CASE WHEN s.n <> t.message_count THEN 1 ELSE 0 END,
             updated_at = now()
         FROM s LEFT JOIN subj ON subj.id = s.thread_id
         WHERE t.id = s.thread_id
           AND (t.message_count, t.participant_count, t.started_at, t.last_at, t.subject, t.subject_key)
               IS DISTINCT FROM
               (s.n, s.p, s.first_at, s.last_at, COALESCE(subj.subject, t.subject),
                COALESCE(subj.key, t.subject_key))",
        &[
            (&ids, Type::INT8_ARRAY),
            (&subject_ids, Type::INT8_ARRAY),
            (&subjects, Type::TEXT_ARRAY),
            (&keys, Type::TEXT_ARRAY),
        ],
    )
    .await?;
    Ok(())
}

/// What is stored for a message besides its headers: its text as written,
/// patches, anchors and chunks.
struct Derived {
    subject: String,
    from_name: String,
    body: String,
    body_clean: String,
    patches: Vec<PatchFile>,
    anchors: Vec<AnchorRef>,
    chunks: Vec<String>,
}

impl Derived {
    fn new(msg: &ParsedMessage) -> Self {
        let clean = clean::clean_body(&msg.body);
        let patches = patch::extract(msg);
        let anchors = anchors::extract_at(&msg.subject, &clean, &patches, msg.sent_at);
        let subject = sanitize(&msg.subject);
        let body_clean = sanitize(&clean);
        let chunks = chunk::chunk(&subject, &body_clean);
        let patches = patches
            .into_iter()
            .map(|patch| PatchFile {
                filename: sanitize(&patch.filename),
                content: sanitize(&patch.content),
                ..patch
            })
            .collect();
        Self {
            subject,
            from_name: sanitize(&display_name(&msg.from_name, msg.from_email.as_deref())),
            body: sanitize(&msg.body),
            body_clean,
            patches,
            anchors,
            chunks,
        }
    }

    /// The text ingest once stored for the message, before it kept email
    /// addresses: each replaced by `[email]`, and the sender's name without
    /// them.
    fn once(&self, email: Option<&str>) -> Once {
        let redact = |text: &str| redact_emails(text).into_owned();
        Once {
            subject: redact(&self.subject),
            from_name: redact(&display_name_without_addresses(&self.from_name, email)),
            body: redact(&self.body),
            body_clean: redact(&self.body_clean),
            patches: self
                .patches
                .iter()
                .map(|p| (redact(&p.filename), redact(&p.content)))
                .collect(),
        }
    }
}

/// A message's text as ingest once stored it ([`Derived::once`]).
struct Once {
    subject: String,
    from_name: String,
    body: String,
    body_clean: String,
    /// Each patch's filename and content.
    patches: Vec<(String, String)>,
}

impl Once {
    /// Whether the message had addresses that ingest once left out.
    fn differs_from(&self, now: &Derived) -> bool {
        self.subject != now.subject
            || self.from_name != now.from_name
            || self.body != now.body
            || self.body_clean != now.body_clean
            || self
                .patches
                .iter()
                .zip(&now.patches)
                .any(|((name, content), p)| *name != p.filename || *content != p.content)
    }
}

/// What [`restore_addresses`] did with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restored {
    /// Its stored text got its email addresses back. A thread's subject is
    /// its root's: refresh the thread ([`refresh_threads`]).
    Restored { thread_id: i64 },
    /// It isn't stored, or it's stored as written.
    Unchanged,
    /// Its stored text isn't what ingest once made of the message's, as when
    /// cleaning changed since: it's left as it is.
    Differs,
}

/// Whether anything was given back, and whether anything differed.
#[derive(Default)]
struct Restoring {
    restored: bool,
    differs: bool,
}

impl Restoring {
    /// What to store for a field: `now`, the text as written, if `stored` is
    /// what ingest once made of it; else what's stored.
    fn pick(&mut self, stored: String, now: &str, once: &str) -> String {
        if stored == now {
            stored
        } else if stored == once {
            self.restored = true;
            now.to_owned()
        } else {
            self.differs = true;
            stored
        }
    }
}

/// Gives a stored message the email addresses that ingest once replaced by
/// `[email]`, and its sender's name the addresses ingest left out, from
/// `msg`, the message as the archive has it. Its subject, sender's name,
/// bodies and each patch are replaced only if they're stored exactly as
/// ingest once made them of `msg`'s; then its chunks are made again, to be
/// embedded again, and its patches are indexed again. On error, nothing of
/// the message changes and `tx` stays usable.
pub async fn restore_addresses(tx: &Transaction<'_>, msg: &ParsedMessage) -> Result<Restored> {
    let now = Derived::new(msg);
    let once = now.once(msg.from_email.as_deref());
    if !once.differs_from(&now) {
        return Ok(Restored::Unchanged);
    }
    tx.batch_execute("SAVEPOINT pghacker_restore_addresses")
        .await?;
    match restore(tx, msg, &now, &once).await {
        Ok(restored) => {
            tx.batch_execute("RELEASE SAVEPOINT pghacker_restore_addresses")
                .await?;
            Ok(restored)
        }
        Err(error) => match rollback_to(tx, "pghacker_restore_addresses").await {
            Ok(()) => Err(error),
            Err(rollback) => Err(error.context(format!("and cannot roll back: {rollback}"))),
        },
    }
}

async fn restore(
    tx: &Transaction<'_>,
    msg: &ParsedMessage,
    now: &Derived,
    once: &Once,
) -> Result<Restored> {
    let message_id = normalize_message_id(&msg.message_id);
    // No key changes, so the lock leaves foreign-key checks free: an extraction
    // citing the message meanwhile doesn't wait on it, and can't deadlock with
    // this transaction's lock on the thread later.
    let Some(row) = tx
        .query_typed_opt(
            "SELECT id, thread_id, author_id, subject, from_name, body, body_clean
             FROM messages WHERE message_id = $1 FOR NO KEY UPDATE",
            &[(&message_id, Type::TEXT)],
        )
        .await?
    else {
        return Ok(Restored::Unchanged);
    };
    let (id, thread_id, author_id): (i64, i64, Option<i64>) = (row.get(0), row.get(1), row.get(2));
    let (stored_subject, stored_name, stored_clean): (String, String, String) =
        (row.get(3), row.get(4), row.get(6));
    let mut restoring = Restoring::default();
    let subject = restoring.pick(stored_subject.clone(), &now.subject, &once.subject);
    let from_name = restoring.pick(stored_name.clone(), &now.from_name, &once.from_name);
    let body = restoring.pick(row.get(5), &now.body, &once.body);
    let body_clean = restoring.pick(stored_clean.clone(), &now.body_clean, &once.body_clean);
    if restoring.restored {
        tx.execute_typed(
            "UPDATE messages SET subject = $2, from_name = $3, body = $4, body_clean = $5 WHERE id = $1",
            &[
                (&id, Type::INT8),
                (&subject, Type::TEXT),
                (&from_name, Type::TEXT),
                (&body, Type::TEXT),
                (&body_clean, Type::TEXT),
            ],
        )
        .await?;
    }

    let patches: Vec<(i64, String, String)> = tx
        .query_typed(
            "SELECT id, filename, content FROM patches WHERE message_id = $1",
            &[(&id, Type::INT8)],
        )
        .await?
        .into_iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    let mut renamed = false;
    for (patch, (once_name, once_content)) in now.patches.iter().zip(&once.patches) {
        match patches
            .iter()
            .find(|(_, name, _)| *name == patch.filename || name == once_name)
        {
            Some((_, name, content)) if *name == patch.filename && *content == patch.content => {}
            Some((patch_id, name, content)) if name == once_name && content == once_content => {
                // Indexed again by the patch_lines job.
                tx.execute_typed(
                    "UPDATE patches SET filename = $2, content = $3, text_ids = NULL WHERE id = $1",
                    &[
                        (patch_id, Type::INT8),
                        (&patch.filename, Type::TEXT),
                        (&patch.content, Type::TEXT),
                    ],
                )
                .await?;
                restoring.restored = true;
                renamed |= *name != patch.filename;
            }
            _ => restoring.differs = true,
        }
    }
    // Names and the subject give the patches' series.
    if !patches.is_empty() && (renamed || subject != stored_subject) {
        series::refresh(tx, &[thread_id]).await?;
    }

    // The chunks follow the subject and the cleaned body; the embed job
    // embeds new ones.
    if subject != stored_subject || body_clean != stored_clean {
        let chunks = chunk::chunk(&subject, &body_clean);
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
                 SELECT $1, $2, ord, content FROM unnest($3::int4[], $4::text[]) AS c (ord, content)",
                &[
                    (&id, Type::INT8),
                    (&thread_id, Type::INT8),
                    (&ords, Type::INT4_ARRAY),
                    (&chunks, Type::TEXT_ARRAY),
                ],
            )
            .await?;
        }
    }
    if from_name != stored_name
        && let Some(author_id) = author_id
    {
        rename_author_name(tx, author_id, &stored_name, &from_name).await?;
    }
    Ok(if restoring.restored {
        Restored::Restored { thread_id }
    } else if restoring.differs {
        Restored::Differs
    } else {
        Restored::Unchanged
    })
}

/// Counts a message of `author_id` under its sender's name `to` rather than
/// `from`, and makes the most frequent name the display name. The names
/// decide who committed a commit in the graph (COMMITTED), so the graph sync
/// is told that the author changed.
async fn rename_author_name(
    tx: &Transaction<'_>,
    author_id: i64,
    from: &str,
    to: &str,
) -> Result<()> {
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

async fn rollback_to(tx: &Transaction<'_>, savepoint: &str) -> Result<()> {
    tx.batch_execute(&format!(
        "ROLLBACK TO SAVEPOINT {savepoint}; RELEASE SAVEPOINT {savepoint}"
    ))
    .await?;
    Ok(())
}

async fn existing(tx: &Transaction<'_>, message_id: &str) -> Result<Option<Stored>> {
    Ok(tx
        .query_typed_opt(
            "SELECT id, thread_id FROM messages WHERE message_id = $1",
            &[(&message_id, Type::TEXT)],
        )
        .await?
        .map(|row| Stored {
            message_row_id: row.get(0),
            thread_id: row.get(1),
            inserted: false,
        }))
}

/// The insert, or None if a concurrent transaction stored the message.
async fn insert(
    tx: &Transaction<'_>,
    ctx: &StoreContext,
    msg: &ParsedMessage,
    message_id: &str,
    d: &Derived,
) -> Result<Option<Stored>> {
    let thread_id = resolve_thread(tx, ctx, msg, message_id, &d.subject).await?;
    let author_id = resolve_author(tx, &d.from_name, msg.from_email.as_deref()).await?;
    let parent = msg
        .in_reply_to
        .clone()
        .or_else(|| msg.references.last().cloned());
    let has_patch = !d.patches.is_empty();
    let raw_size = i32::try_from(msg.raw_size).unwrap_or(i32::MAX);
    let row = tx
        .query_typed_opt(
            "INSERT INTO messages (message_id, list, thread_id, parent_message_id, references_ids,
                                   author_id, from_name, subject, sent_at, body, body_clean,
                                   has_patch, archive_month, source_id, raw_sha256, raw_size)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (message_id) DO NOTHING
             RETURNING id",
            &[
                (&message_id, Type::TEXT),
                (&ctx.list, Type::TEXT),
                (&thread_id, Type::INT8),
                (&parent, Type::TEXT),
                (&msg.references, Type::TEXT_ARRAY),
                (&author_id, Type::INT8),
                (&d.from_name, Type::TEXT),
                (&d.subject, Type::TEXT),
                (&msg.sent_at, Type::TIMESTAMPTZ),
                (&d.body, Type::TEXT),
                (&d.body_clean, Type::TEXT),
                (&has_patch, Type::BOOL),
                (&ctx.archive_month, Type::TEXT),
                (&ctx.source_id, Type::INT8),
                (&msg.raw_sha256, Type::BYTEA),
                (&raw_size, Type::INT4),
            ],
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: i64 = row.get(0);

    for p in &d.patches {
        tx.execute_typed(
            "INSERT INTO patches (message_id, filename, series_version, part, paths, added, removed, content)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (message_id, filename) DO NOTHING",
            &[
                (&id, Type::INT8),
                (&p.filename, Type::TEXT),
                (&p.series_version, Type::TEXT),
                (&p.part, Type::INT4),
                (&p.paths, Type::TEXT_ARRAY),
                (&p.added, Type::INT4),
                (&p.removed, Type::INT4),
                (&p.content, Type::TEXT),
            ],
        )
        .await?;
    }
    if !d.anchors.is_empty() {
        let kinds: Vec<&str> = d.anchors.iter().map(|a| a.kind).collect();
        let values: Vec<&str> = d.anchors.iter().map(|a| a.value.as_str()).collect();
        let origins: Vec<&str> = d.anchors.iter().map(|a| a.origin).collect();
        tx.execute_typed(
            "INSERT INTO anchors (message_id, kind, value, origin)
             SELECT $1, kind, value, origin
             FROM unnest($2::text[], $3::text[], $4::text[]) AS a (kind, value, origin)
             ON CONFLICT (message_id, kind, value) DO NOTHING",
            &[
                (&id, Type::INT8),
                (&kinds, Type::TEXT_ARRAY),
                (&values, Type::TEXT_ARRAY),
                (&origins, Type::TEXT_ARRAY),
            ],
        )
        .await?;
    }
    let ords: Vec<i32> = (0..d.chunks.len() as i32).collect();
    tx.execute_typed(
        "INSERT INTO message_chunks (message_id, thread_id, ord, content)
         SELECT $1, $2, ord, content FROM unnest($3::int4[], $4::text[]) AS c (ord, content)",
        &[
            (&id, Type::INT8),
            (&thread_id, Type::INT8),
            (&ords, Type::INT4_ARRAY),
            (&d.chunks, Type::TEXT_ARRAY),
        ],
    )
    .await?;

    count_author_message(tx, author_id, &d.from_name, msg.sent_at).await?;
    tx.execute_typed(
        "UPDATE threads
         SET message_count = message_count + 1,
             started_at = LEAST(started_at, $2),
             last_at = GREATEST(last_at, $2),
             content_version = content_version + 1,
             updated_at = now()
         WHERE id = $1",
        &[(&thread_id, Type::INT8), (&msg.sent_at, Type::TIMESTAMPTZ)],
    )
    .await?;
    // A name numbered like a version (`fix-3.patch`) is one if the thread's
    // posts of it rise; this one may make it so for earlier posts. The
    // thread's row, locked above, keeps concurrent posts in order.
    if d.patches.iter().any(|p| {
        series::name_series(&p.filename, msg.sent_at)
            .sequence
            .is_some()
    }) {
        series::refresh(tx, &[thread_id]).await?;
    }
    Ok(Some(Stored {
        message_row_id: id,
        thread_id,
        inserted: true,
    }))
}

/// The stored ancestor's thread, or the thread rooted in the message's chain,
/// or the subject match, or a new thread; merging the other threads the
/// message connects.
async fn resolve_thread(
    tx: &Transaction<'_>,
    ctx: &StoreContext,
    msg: &ParsedMessage,
    message_id: &str,
    stored_subject: &str,
) -> Result<i64> {
    let mut seen = HashSet::from([message_id.to_owned()]);
    let ancestors: Vec<String> = msg
        .in_reply_to
        .iter()
        .chain(msg.references.iter().rev())
        .map(|id| normalize_message_id(id))
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect();
    let root_hint = msg
        .references
        .first()
        .or(msg.in_reply_to.as_ref())
        .map(|id| normalize_message_id(id))
        .filter(|id| !id.is_empty() && id != message_id);
    let mut chain = vec![message_id.to_owned()];
    chain.extend(ancestors.iter().cloned());

    // Each stored ancestor's thread and its root, and the threads rooted in
    // the chain.
    let rows = tx
        .query_typed(
            "SELECT 'stored', m.message_id, t.id, t.root_message_id
             FROM messages m JOIN threads t ON t.id = m.thread_id
             WHERE m.message_id = ANY($1)
             UNION ALL
             SELECT 'rooted', t.root_message_id, t.id, t.root_message_id
             FROM threads t WHERE t.root_message_id = ANY($2)",
            &[(&ancestors, Type::TEXT_ARRAY), (&chain, Type::TEXT_ARRAY)],
        )
        .await?;
    let mut stored: HashMap<String, i64> = HashMap::new();
    let mut rooted: HashMap<String, i64> = HashMap::new();
    let mut roots: HashMap<i64, String> = HashMap::new();
    for row in rows {
        let kind: &str = row.get(0);
        let key: String = row.get(1);
        let thread: i64 = row.get(2);
        roots.insert(thread, row.get(3));
        if kind == "stored" {
            stored.insert(key, thread);
        } else {
            rooted.insert(key, thread);
        }
    }
    // Without a stored ancestor: the threads of messages that reply to this
    // one or share one of its ancestors that isn't stored.
    let mut shared: BTreeSet<i64> = BTreeSet::new();
    if stored.is_empty() {
        let rows = tx
            .query_typed(
                "SELECT t.id, t.root_message_id FROM threads t
                 WHERE t.id IN (
                     SELECT thread_id FROM messages WHERE references_ids && $1
                     UNION
                     SELECT thread_id FROM messages WHERE parent_message_id = ANY($1))
                 LIMIT 32",
                &[(&chain, Type::TEXT_ARRAY)],
            )
            .await?;
        for row in rows {
            let thread: i64 = row.get(0);
            roots.insert(thread, row.get(1));
            shared.insert(thread);
        }
    }

    let found = ancestors
        .iter()
        .find_map(|a| stored.get(a))
        .or_else(|| root_hint.as_ref().and_then(|r| rooted.get(r)))
        .or_else(|| shared.iter().next())
        .or_else(|| ancestors.iter().rev().find_map(|a| rooted.get(a)))
        .or_else(|| rooted.get(message_id))
        .copied();
    let mut created = false;
    let target = match found {
        Some(thread) => thread,
        None => {
            let matched = if ancestors.is_empty() && subject::is_reply(&msg.subject) {
                subject_match(tx, ctx, stored_subject, msg.sent_at).await?
            } else {
                None
            };
            match matched {
                Some(thread) => thread,
                None => {
                    let root = root_hint.clone().unwrap_or_else(|| message_id.to_owned());
                    let thread = create_thread(tx, ctx, &root, stored_subject, msg.sent_at).await?;
                    roots.insert(thread, root);
                    created = true;
                    thread
                }
            }
        }
    };
    if created {
        adopt_orphan_replies(
            tx,
            ctx,
            target,
            stored_subject,
            msg.sent_at,
            !ancestors.is_empty(),
        )
        .await?;
    }

    let others: BTreeSet<i64> = stored
        .values()
        .chain(rooted.values())
        .chain(shared.iter())
        .copied()
        .filter(|&thread| thread != target)
        .collect();
    let mut kept = HashSet::new();
    for other in others {
        if !merge_thread(tx, other, target).await? {
            kept.insert(other);
        }
    }

    // A thread rooted at this message or a later ancestor takes the earlier
    // root, so that the root's own arrival finds it.
    if let (Some(hint), Some(current)) = (&root_hint, roots.get(&target))
        && current != hint
        && chain.contains(current)
        && rooted.get(hint).is_none_or(|t| !kept.contains(t))
    {
        tx.execute_typed(
            "UPDATE threads SET root_message_id = $2, updated_at = now()
             WHERE id = $1 AND NOT EXISTS (SELECT FROM threads WHERE root_message_id = $2)",
            &[(&target, Type::INT8), (hint, Type::TEXT)],
        )
        .await?;
    }
    Ok(target)
}

/// A thread with the same subject key, active around `sent_at`.
async fn subject_match(
    tx: &Transaction<'_>,
    ctx: &StoreContext,
    stored_subject: &str,
    sent_at: Option<DateTime<Utc>>,
) -> Result<Option<i64>> {
    let key = subject::key(stored_subject);
    let Some(sent_at) = sent_at else {
        return Ok(None);
    };
    if !subject::is_distinctive(&key) {
        return Ok(None);
    }
    // Around the reply both ways, since the thread's earlier messages may not
    // be stored yet; but a stored root must precede the reply.
    Ok(tx
        .query_typed_opt(
            "SELECT t.id FROM threads t
             LEFT JOIN messages r ON r.message_id = t.root_message_id
             WHERE t.list = $1 AND t.subject_key = $2
               AND t.started_at <= $3 + make_interval(days => $4)
               AND t.last_at >= $3 - make_interval(days => $4)
               AND (r.id IS NULL OR r.sent_at <= $3)
             ORDER BY abs(extract(epoch FROM t.last_at - $3)), t.id
             LIMIT 1",
            &[
                (&ctx.list, Type::TEXT),
                (&key, Type::TEXT),
                (&sent_at, Type::TIMESTAMPTZ),
                (&SUBJECT_WINDOW_DAYS, Type::INT4),
            ],
        )
        .await?
        .map(|row| row.get(0)))
}

/// Merges into a new thread the threads that replies without threading
/// headers started before it existed: threads whose root is such a reply
/// with the same subject key, sent within the window (and after `sent_at`
/// unless the new thread continues an older one).
async fn adopt_orphan_replies(
    tx: &Transaction<'_>,
    ctx: &StoreContext,
    target: i64,
    stored_subject: &str,
    sent_at: Option<DateTime<Utc>>,
    continues_older: bool,
) -> Result<()> {
    let key = subject::key(stored_subject);
    let Some(sent_at) = sent_at else {
        return Ok(());
    };
    if !subject::is_distinctive(&key) {
        return Ok(());
    }
    let rows = tx
        .query_typed(
            "SELECT t.id, m.subject
             FROM threads t JOIN messages m ON m.message_id = t.root_message_id
             WHERE t.list = $1 AND t.subject_key = $2 AND t.id <> $3
               AND m.parent_message_id IS NULL AND m.references_ids = '{}'
               AND m.sent_at BETWEEN $4 - make_interval(days => $5) AND $4 + make_interval(days => $5)
               AND ($6 OR m.sent_at >= $4)",
            &[
                (&ctx.list, Type::TEXT),
                (&key, Type::TEXT),
                (&target, Type::INT8),
                (&sent_at, Type::TIMESTAMPTZ),
                (&SUBJECT_WINDOW_DAYS, Type::INT4),
                (&continues_older, Type::BOOL),
            ],
        )
        .await?;
    for row in rows {
        let subject: String = row.get(1);
        if subject::is_reply(&subject) {
            merge_thread(tx, row.get(0), target).await?;
        }
    }
    Ok(())
}

async fn create_thread(
    tx: &Transaction<'_>,
    ctx: &StoreContext,
    root: &str,
    stored_subject: &str,
    sent_at: Option<DateTime<Utc>>,
) -> Result<i64> {
    let display = subject::display(stored_subject);
    let key = subject::key(stored_subject);
    let row = tx
        .query_typed_opt(
            "INSERT INTO threads (list, root_message_id, subject, subject_key, started_at, last_at)
             VALUES ($1, $2, $3, $4, $5, $5)
             ON CONFLICT (root_message_id) DO NOTHING
             RETURNING id",
            &[
                (&ctx.list, Type::TEXT),
                (&root, Type::TEXT),
                (&display, Type::TEXT),
                (&key, Type::TEXT),
                (&sent_at, Type::TIMESTAMPTZ),
            ],
        )
        .await?;
    if let Some(row) = row {
        return Ok(row.get(0));
    }
    Ok(tx
        .query_typed_one(
            "SELECT id FROM threads WHERE root_message_id = $1",
            &[(&root, Type::TEXT)],
        )
        .await?
        .get(0))
}

/// Moves `from`'s messages into `into` and drops `from`. False, with nothing
/// changed, if extracts cite `from`.
async fn merge_thread(tx: &Transaction<'_>, from: i64, into: i64) -> Result<bool> {
    tx.batch_execute("SAVEPOINT pghacker_merge_thread").await?;
    let result = async {
        let moved = tx
            .execute_typed(
                "UPDATE messages SET thread_id = $2 WHERE thread_id = $1",
                &[(&from, Type::INT8), (&into, Type::INT8)],
            )
            .await?;
        tx.execute_typed(
            "UPDATE message_chunks SET thread_id = $2 WHERE thread_id = $1",
            &[(&from, Type::INT8), (&into, Type::INT8)],
        )
        .await?;
        tx.execute_typed("DELETE FROM threads WHERE id = $1", &[(&from, Type::INT8)])
            .await?;
        tx.execute_typed(
            "UPDATE threads t
             SET message_count = s.n, started_at = s.first_at, last_at = s.last_at,
                 content_version = t.content_version + 1, updated_at = now()
             FROM (SELECT count(*)::int AS n, min(sent_at) AS first_at, max(sent_at) AS last_at
                   FROM messages WHERE thread_id = $1) s
             WHERE t.id = $1",
            &[(&into, Type::INT8)],
        )
        .await?;
        Ok::<u64, tokio_postgres::Error>(moved)
    }
    .await;
    match result {
        Ok(moved) => {
            tx.batch_execute("RELEASE SAVEPOINT pghacker_merge_thread")
                .await?;
            tracing::info!(from, into, moved, "merged threads");
            // Names numbered like versions may rise across the merged posts.
            series::refresh(tx, &[into]).await?;
            Ok(true)
        }
        Err(error) if error.code() == Some(&SqlState::FOREIGN_KEY_VIOLATION) => {
            rollback_to(tx, "pghacker_merge_thread").await?;
            tracing::warn!(from, into, "extracts cite the thread; not merging it");
            Ok(false)
        }
        Err(error) => {
            rollback_to(tx, "pghacker_merge_thread").await?;
            Err(error.into())
        }
    }
}

/// The canonical author of an address, created if new; without an address,
/// the only author known by the name, or a new one.
async fn resolve_author(tx: &Transaction<'_>, name: &str, email: Option<&str>) -> Result<i64> {
    let Some(email) = email.map(str::to_lowercase).filter(|e| !e.is_empty()) else {
        let rows = tx
            .query_typed(
                "SELECT DISTINCT author_id FROM author_names WHERE name = $1 LIMIT 2",
                &[(&name, Type::TEXT)],
            )
            .await?;
        if rows.len() == 1 {
            return canonical_author(tx, rows[0].get(0)).await;
        }
        return create_author(tx, name).await;
    };
    if let Some(id) = author_by_email(tx, &email).await? {
        return Ok(id);
    }
    let id = create_author(tx, name).await?;
    let claimed = tx
        .query_typed_opt(
            "INSERT INTO author_emails (email, author_id) VALUES ($1, $2)
             ON CONFLICT (email) DO NOTHING RETURNING author_id",
            &[(&email, Type::TEXT), (&id, Type::INT8)],
        )
        .await?;
    if claimed.is_some() {
        return Ok(id);
    }
    // A concurrent transaction claimed the address first.
    tx.execute_typed("DELETE FROM authors WHERE id = $1", &[(&id, Type::INT8)])
        .await?;
    author_by_email(tx, &email)
        .await?
        .ok_or_else(|| anyhow!("a sender's address conflicted but has no author"))
}

async fn create_author(tx: &Transaction<'_>, name: &str) -> Result<i64> {
    Ok(tx
        .query_typed_one(
            "INSERT INTO authors (display_name) VALUES ($1) RETURNING id",
            &[(&name, Type::TEXT)],
        )
        .await?
        .get(0))
}

/// The author an address belongs to, following merges.
async fn author_by_email(tx: &Transaction<'_>, email: &str) -> Result<Option<i64>> {
    let row = tx
        .query_typed_opt(
            "SELECT author_id FROM author_emails WHERE email = $1",
            &[(&email, Type::TEXT)],
        )
        .await?;
    match row {
        Some(row) => Ok(Some(canonical_author(tx, row.get(0)).await?)),
        None => Ok(None),
    }
}

/// The author that `id` was merged into, if it was.
async fn canonical_author(tx: &Transaction<'_>, id: i64) -> Result<i64> {
    let row = tx
        .query_typed_one(
            "WITH RECURSIVE chain (id, merged_into, depth) AS (
                 SELECT id, merged_into, 0 FROM authors WHERE id = $1
                 UNION ALL
                 SELECT a.id, a.merged_into, c.depth + 1
                 FROM chain c JOIN authors a ON a.id = c.merged_into
                 WHERE c.depth < 32
             )
             SELECT id FROM chain ORDER BY (merged_into IS NULL) DESC, depth DESC LIMIT 1",
            &[(&id, Type::INT8)],
        )
        .await?;
    Ok(row.get(0))
}

/// Counts the message for its author and name; the most frequent name
/// becomes the display name.
async fn count_author_message(
    tx: &Transaction<'_>,
    author_id: i64,
    name: &str,
    sent_at: Option<DateTime<Utc>>,
) -> Result<()> {
    tx.execute_typed(
        "INSERT INTO author_names (author_id, name, count) VALUES ($1, $2, 1)
         ON CONFLICT (author_id, name) DO UPDATE SET count = author_names.count + 1",
        &[(&author_id, Type::INT8), (&name, Type::TEXT)],
    )
    .await?;
    tx.execute_typed(
        "UPDATE authors a
         SET message_count = a.message_count + 1,
             first_seen = LEAST(a.first_seen, $2),
             last_seen = GREATEST(a.last_seen, $2),
             display_name = COALESCE(
                 (SELECT n.name FROM author_names n WHERE n.author_id = a.id
                  ORDER BY n.count DESC, (n.name = a.display_name) DESC, n.name LIMIT 1),
                 a.display_name),
             updated_at = now()
         WHERE a.id = $1",
        &[(&author_id, Type::INT8), (&sent_at, Type::TIMESTAMPTZ)],
    )
    .await?;
    Ok(())
}
