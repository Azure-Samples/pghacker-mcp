// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Threading stored mail again by the rules that thread mail as it arrives
//! ([`crate::store`]), where mail stored before them was threaded otherwise:
//!
//! - Old mailers wrote senders' addresses and placeholders into In-Reply-To
//!   and References, and replies that named the same one shared a thread
//!   ([`store::threading_ids`]). [`reread`] reads again, from the archive,
//!   the months of the messages that name such an ID, and parses each of
//!   their stored messages again: its threading IDs, where they differ from
//!   the stored ones.
//! - Replies on topics of their own ([`subject::starts_topic`]) shared their
//!   parent's thread.
//!
//! [`plan`] threads the affected threads' messages afresh, all at once and
//! so in no order. A message is linked to its ancestors up to the newest that
//! starts a topic. A piece of linked messages joins the other thread whose
//! message, root or unstored ancestor it's linked to, else keeps the
//! affected thread whose root it holds. A piece that starts with a reply
//! without ancestors then joins the nearest thread with its subject, as the
//! subject match of [`crate::store`] does. An affected thread rooted at what
//! isn't a message keeps the remaining piece with most of its messages, and
//! the others become threads of their own, rooted at their first message
//! without ancestors, else at the oldest ancestor of their first message.
//! The changes fall into groups that touch threads no other group touches,
//! each applied in a transaction of its own ([`check`], [`apply_moves`],
//! [`finish`]).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use pghacker_core::db::Pool;
use serde::{Deserialize, Serialize};
use tokio_postgres::types::Type;
use tokio_postgres::{GenericClient, Transaction};

use crate::client::ArchiveClient;
use crate::parse::{self, ParsedMessage};
use crate::store::{self, ThreadingIds};
use crate::{mbox, series, subject};

/// How long a thread stays open to replies that lost their headers, as in
/// [`crate::store`].
const SUBJECT_WINDOW_DAYS: i64 = 60;

/// A stored message's threading IDs, as stored and as they should be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderChange {
    pub message: i64,
    pub message_id: String,
    pub thread: i64,
    pub parent: Option<String>,
    pub references: Vec<String>,
    pub new_parent: Option<String>,
    pub new_references: Vec<String>,
}

/// What reading mail again found.
#[derive(Debug, Default, Serialize)]
pub struct Reread {
    /// Months read again.
    pub months: usize,
    /// Stored messages parsed again.
    pub messages: usize,
    /// Months and messages that couldn't be read again: their stored IDs are
    /// checked as they are.
    pub unreadable: Vec<String>,
    pub changes: Vec<HeaderChange>,
}

/// A stored message's threading, as [`reread`] compares it.
struct StoredIds {
    message: i64,
    thread: i64,
    parent: Option<String>,
    references: Vec<String>,
}

/// Reads again the mail whose stored threading IDs name what isn't a
/// message (a placeholder or a sender's address): every message of the
/// months it's in, and the messages that came from thread pages. Returns
/// the stored messages whose IDs read now differ.
pub async fn reread(pool: &Pool, archive: &ArchiveClient) -> Result<Reread> {
    let client = pool
        .get()
        .await
        .context("cannot get a database connection")?;
    let placeholder = parse::PLACEHOLDER_CHARS as i32;
    let rows = client
        .query_typed(
            "WITH tokens AS (
                 SELECT m.id, x.token
                 FROM messages m
                 CROSS JOIN LATERAL unnest(array_append(m.references_ids, m.parent_message_id)) AS x (token)
             ), named AS (
                 SELECT DISTINCT t.id FROM tokens t
                 WHERE t.token IS NOT NULL
                   AND ((position('@' in t.token) = 0 AND char_length(t.token) <= $1)
                        OR lower(t.token) IN (SELECT email FROM author_emails))
                   AND NOT EXISTS (SELECT FROM messages s WHERE s.message_id = t.token)
             )
             SELECT m.message_id, m.list, m.archive_month FROM named n JOIN messages m ON m.id = n.id
             ORDER BY m.list, m.archive_month, m.id",
            &[(&placeholder, Type::INT4)],
        )
        .await?;
    drop(client);
    let mut months: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut pages: Vec<String> = Vec::new();
    for row in rows {
        let message_id: String = row.get(0);
        match row.get::<_, Option<String>>(2) {
            Some(month) => months
                .entry((row.get(1), month))
                .or_default()
                .push(message_id),
            None => pages.push(message_id),
        }
    }

    let mut out = Reread::default();
    for ((list, month), named) in &months {
        match archive.mbox_month_again(list, month).await {
            Ok(data) => {
                out.months += 1;
                let data = Arc::new(data);
                let parsed = tokio::task::spawn_blocking(move || {
                    mbox::ranges(&data)
                        .into_iter()
                        .filter_map(|range| parse::parse(&data[range]).ok())
                        .collect::<Vec<_>>()
                })
                .await?;
                compare(pool, parsed, &mut out).await?;
            }
            Err(error) => {
                tracing::warn!(%list, %month, error = %format!("{error:#}"), "cannot read a month again");
                out.unreadable.push(format!("{list}/{month}"));
                compare_stored(pool, named, &mut out).await?;
            }
        }
    }
    for id in &pages {
        match archive
            .raw_message(id)
            .await
            .and_then(|raw| parse::parse(&raw))
        {
            Ok(message) => compare(pool, vec![message], &mut out).await?,
            Err(error) => {
                tracing::warn!(message_id = %id, error = %format!("{error:#}"), "cannot read a message again");
                out.unreadable.push(id.clone());
                compare_stored(pool, std::slice::from_ref(id), &mut out).await?;
            }
        }
    }
    out.changes.sort_by_key(|c| c.message);
    out.changes.dedup_by_key(|c| c.message);
    Ok(out)
}

/// The stored threading of the messages with these Message-IDs.
async fn stored_ids(
    client: &impl GenericClient,
    ids: &[&str],
) -> Result<HashMap<String, StoredIds>> {
    Ok(client
        .query_typed(
            "SELECT message_id, id, thread_id, parent_message_id, references_ids FROM messages
             WHERE message_id = ANY($1)",
            &[(&ids, Type::TEXT_ARRAY)],
        )
        .await?
        .iter()
        .map(|row| {
            (
                row.get(0),
                StoredIds {
                    message: row.get(1),
                    thread: row.get(2),
                    parent: row.get(3),
                    references: row.get(4),
                },
            )
        })
        .collect())
}

/// Adds the stored messages among `parsed` whose threading IDs read now
/// differ from the stored ones.
async fn compare(pool: &Pool, parsed: Vec<ParsedMessage>, out: &mut Reread) -> Result<()> {
    let client = pool
        .get()
        .await
        .context("cannot get a database connection")?;
    let ids: Vec<&str> = parsed.iter().map(|m| m.message_id.as_str()).collect();
    let stored = stored_ids(&**client, &ids).await?;
    let mut seen = HashSet::new();
    let parsed: Vec<ParsedMessage> = parsed
        .into_iter()
        .filter(|m| stored.contains_key(&m.message_id) && seen.insert(m.message_id.clone()))
        .collect();
    out.messages += parsed.len();
    let now = store::threading_ids_many(&**client, &parsed).await?;
    for (message, now) in parsed.iter().zip(now) {
        push_change(out, &message.message_id, &stored[&message.message_id], now);
    }
    Ok(())
}

/// Adds the stored messages among `ids` whose stored threading IDs, checked
/// as they are, name what isn't a message: for mail that can't be read
/// again.
async fn compare_stored(pool: &Pool, ids: &[String], out: &mut Reread) -> Result<()> {
    let client = pool
        .get()
        .await
        .context("cannot get a database connection")?;
    let names: Vec<&str> = ids.iter().map(String::as_str).collect();
    let stored = stored_ids(&**client, &names).await?;
    let as_stored: Vec<ParsedMessage> = stored
        .iter()
        .map(|(message_id, row)| ParsedMessage {
            message_id: message_id.clone(),
            in_reply_to: row.parent.clone(),
            references: row.references.clone(),
            ..Default::default()
        })
        .collect();
    let now = store::threading_ids_many(&**client, &as_stored).await?;
    for (message, now) in as_stored.iter().zip(now) {
        push_change(out, &message.message_id, &stored[&message.message_id], now);
    }
    Ok(())
}

fn push_change(out: &mut Reread, message_id: &str, row: &StoredIds, now: ThreadingIds) {
    let new_parent = now.parent().map(str::to_owned);
    if new_parent != row.parent || now.references != row.references {
        out.changes.push(HeaderChange {
            message: row.message,
            message_id: message_id.to_owned(),
            thread: row.thread,
            parent: row.parent.clone(),
            references: row.references.clone(),
            new_parent,
            new_references: now.references,
        });
    }
}

/// Where a message goes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// A stored thread.
    Thread(i64),
    /// A new thread, rooted at this Message-ID.
    New(String),
}

/// A message of an affected thread, as [`partition`] sees it.
#[derive(Clone, Debug)]
pub struct Msg {
    pub id: i64,
    pub message_id: String,
    pub thread: i64,
    pub list: String,
    pub subject: String,
    pub sent_at: Option<DateTime<Utc>>,
    /// It starts a topic.
    pub starts_topic: bool,
    /// The ancestors that link it ([`ancestors`]).
    pub ancestors: Vec<String>,
}

/// The ancestors that link a message: its parent, then References from the
/// newest, up to the newest that starts a topic; none if the message starts
/// one itself.
pub fn ancestors(
    message_id: &str,
    parent: Option<&str>,
    references: &[String],
    starts_topic: bool,
    is_topic: impl Fn(&str) -> bool,
) -> Vec<String> {
    if starts_topic {
        return Vec::new();
    }
    let mut seen = HashSet::from([message_id]);
    let mut out = Vec::new();
    for id in parent
        .into_iter()
        .chain(references.iter().rev().map(String::as_str))
    {
        if !seen.insert(id) {
            continue;
        }
        out.push(id.to_owned());
        if is_topic(id) {
            break;
        }
    }
    out
}

/// What [`partition`] threads.
#[derive(Debug, Default)]
pub struct Input {
    pub messages: Vec<Msg>,
    /// The affected threads and their roots.
    pub roots: BTreeMap<i64, String>,
    /// Other threads' messages among the messages' ancestors: their threads.
    pub outside_messages: HashMap<String, i64>,
    /// Other threads rooted at a message or an ancestor.
    pub outside_roots: HashMap<String, i64>,
    /// Ancestors that aren't stored, and the other threads with messages
    /// linked to them.
    pub outside_shared: HashMap<String, BTreeSet<i64>>,
    /// For a reply without ancestors, the other threads it would match by
    /// subject.
    pub outside_subjects: HashMap<i64, Vec<SubjectMatch>>,
}

/// A thread a reply without ancestors would match by subject, and its last
/// date.
pub type SubjectMatch = (i64, Option<DateTime<Utc>>);

/// Where [`partition`] puts the affected messages.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Partition {
    /// Each message's thread.
    pub destinations: BTreeMap<i64, Destination>,
    /// Affected threads that keep their id under another root.
    pub reroots: BTreeMap<i64, String>,
    /// For threads that pieces join, the other threads those pieces were
    /// linked to as well, which stay apart.
    pub also: BTreeMap<i64, BTreeSet<i64>>,
}

/// Union-find over Message-IDs.
#[derive(Default)]
struct Links<'a> {
    index: HashMap<&'a str, usize>,
    parent: Vec<usize>,
}

impl<'a> Links<'a> {
    fn node(&mut self, id: &'a str) -> usize {
        let parent = &mut self.parent;
        *self.index.entry(id).or_insert_with(|| {
            parent.push(parent.len());
            parent.len() - 1
        })
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a.max(b)] = a.min(b);
        }
    }
}

/// Affected messages linked together, and the IDs that link them.
struct Linked<'a> {
    messages: Vec<&'a Msg>,
    ids: BTreeSet<&'a str>,
}

/// The order of messages: by date, undated last, then by id.
fn by_date(m: &Msg) -> (bool, Option<DateTime<Utc>>, i64) {
    (m.sent_at.is_none(), m.sent_at, m.id)
}

impl<'a> Linked<'a> {
    fn first(&self) -> &'a Msg {
        self.messages
            .iter()
            .copied()
            .min_by_key(|m| by_date(m))
            .expect("linked messages")
    }

    /// Its first message without ancestors, if any.
    fn root_message(&self) -> Option<&'a Msg> {
        self.messages
            .iter()
            .copied()
            .filter(|m| m.ancestors.is_empty())
            .min_by_key(|m| by_date(m))
    }

    /// Its first message without ancestors, else the oldest ancestor of its
    /// first message.
    fn root(&self) -> String {
        if let Some(root) = self.root_message() {
            return root.message_id.clone();
        }
        let first = self.first();
        first.ancestors.last().unwrap_or(&first.message_id).clone()
    }

    /// As a thread that a reply without ancestors may join by subject.
    fn candidate(&self, destination: Destination) -> SubjectCandidate<'a> {
        let root = self.root();
        let root_message = self.messages.iter().find(|m| m.message_id == root);
        SubjectCandidate {
            destination,
            list: self.first().list.as_str(),
            key: subject::key(root_message.map_or(&self.first().subject, |m| &m.subject)),
            first: self.messages.iter().filter_map(|m| m.sent_at).min(),
            last: self.messages.iter().filter_map(|m| m.sent_at).max(),
            root_sent: root_message.and_then(|m| m.sent_at),
        }
    }
}

/// A thread a reply without ancestors may join by subject.
struct SubjectCandidate<'a> {
    destination: Destination,
    list: &'a str,
    key: String,
    first: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
    /// When its root was sent, if it's stored.
    root_sent: Option<DateTime<Utc>>,
}

/// Whether a message is a reply without ancestors, which joins a thread
/// with its subject.
fn headerless_reply(m: &Msg) -> bool {
    m.ancestors.is_empty()
        && !m.starts_topic
        && subject::is_reply(&m.subject)
        && subject::is_distinctive(&subject::key(&m.subject))
}

/// Threads the affected messages afresh.
pub fn partition(input: &Input) -> Partition {
    let mut links = Links::default();
    for m in &input.messages {
        let a = links.node(&m.message_id);
        for ancestor in &m.ancestors {
            let b = links.node(ancestor);
            links.union(a, b);
        }
    }
    let mut by_root: BTreeMap<usize, Linked<'_>> = BTreeMap::new();
    for m in &input.messages {
        let node = links.node(&m.message_id);
        let root = links.find(node);
        by_root
            .entry(root)
            .or_insert_with(|| Linked {
                messages: Vec::new(),
                ids: BTreeSet::new(),
            })
            .messages
            .push(m);
    }
    let nodes: Vec<(&str, usize)> = links.index.iter().map(|(id, n)| (*id, *n)).collect();
    for (id, node) in nodes {
        let root = links.find(node);
        if let Some(linked) = by_root.get_mut(&root) {
            linked.ids.insert(id);
        }
    }
    let pieces: Vec<Linked<'_>> = by_root.into_values().collect();
    let stored: HashSet<&str> = input
        .messages
        .iter()
        .map(|m| m.message_id.as_str())
        .collect();

    let mut out = Partition::default();
    // A piece linked to other threads joins the one it's linked to most; one
    // that holds an affected thread's root keeps that thread.
    let mut destinations: Vec<Option<Destination>> = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        let mut through_messages: BTreeMap<i64, usize> = BTreeMap::new();
        for m in &piece.messages {
            for a in &m.ancestors {
                if let Some(&t) = input.outside_messages.get(a) {
                    *through_messages.entry(t).or_default() += 1;
                }
            }
        }
        let through_roots: BTreeSet<i64> = piece
            .ids
            .iter()
            .filter_map(|id| input.outside_roots.get(*id).copied())
            .collect();
        let through_shared: BTreeSet<i64> = piece
            .ids
            .iter()
            .filter(|id| !stored.contains(**id) && !input.outside_messages.contains_key(**id))
            .filter_map(|id| input.outside_shared.get(*id))
            .flatten()
            .copied()
            .collect();
        let joined = through_messages
            .iter()
            .max_by_key(|(t, n)| (**n, Reverse(**t)))
            .map(|(t, _)| *t)
            .or_else(|| through_roots.first().copied())
            .or_else(|| through_shared.first().copied());
        destinations.push(match joined {
            Some(t) => {
                let also: BTreeSet<i64> = through_messages
                    .keys()
                    .chain(&through_roots)
                    .chain(&through_shared)
                    .copied()
                    .filter(|&o| o != t)
                    .collect();
                if !also.is_empty() {
                    out.also.entry(t).or_default().extend(also);
                }
                Some(Destination::Thread(t))
            }
            None => input
                .roots
                .iter()
                .find(|(_, root)| piece.ids.contains(root.as_str()))
                .map(|(t, _)| Destination::Thread(*t)),
        });
    }
    // The others that start with a reply without ancestors join a thread
    // with their subject, below.
    let by_subject: Vec<bool> = pieces
        .iter()
        .zip(&destinations)
        .map(|(p, d)| d.is_none() && p.root_message().is_some_and(headerless_reply))
        .collect();
    // An affected thread rooted at what links no piece (a placeholder or an
    // address) keeps the remaining piece with most of its messages, under
    // that piece's root.
    let held: BTreeSet<&str> = pieces.iter().flat_map(|p| p.ids.iter().copied()).collect();
    for (&thread, root) in &input.roots {
        if held.contains(root.as_str()) {
            continue;
        }
        let best = pieces
            .iter()
            .enumerate()
            .filter(|(i, _)| destinations[*i].is_none() && !by_subject[*i])
            .map(|(i, p)| {
                let n = p.messages.iter().filter(|m| m.thread == thread).count();
                (i, n, by_date(p.first()))
            })
            .filter(|(_, n, _)| *n > 0)
            .max_by_key(|(_, n, first)| (*n, Reverse(*first)));
        if let Some((i, _, _)) = best {
            destinations[i] = Some(Destination::Thread(thread));
            out.reroots.insert(thread, pieces[i].root());
        }
    }
    for (i, piece) in pieces.iter().enumerate() {
        if destinations[i].is_none() && !by_subject[i] {
            destinations[i] = Some(Destination::New(piece.root()));
        }
    }

    // Pieces that start with a reply without ancestors, in the replies'
    // order: the nearest thread with their subject, active within 60 days
    // and rooted before them, as the subject match does; else a thread of
    // their own, which later ones may join.
    let window = TimeDelta::days(SUBJECT_WINDOW_DAYS);
    let mut candidates: Vec<SubjectCandidate<'_>> = pieces
        .iter()
        .zip(&destinations)
        .filter_map(|(p, d)| match d {
            Some(Destination::Thread(t)) if !input.roots.contains_key(t) => None,
            Some(d) => Some(p.candidate(d.clone())),
            None => None,
        })
        .collect();
    let mut replies: Vec<(usize, &Msg)> = pieces
        .iter()
        .enumerate()
        .filter(|(i, _)| by_subject[*i])
        .filter_map(|(i, p)| p.root_message().map(|m| (i, m)))
        .collect();
    replies.sort_by_key(|(_, m)| by_date(m));
    for (i, m) in replies {
        let piece = &pieces[i];
        let key = subject::key(&m.subject);
        let mut best: Option<(TimeDelta, Destination)> = None;
        if let Some(sent) = m.sent_at {
            let mut consider = |last: Option<DateTime<Utc>>, destination: &Destination| {
                let Some(last) = last else { return };
                let distance = (last - sent).abs();
                if best
                    .as_ref()
                    .is_none_or(|(d, b)| (distance, destination) < (*d, b))
                {
                    best = Some((distance, destination.clone()));
                }
            };
            for c in &candidates {
                if c.list == m.list
                    && c.key == key
                    && c.first.is_some_and(|f| f <= sent + window)
                    && c.last.is_some_and(|l| l >= sent - window)
                    && c.root_sent.is_none_or(|r| r <= sent)
                {
                    consider(c.last, &c.destination);
                }
            }
            for (thread, last) in input.outside_subjects.get(&m.id).into_iter().flatten() {
                consider(*last, &Destination::Thread(*thread));
            }
        }
        let destination = match best {
            Some((_, destination)) => {
                // The thread it joins grows with it.
                if let Some(c) = candidates.iter_mut().find(|c| c.destination == destination) {
                    let joined = piece.candidate(destination.clone());
                    c.first = c.first.into_iter().chain(joined.first).min();
                    c.last = c.last.max(joined.last);
                }
                destination
            }
            None => {
                let destination = Destination::New(m.message_id.clone());
                candidates.push(piece.candidate(destination.clone()));
                destination
            }
        };
        destinations[i] = Some(destination);
    }
    for (piece, destination) in pieces.iter().zip(destinations) {
        let destination = destination.expect("every piece settled");
        for m in &piece.messages {
            out.destinations.insert(m.id, destination.clone());
        }
    }
    out
}

/// A stored thread a group touches, as the plan read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadState {
    pub id: i64,
    pub root: String,
    pub subject: String,
    pub content_version: i32,
}

/// A message that moves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub message: i64,
    pub message_id: String,
    pub from: i64,
    pub to: Destination,
}

/// A thread a group ends up with, for review: the affected messages it has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Piece {
    pub destination: Destination,
    pub root: String,
    pub subject: String,
    pub messages: usize,
    pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>,
    /// The threads its messages were in, with how many.
    pub from: BTreeMap<i64, usize>,
}

/// Threads that no other group touches, and how they change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub threads: Vec<ThreadState>,
    pub headers: Vec<HeaderChange>,
    pub moves: Vec<Move>,
    /// New threads: root and list.
    pub new_threads: BTreeMap<String, String>,
    pub reroots: BTreeMap<i64, String>,
    /// Threads left without messages, which go.
    pub deleted: Vec<i64>,
    /// For threads that pieces join, the other threads those pieces were
    /// linked to as well, which stay apart.
    pub also: BTreeMap<i64, BTreeSet<i64>>,
    pub pieces: Vec<Piece>,
}

/// The changes, in groups.
#[derive(Debug, Default)]
pub struct Plan {
    pub groups: Vec<Group>,
    /// Affected threads, and their messages.
    pub threads: usize,
    pub messages: usize,
    /// Messages that start topics, and those not rooting their threads.
    pub topics: usize,
    pub misplaced_topics: usize,
}

/// A stored message's thread, and what tells whether it starts a topic.
struct StoredInfo {
    thread: i64,
    subject: String,
    sent_at: Option<DateTime<Utc>>,
    parent: Option<(String, Option<DateTime<Utc>>)>,
}

impl StoredInfo {
    fn starts_topic(&self) -> bool {
        self.parent
            .as_ref()
            .is_some_and(|(s, at)| subject::starts_topic(&self.subject, self.sent_at, s, *at))
    }
}

async fn stored_info(
    client: &impl GenericClient,
    ids: &[&str],
) -> Result<HashMap<String, StoredInfo>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(client
        .query_typed(
            "SELECT m.message_id, m.thread_id, m.subject, m.sent_at, p.subject, p.sent_at
             FROM messages m LEFT JOIN messages p ON p.message_id = m.parent_message_id
             WHERE m.message_id = ANY($1)",
            &[(&ids, Type::TEXT_ARRAY)],
        )
        .await?
        .iter()
        .map(|row| {
            let parent: Option<String> = row.get(4);
            (
                row.get(0),
                StoredInfo {
                    thread: row.get(1),
                    subject: row.get(2),
                    sent_at: row.get(3),
                    parent: parent.map(|s| (s, row.get(5))),
                },
            )
        })
        .collect())
}

/// An affected message as stored, with its corrected IDs.
struct Row {
    id: i64,
    message_id: String,
    thread: i64,
    list: String,
    parent: Option<String>,
    references: Vec<String>,
    subject: String,
    sent_at: Option<DateTime<Utc>>,
}

/// Plans threading again the threads of `changes`, and the threads where a
/// reply on a topic of its own isn't its thread's root or replies below it
/// sit in another thread.
pub async fn plan(client: &impl GenericClient, changes: &[HeaderChange]) -> Result<Plan> {
    let fixed: HashMap<i64, &HeaderChange> = changes.iter().map(|c| (c.message, c)).collect();
    let mut affected: BTreeSet<i64> = changes.iter().map(|c| c.thread).collect();

    // Replies on topics of their own, by their stored parents.
    let mut topics: Vec<String> = Vec::new();
    let mut misplaced = 0;
    for row in client
        .query_typed(
            "SELECT m.message_id, m.thread_id, t.root_message_id, m.subject, m.sent_at, p.subject, p.sent_at
             FROM messages m JOIN messages p ON p.message_id = m.parent_message_id
             JOIN threads t ON t.id = m.thread_id
             WHERE m.sent_at >= p.sent_at + make_interval(days => $1)",
            &[(&(subject::TOPIC_DAYS as i32), Type::INT4)],
        )
        .await?
    {
        if subject::starts_topic(row.get(3), row.get(4), row.get(5), row.get(6)) {
            let message_id: String = row.get(0);
            if row.get::<_, &str>(2) != message_id {
                affected.insert(row.get(1));
                misplaced += 1;
            }
            topics.push(message_id);
        }
    }
    // Replies below them in other threads.
    for row in client
        .query_typed(
            "SELECT DISTINCT d.thread_id, b.thread_id
             FROM unnest($1::text[]) AS x (topic)
             JOIN messages b ON b.message_id = x.topic
             JOIN messages d ON d.references_ids @> ARRAY[x.topic] OR d.parent_message_id = x.topic
             WHERE d.thread_id <> b.thread_id",
            &[(&topics, Type::TEXT_ARRAY)],
        )
        .await?
    {
        affected.insert(row.get(0));
        affected.insert(row.get(1));
    }
    let affected: Vec<i64> = affected.into_iter().collect();

    // The affected threads and their messages, with corrected IDs.
    let mut input = Input::default();
    for row in client
        .query_typed(
            "SELECT id, root_message_id FROM threads WHERE id = ANY($1)",
            &[(&affected, Type::INT8_ARRAY)],
        )
        .await?
    {
        input.roots.insert(row.get(0), row.get(1));
    }
    let rows: Vec<Row> = client
        .query_typed(
            "SELECT id, message_id, thread_id, list, parent_message_id, references_ids, subject, sent_at
             FROM messages WHERE thread_id = ANY($1) ORDER BY id",
            &[(&affected, Type::INT8_ARRAY)],
        )
        .await?
        .iter()
        .map(|row| {
            let id: i64 = row.get(0);
            let (parent, references) = match fixed.get(&id) {
                Some(c) => (c.new_parent.clone(), c.new_references.clone()),
                None => (row.get(4), row.get(5)),
            };
            Row {
                id,
                message_id: row.get(1),
                thread: row.get(2),
                list: row.get(3),
                parent,
                references,
                subject: row.get(6),
                sent_at: row.get(7),
            }
        })
        .collect();
    let affected_rows: HashMap<&str, &Row> =
        rows.iter().map(|r| (r.message_id.as_str(), r)).collect();

    // Other stored messages among their ancestors: their threads, and
    // whether they start topics.
    let wanted: BTreeSet<&str> = rows
        .iter()
        .flat_map(|r| r.parent.iter().chain(&r.references))
        .map(String::as_str)
        .filter(|id| !affected_rows.contains_key(id))
        .collect();
    let wanted: Vec<&str> = wanted.into_iter().collect();
    let outside = stored_info(client, &wanted).await?;
    let starts: HashMap<&str, bool> = rows
        .iter()
        .map(|r| {
            let parent = r.parent.as_deref().and_then(|p| {
                affected_rows
                    .get(p)
                    .map(|q| (q.subject.as_str(), q.sent_at))
                    .or_else(|| outside.get(p).map(|o| (o.subject.as_str(), o.sent_at)))
            });
            let starts =
                parent.is_some_and(|(s, at)| subject::starts_topic(&r.subject, r.sent_at, s, at));
            (r.message_id.as_str(), starts)
        })
        .collect();
    let is_topic = |id: &str| -> bool {
        starts
            .get(id)
            .copied()
            .unwrap_or_else(|| outside.get(id).is_some_and(StoredInfo::starts_topic))
    };
    input.messages = rows
        .iter()
        .map(|r| {
            let starts_topic = starts[r.message_id.as_str()];
            Msg {
                id: r.id,
                message_id: r.message_id.clone(),
                thread: r.thread,
                list: r.list.clone(),
                subject: r.subject.clone(),
                sent_at: r.sent_at,
                starts_topic,
                ancestors: ancestors(
                    &r.message_id,
                    r.parent.as_deref(),
                    &r.references,
                    starts_topic,
                    is_topic,
                ),
            }
        })
        .collect();

    // What links them to other threads: those threads' messages and roots,
    // and unstored ancestors that those threads' messages are linked to.
    let mut unstored: BTreeSet<String> = BTreeSet::new();
    let mut universe: BTreeSet<String> = BTreeSet::new();
    for m in &input.messages {
        universe.insert(m.message_id.clone());
        for a in &m.ancestors {
            universe.insert(a.clone());
            if let Some(o) = outside.get(a) {
                input.outside_messages.insert(a.clone(), o.thread);
            } else if !affected_rows.contains_key(a.as_str()) {
                unstored.insert(a.clone());
            }
        }
    }
    let universe: Vec<String> = universe.into_iter().collect();
    for row in client
        .query_typed(
            "SELECT root_message_id, id FROM threads WHERE root_message_id = ANY($1) AND id <> ALL($2)",
            &[(&universe, Type::TEXT_ARRAY), (&affected, Type::INT8_ARRAY)],
        )
        .await?
    {
        input.outside_roots.insert(row.get(0), row.get(1));
    }
    if !unstored.is_empty() {
        let unstored_ids: Vec<&String> = unstored.iter().collect();
        let sharing = client
            .query_typed(
                "SELECT message_id, thread_id, parent_message_id, references_ids FROM messages
                 WHERE (references_ids && $1 OR parent_message_id = ANY($1)) AND thread_id <> ALL($2)",
                &[(&unstored_ids, Type::TEXT_ARRAY), (&affected, Type::INT8_ARRAY)],
            )
            .await?;
        let mut chain_ids: BTreeSet<String> = BTreeSet::new();
        for row in &sharing {
            chain_ids.insert(row.get(0));
            let parent: Option<String> = row.get(2);
            chain_ids.extend(parent);
            chain_ids.extend(row.get::<_, Vec<String>>(3));
        }
        let chain_ids: Vec<&str> = chain_ids.iter().map(String::as_str).collect();
        let theirs = stored_info(client, &chain_ids).await?;
        for row in &sharing {
            let message_id: String = row.get(0);
            let parent: Option<String> = row.get(2);
            let references: Vec<String> = row.get(3);
            let own = theirs
                .get(&message_id)
                .is_some_and(StoredInfo::starts_topic);
            let linked = ancestors(&message_id, parent.as_deref(), &references, own, |id| {
                theirs.get(id).is_some_and(StoredInfo::starts_topic)
            });
            for a in linked {
                if unstored.contains(&a) {
                    input
                        .outside_shared
                        .entry(a)
                        .or_default()
                        .insert(row.get(1));
                }
            }
        }
    }
    let window = SUBJECT_WINDOW_DAYS as i32;
    for m in input.messages.iter().filter(|m| headerless_reply(m)) {
        let Some(sent) = m.sent_at else { continue };
        let key = subject::key(&m.subject);
        let found: Vec<SubjectMatch> = client
            .query_typed(
                "SELECT t.id, t.last_at FROM threads t
                 LEFT JOIN messages r ON r.message_id = t.root_message_id
                 WHERE t.list = $1 AND t.subject_key = $2 AND t.id <> ALL($5)
                   AND t.started_at <= $3 + make_interval(days => $4)
                   AND t.last_at >= $3 - make_interval(days => $4)
                   AND (r.id IS NULL OR r.sent_at <= $3)",
                &[
                    (&m.list, Type::TEXT),
                    (&key, Type::TEXT),
                    (&sent, Type::TIMESTAMPTZ),
                    (&window, Type::INT4),
                    (&affected, Type::INT8_ARRAY),
                ],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        if !found.is_empty() {
            input.outside_subjects.insert(m.id, found);
        }
    }
    let partition = partition(&input);
    let mut plan = Plan {
        groups: groups(client, &input, &partition, changes).await?,
        threads: affected.len(),
        messages: input.messages.len(),
        topics: topics.len(),
        misplaced_topics: misplaced,
    };
    plan.groups.sort_by_key(|g| g.threads.first().map(|t| t.id));
    Ok(plan)
}

/// A node of the graph of threads that changes link.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Node {
    Thread(i64),
    New(String),
}

impl From<&Destination> for Node {
    fn from(d: &Destination) -> Self {
        match d {
            Destination::Thread(t) => Self::Thread(*t),
            Destination::New(root) => Self::New(root.clone()),
        }
    }
}

/// The partition's changes, in groups of threads that no other group
/// touches, with what review needs.
async fn groups(
    client: &impl GenericClient,
    input: &Input,
    partition: &Partition,
    changes: &[HeaderChange],
) -> Result<Vec<Group>> {
    let moves: Vec<Move> = input
        .messages
        .iter()
        .filter(|m| partition.destinations[&m.id] != Destination::Thread(m.thread))
        .map(|m| Move {
            message: m.id,
            message_id: m.message_id.clone(),
            from: m.thread,
            to: partition.destinations[&m.id].clone(),
        })
        .collect();
    let mut index: BTreeMap<Node, usize> = BTreeMap::new();
    let mut parent: Vec<usize> = Vec::new();
    let mut node = |n: Node, parent: &mut Vec<usize>| -> usize {
        *index.entry(n).or_insert_with(|| {
            parent.push(parent.len());
            parent.len() - 1
        })
    };
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for mv in &moves {
        let a = node(Node::Thread(mv.from), &mut parent);
        let b = node(Node::from(&mv.to), &mut parent);
        let (a, b) = (find(&mut parent, a), find(&mut parent, b));
        if a != b {
            parent[a.max(b)] = a.min(b);
        }
    }
    for c in changes {
        node(Node::Thread(c.thread), &mut parent);
    }
    for t in partition.reroots.keys() {
        node(Node::Thread(*t), &mut parent);
    }
    let mut group_of: HashMap<Node, usize> = HashMap::new();
    let mut groups: BTreeMap<usize, Group> = BTreeMap::new();
    let mut thread_ids: BTreeSet<i64> = BTreeSet::new();
    for (n, i) in index {
        let root = find(&mut parent, i);
        if let Node::Thread(t) = n {
            thread_ids.insert(t);
        }
        group_of.insert(n, root);
        groups.entry(root).or_default();
    }
    for mv in moves {
        let g = group_of[&Node::Thread(mv.from)];
        groups.get_mut(&g).expect("a group").moves.push(mv);
    }
    for c in changes {
        let g = group_of[&Node::Thread(c.thread)];
        groups.get_mut(&g).expect("a group").headers.push(c.clone());
    }
    for (t, root) in &partition.reroots {
        let g = group_of[&Node::Thread(*t)];
        groups
            .get_mut(&g)
            .expect("a group")
            .reroots
            .insert(*t, root.clone());
    }

    // The stored threads, as they are now.
    let thread_ids: Vec<i64> = thread_ids.into_iter().collect();
    let states: HashMap<i64, ThreadState> = client
        .query_typed(
            "SELECT id, root_message_id, subject, content_version FROM threads WHERE id = ANY($1)",
            &[(&thread_ids, Type::INT8_ARRAY)],
        )
        .await?
        .iter()
        .map(|row| {
            (
                row.get(0),
                ThreadState {
                    id: row.get(0),
                    root: row.get(1),
                    subject: row.get(2),
                    content_version: row.get(3),
                },
            )
        })
        .collect();
    let kept: HashSet<i64> = partition
        .destinations
        .values()
        .filter_map(|d| match d {
            Destination::Thread(t) => Some(*t),
            Destination::New(_) => None,
        })
        .collect();
    let lists: HashMap<i64, &str> = input
        .messages
        .iter()
        .map(|m| (m.id, m.list.as_str()))
        .collect();
    let mut out = Vec::new();
    for mut g in groups.into_values() {
        if g.moves.is_empty() && g.headers.is_empty() && g.reroots.is_empty() {
            continue;
        }
        let mut threads: BTreeSet<i64> = BTreeSet::new();
        for mv in &g.moves {
            threads.insert(mv.from);
            match &mv.to {
                Destination::Thread(t) => {
                    threads.insert(*t);
                }
                Destination::New(root) => {
                    g.new_threads
                        .entry(root.clone())
                        .or_insert_with(|| lists[&mv.message].to_owned());
                }
            }
        }
        threads.extend(g.headers.iter().map(|c| c.thread));
        threads.extend(g.reroots.keys());
        g.threads = threads
            .iter()
            .filter_map(|t| states.get(t).cloned())
            .collect();
        g.deleted = threads
            .iter()
            .copied()
            .filter(|t| input.roots.contains_key(t) && !kept.contains(t))
            .collect();
        for (t, also) in &partition.also {
            if threads.contains(t) {
                g.also.insert(*t, also.clone());
            }
        }
        g.pieces = pieces(input, partition, &threads, &states, &g.reroots);
        g.headers.sort_by_key(|c| c.message);
        g.moves.sort_by_key(|m| m.message);
        out.push(g);
    }
    Ok(out)
}

/// The threads that a group's affected messages end up in, for review.
fn pieces(
    input: &Input,
    partition: &Partition,
    threads: &BTreeSet<i64>,
    states: &HashMap<i64, ThreadState>,
    reroots: &BTreeMap<i64, String>,
) -> Vec<Piece> {
    let mut members: BTreeMap<&Destination, Vec<&Msg>> = BTreeMap::new();
    for m in input
        .messages
        .iter()
        .filter(|m| threads.contains(&m.thread))
    {
        members
            .entry(&partition.destinations[&m.id])
            .or_default()
            .push(m);
    }
    members
        .into_iter()
        .map(|(destination, messages)| {
            let (root, subject) = match destination {
                Destination::Thread(t) => (
                    reroots
                        .get(t)
                        .cloned()
                        .or_else(|| states.get(t).map(|s| s.root.clone()))
                        .unwrap_or_default(),
                    states.get(t).map(|s| s.subject.clone()).unwrap_or_default(),
                ),
                Destination::New(root) => {
                    let subject = messages
                        .iter()
                        .find(|m| m.message_id == *root)
                        .or_else(|| messages.iter().min_by_key(|m| by_date(m)))
                        .map(|m| subject::display(&m.subject))
                        .unwrap_or_default();
                    (root.clone(), subject)
                }
            };
            let mut from: BTreeMap<i64, usize> = BTreeMap::new();
            for m in &messages {
                *from.entry(m.thread).or_default() += 1;
            }
            Piece {
                destination: destination.clone(),
                root,
                subject,
                messages: messages.len(),
                first: messages.iter().filter_map(|m| m.sent_at).min(),
                last: messages.iter().filter_map(|m| m.sent_at).max(),
                from,
            }
        })
        .collect()
}

/// Why a group can't be applied as planned, if it can't: a thread it touches
/// changed or went, a message moved, or stored IDs changed. Locks the
/// group's threads, in id order, first.
pub async fn check(client: &impl GenericClient, group: &Group) -> Result<Option<String>> {
    let ids: Vec<i64> = group.threads.iter().map(|t| t.id).collect();
    let now: HashMap<i64, (String, i32)> = client
        .query_typed(
            "SELECT id, root_message_id, content_version FROM threads WHERE id = ANY($1)
             ORDER BY id FOR UPDATE",
            &[(&ids, Type::INT8_ARRAY)],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), (row.get(1), row.get(2))))
        .collect();
    for t in &group.threads {
        match now.get(&t.id) {
            None => return Ok(Some(format!("thread {} is gone", t.id))),
            Some((root, version)) if *root != t.root || *version != t.content_version => {
                return Ok(Some(format!("thread {} changed", t.id)));
            }
            Some(_) => {}
        }
    }
    let messages: Vec<i64> = group
        .moves
        .iter()
        .map(|m| m.message)
        .chain(group.headers.iter().map(|c| c.message))
        .collect();
    let stored: HashMap<i64, (i64, Option<String>, Vec<String>)> = client
        .query_typed(
            "SELECT id, thread_id, parent_message_id, references_ids FROM messages WHERE id = ANY($1)",
            &[(&messages, Type::INT8_ARRAY)],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), (row.get(1), row.get(2), row.get(3))))
        .collect();
    for mv in &group.moves {
        if stored.get(&mv.message).map(|s| s.0) != Some(mv.from) {
            return Ok(Some(format!("message {} moved", mv.message_id)));
        }
    }
    for c in &group.headers {
        match stored.get(&c.message) {
            Some((_, parent, references)) if *parent == c.parent && *references == c.references => {
            }
            _ => return Ok(Some(format!("message {} changed", c.message_id))),
        }
    }
    Ok(None)
}

/// Writes a group's corrected IDs, makes its new threads and moves its
/// messages there; returns the new threads' ids by root. What cites the
/// threads that lose messages follows them before [`finish`].
pub async fn apply_moves(
    client: &impl GenericClient,
    group: &Group,
) -> Result<BTreeMap<String, i64>> {
    for c in &group.headers {
        client
            .execute_typed(
                "UPDATE messages SET parent_message_id = $2, references_ids = $3 WHERE id = $1",
                &[
                    (&c.message, Type::INT8),
                    (&c.new_parent, Type::TEXT),
                    (&c.new_references, Type::TEXT_ARRAY),
                ],
            )
            .await?;
    }
    let mut created = BTreeMap::new();
    for (root, list) in &group.new_threads {
        let id: i64 = client
            .query_typed_one(
                "INSERT INTO threads (list, root_message_id) VALUES ($1, $2) RETURNING id",
                &[(list, Type::TEXT), (root, Type::TEXT)],
            )
            .await
            .with_context(|| format!("cannot make the thread rooted at {root}"))?
            .get(0);
        created.insert(root.clone(), id);
    }
    let mut by_target: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for mv in &group.moves {
        by_target
            .entry(resolve(&mv.to, &created))
            .or_default()
            .push(mv.message);
    }
    for (target, ids) in &by_target {
        client
            .execute_typed(
                "UPDATE messages SET thread_id = $2 WHERE id = ANY($1)",
                &[(ids, Type::INT8_ARRAY), (target, Type::INT8)],
            )
            .await?;
        client
            .execute_typed(
                "UPDATE message_chunks SET thread_id = $2 WHERE message_id = ANY($1)",
                &[(ids, Type::INT8_ARRAY), (target, Type::INT8)],
            )
            .await?;
    }
    Ok(created)
}

/// The id of a destination once [`apply_moves`] made the new threads.
pub fn resolve(destination: &Destination, created: &BTreeMap<String, i64>) -> i64 {
    match destination {
        Destination::Thread(t) => *t,
        Destination::New(root) => created[root],
    }
}

/// Re-roots, drops the threads left without messages (once nothing cites
/// them), and recounts the group's threads: counts, dates, subjects, new
/// content versions for those whose messages changed, and patch series.
pub async fn finish(
    tx: &Transaction<'_>,
    group: &Group,
    created: &BTreeMap<String, i64>,
) -> Result<()> {
    tx.execute_typed(
        "DELETE FROM threads WHERE id = ANY($1)",
        &[(&group.deleted, Type::INT8_ARRAY)],
    )
    .await
    .context("cannot drop the threads left without messages")?;
    for (thread, root) in &group.reroots {
        tx.execute_typed(
            "UPDATE threads SET root_message_id = $2, updated_at = now() WHERE id = $1",
            &[(thread, Type::INT8), (root, Type::TEXT)],
        )
        .await
        .with_context(|| format!("cannot root thread {thread} at {root}"))?;
    }
    let deleted: HashSet<i64> = group.deleted.iter().copied().collect();
    let changed: Vec<i64> = group
        .moves
        .iter()
        .flat_map(|m| [m.from, resolve(&m.to, created)])
        .filter(|t| !deleted.contains(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let touched: Vec<i64> = group
        .threads
        .iter()
        .map(|t| t.id)
        .chain(created.values().copied())
        .filter(|t| !deleted.contains(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    store::recount_threads(tx, &changed).await?;
    store::refresh_threads(tx, &touched).await?;
    series::refresh(tx, &touched).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
