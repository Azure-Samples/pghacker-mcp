// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Threading stored mail again, as `rethread_mail` jobs: the threads that
//! senders' addresses and placeholders in old mail's headers glued together,
//! and replies on topics of their own that share their parent's thread
//! ([`pghacker_mail::rethread`]).
//!
//! A dry run (key `dry-run`) reads the mail again, plans, and records its
//! report as the job's result: the groups of changes, each with a hash, and
//! an ID for the plan. An apply (key `apply`, with the plan's ID) checks the
//! ID against the last dry run's, plans again, and applies each group whose
//! hash the dry run reported, in a transaction of its own; it skips a group
//! that changed since, or whose threads an extraction lease holds. Extracts
//! follow the messages they cite: a decision or position goes to the thread
//! with most of its citations, a feature is linked to each thread with its
//! citations, decisions or positions, a feature's file to the one whose
//! patches show it. Nothing is deleted but threads left without messages.
//! Threads that change are extracted again: new ones are pending, the
//! others get a new content version.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use pghacker_mail::rethread::{self, Destination, Group, Plan, Reread};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::GenericClient;

use crate::Indexer;
use crate::jobs::{self, Job, kind};

/// The job keys.
pub const DRY_RUN: &str = "dry-run";
pub const APPLY: &str = "apply";

/// An extract that moves from a thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractMove {
    pub id: i64,
    pub from: i64,
    pub to: Destination,
}

/// A feature's link to a thread that loses messages: the threads it's
/// linked to instead or as well.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureLinks {
    pub feature: i64,
    pub from: i64,
    pub to: BTreeSet<Destination>,
    /// It stays linked to `from` too.
    pub keep: bool,
}

/// A feature's file whose thread changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFile {
    pub feature: i64,
    pub path: String,
    pub from: i64,
    pub to: Destination,
}

/// A feature merge's record of the threads it linked (what `undo_merge`
/// unlinks), where one of them changes: `from` gives way to `to`, the
/// threads its link goes to that the feature wasn't linked to otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeLinks {
    pub merge: i64,
    pub feature: i64,
    pub from: i64,
    pub to: BTreeSet<Destination>,
}

/// Where the extracts on a group's threads that lose messages go.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractMoves {
    pub decisions: Vec<ExtractMove>,
    pub positions: Vec<ExtractMove>,
    pub feature_links: Vec<FeatureLinks>,
    pub feature_files: Vec<FeatureFile>,
    pub drafts: Vec<ExtractMove>,
    pub merges: Vec<MergeLinks>,
}

/// A group of the plan, with where its extracts go and its hash.
#[derive(Clone, Debug)]
pub struct PlannedGroup {
    pub hash: String,
    pub group: Group,
    pub extracts: ExtractMoves,
}

/// Runs a `rethread_mail` job: a dry run, or an apply.
pub async fn run(indexer: &Indexer, job: &Job) -> Result<Value> {
    match job.key.as_str() {
        DRY_RUN => dry_run(indexer).await,
        APPLY => {
            let plan = job
                .payload
                .get("plan")
                .and_then(Value::as_str)
                .context("an apply needs the plan's ID from a dry run")?;
            apply(indexer, plan).await
        }
        other => anyhow::bail!("rethread_mail jobs are {DRY_RUN} or {APPLY}, not {other}"),
    }
}

/// Queues a dry run, or the apply of a dry run's plan: what
/// `pghacker.rethread_mail()` says.
pub async fn queue(client: &impl GenericClient, plan: Option<&str>) -> Result<String> {
    Ok(client
        .query_one("SELECT pghacker.rethread_mail($1)", &[&plan])
        .await?
        .get(0))
}

/// Reads the mail again and plans, with where extracts go.
async fn compute(indexer: &Indexer) -> Result<(Reread, Plan, Vec<PlannedGroup>)> {
    let reread = rethread::reread(&indexer.pool, &indexer.archive).await?;
    let client = indexer.pool.get().await?;
    let plan = rethread::plan(&**client, &reread.changes).await?;
    let mut groups = Vec::with_capacity(plan.groups.len());
    for group in &plan.groups {
        let extracts = plan_extracts(&**client, group).await?;
        groups.push(PlannedGroup {
            hash: hash(group, &extracts)?,
            group: group.clone(),
            extracts,
        });
    }
    Ok((reread, plan, groups))
}

/// A group's hash: of everything it changes, and the state it changes it
/// from.
fn hash(group: &Group, extracts: &ExtractMoves) -> Result<String> {
    let canonical = serde_json::to_vec(&(group, extracts))?;
    Ok(hex::encode(Sha256::digest(&canonical)))
}

/// A plan's ID: of its groups' hashes.
fn plan_id(groups: &[PlannedGroup]) -> String {
    let mut digest = Sha256::new();
    for g in groups {
        digest.update(g.hash.as_bytes());
        digest.update(b"\n");
    }
    hex::encode(digest.finalize())
}

async fn dry_run(indexer: &Indexer) -> Result<Value> {
    let (reread, plan, groups) = compute(indexer).await?;
    let id = plan_id(&groups);
    let count = |f: fn(&PlannedGroup) -> usize| groups.iter().map(f).sum::<usize>();
    Ok(json!({
        "plan": id,
        "reread": {
            "months": reread.months,
            "messages": reread.messages,
            "unreadable": reread.unreadable,
            "header_changes": reread.changes.len(),
        },
        "summary": {
            "affected_threads": plan.threads,
            "affected_messages": plan.messages,
            "topics": plan.topics,
            "misplaced_topics": plan.misplaced_topics,
            "groups": groups.len(),
            "moves": count(|g| g.group.moves.len()),
            "new_threads": count(|g| g.group.new_threads.len()),
            "deleted_threads": count(|g| g.group.deleted.len()),
            "reroots": count(|g| g.group.reroots.len()),
            "header_changes": count(|g| g.group.headers.len()),
            "extracts": {
                "decisions": count(|g| g.extracts.decisions.len()),
                "positions": count(|g| g.extracts.positions.len()),
                "feature_links": count(|g| g.extracts.feature_links.len()),
                "feature_files": count(|g| g.extracts.feature_files.len()),
                "drafts": count(|g| g.extracts.drafts.len()),
                "merges": count(|g| g.extracts.merges.len()),
            },
        },
        "groups": groups.iter().map(|g| json!({
            "hash": g.hash,
            "threads": g.group.threads,
            "pieces": g.group.pieces,
            "deleted": g.group.deleted,
            "reroots": g.group.reroots,
            "also": g.group.also,
            "moves": g.group.moves.len(),
            "headers": g.group.headers,
            "extracts": g.extracts,
        })).collect::<Vec<_>>(),
        "next": format!("Review the groups, then apply them: SELECT pghacker.rethread_mail('{id}');"),
    }))
}

async fn apply(indexer: &Indexer, approved: &str) -> Result<Value> {
    let report: Option<Value> = indexer
        .pool
        .get()
        .await?
        .query_opt(
            "SELECT result FROM jobs WHERE kind = $1 AND key = $2 AND state = 'done'",
            &[&kind::RETHREAD_MAIL, &DRY_RUN],
        )
        .await?
        .map(|row| row.get(0));
    let Some(report) = report else {
        return Ok(json!({
            "plan": approved,
            "applied": [],
            "error": "no dry run has finished: SELECT pghacker.rethread_mail(); first",
        }));
    };
    if report["plan"].as_str() != Some(approved) {
        return Ok(json!({
            "plan": approved,
            "applied": [],
            "error": format!("the last dry run's plan is {}: review that one and apply it", report["plan"]),
        }));
    }
    let reviewed: HashSet<&str> = report["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g["hash"].as_str())
        .collect();

    let (_, _, groups) = compute(indexer).await?;
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    let mut extract: BTreeSet<i64> = BTreeSet::new();
    let mut found = HashSet::new();
    for g in &groups {
        let threads: Vec<i64> = g.group.threads.iter().map(|t| t.id).collect();
        if !reviewed.contains(g.hash.as_str()) {
            skipped.push(json!({ "hash": g.hash, "threads": threads,
                                 "reason": "not in the reviewed plan: run a dry run again" }));
            continue;
        }
        found.insert(g.hash.as_str());
        match apply_group(indexer, g).await {
            Ok(Ok(created)) => {
                let deleted: HashSet<i64> = g.group.deleted.iter().copied().collect();
                for mv in &g.group.moves {
                    extract.insert(mv.from);
                    extract.insert(rethread::resolve(&mv.to, &created));
                }
                extract.retain(|t| !deleted.contains(t));
                applied.push(json!({ "hash": g.hash, "threads": threads, "new_threads": created }));
            }
            Ok(Err(reason)) => {
                skipped.push(json!({ "hash": g.hash, "threads": threads, "reason": reason }))
            }
            Err(error) => {
                tracing::warn!(hash = %g.hash, error = %format!("{error:#}"), "cannot apply a group");
                skipped.push(
                    json!({ "hash": g.hash, "threads": threads, "reason": format!("{error:#}") }),
                );
            }
        }
    }
    let gone: Vec<&str> = reviewed
        .iter()
        .copied()
        .filter(|h| !found.contains(h))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !applied.is_empty() {
        // Commits' links to threads follow their messages now rather than
        // at the next history sync, and the graph soon. The groups are
        // committed: a failure here mustn't lose the report.
        match indexer.pool.get().await {
            Ok(client) => {
                if let Err(error) =
                    pghacker_code::history::links::refresh(&**client, "pghacker").await
                {
                    tracing::warn!(error = %format!("{error:#}"), "cannot refresh commits' thread links; the history sync will");
                }
                if let Err(error) = jobs::wake(&**client, kind::GRAPH_SYNC).await {
                    tracing::warn!(error = %format!("{error:#}"), "cannot wake the graph sync; it runs on its own");
                }
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "cannot refresh commits' thread links; the history sync will")
            }
        }
    }
    Ok(json!({
        "plan": approved,
        "applied": applied,
        "skipped": skipped,
        "not_found": gone,
        "extract": extract,
        "next": if skipped.is_empty() && gone.is_empty() {
            "Extract the threads in `extract` again."
        } else {
            "Extract the threads in `extract` again. For the rest, run a dry run again and review it."
        },
    }))
}

/// Applies a group in a transaction, unless an extraction lease holds one of
/// its threads or it changed since it was planned: why not, then.
async fn apply_group(
    indexer: &Indexer,
    g: &PlannedGroup,
) -> Result<std::result::Result<BTreeMap<String, i64>, String>> {
    let mut client = indexer.pool.get().await?;
    let tx = client.transaction().await?;
    // No lease starts while the group changes.
    tx.batch_execute("SELECT pg_advisory_xact_lock(hashtext('pghacker.extraction_leases'))")
        .await?;
    let ids: Vec<i64> = g.group.threads.iter().map(|t| t.id).collect();
    let leased: Vec<i64> = tx
        .query(
            "SELECT DISTINCT t FROM extraction_leases l, unnest(l.thread_ids) AS t
             WHERE l.released_at IS NULL AND l.expires_at > now() AND t = ANY ($1) ORDER BY 1",
            &[&ids],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect();
    if !leased.is_empty() {
        return Ok(Err(format!(
            "extraction leases hold threads {leased:?}: apply again once they're released"
        )));
    }
    if let Some(reason) = rethread::check(&*tx, &g.group).await? {
        return Ok(Err(format!("{reason} since the plan: run a dry run again")));
    }
    if plan_extracts(&*tx, &g.group).await? != g.extracts {
        return Ok(Err(
            "its extracts changed since the plan: run a dry run again".into(),
        ));
    }
    let created = rethread::apply_moves(&*tx, &g.group).await?;
    apply_extracts(&*tx, &g.extracts, &created).await?;
    rethread::finish(&tx, &g.group, &created).await?;
    tx.commit().await?;
    tracing::info!(hash = %g.hash, threads = ?ids, moved = g.group.moves.len(), "threaded mail again");
    Ok(Ok(created))
}

/// A message of a thread that loses messages.
struct SourceMessage {
    thread: i64,
    message_id: String,
    sent_at: Option<DateTime<Utc>>,
}

/// Where the extracts on the group's threads that lose messages go.
pub async fn plan_extracts(client: &impl GenericClient, group: &Group) -> Result<ExtractMoves> {
    let sources: Vec<i64> = group
        .moves
        .iter()
        .map(|m| m.from)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut out = ExtractMoves::default();
    if sources.is_empty() {
        return Ok(out);
    }
    let deleted: HashSet<i64> = group.deleted.iter().copied().collect();
    let moved: HashMap<i64, &Destination> =
        group.moves.iter().map(|m| (m.message, &m.to)).collect();
    let messages: HashMap<i64, SourceMessage> = client
        .query(
            "SELECT id, thread_id, message_id, sent_at FROM messages WHERE thread_id = ANY ($1)",
            &[&sources],
        )
        .await?
        .iter()
        .map(|row| {
            (
                row.get(0),
                SourceMessage {
                    thread: row.get(1),
                    message_id: row.get(2),
                    sent_at: row.get(3),
                },
            )
        })
        .collect();
    let destination = |message: i64| -> Destination {
        moved
            .get(&message)
            .map(|d| (*d).clone())
            .unwrap_or_else(|| Destination::Thread(messages[&message].thread))
    };
    let roots: HashMap<i64, &str> = group
        .threads
        .iter()
        .map(|t| (t.id, t.root.as_str()))
        .collect();
    // Where what was a thread's goes: where its root message goes, else
    // most of its messages.
    let successor = |thread: i64| -> Destination {
        let in_thread = || messages.iter().filter(move |(_, m)| m.thread == thread);
        if let Some((id, _)) =
            in_thread().find(|(_, m)| Some(m.message_id.as_str()) == roots.get(&thread).copied())
        {
            return destination(*id);
        }
        let mut counts: BTreeMap<Destination, usize> = BTreeMap::new();
        for (id, _) in in_thread() {
            *counts.entry(destination(*id)).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map_or(Destination::Thread(thread), |(d, _)| d)
    };
    let mut citations: HashMap<(String, i64), Vec<i64>> = HashMap::new();
    for row in client
        .query(
            "SELECT c.extract_kind, c.extract_id, c.message_id FROM citations c
             JOIN messages m ON m.id = c.message_id WHERE m.thread_id = ANY ($1)
             ORDER BY c.message_id",
            &[&sources],
        )
        .await?
    {
        citations
            .entry((row.get(0), row.get(1)))
            .or_default()
            .push(row.get(2));
    }
    // Where an extract on `from` goes: the thread with most of its
    // citations there, the earliest cited message's on a tie; without
    // citations there, it stays, or follows a thread that goes.
    let home = |from: i64, kind: &str, id: i64| -> Destination {
        let cited: Vec<i64> = citations
            .get(&(kind.to_owned(), id))
            .into_iter()
            .flatten()
            .copied()
            .filter(|m| messages[m].thread == from)
            .collect();
        if cited.is_empty() {
            return if deleted.contains(&from) {
                successor(from)
            } else {
                Destination::Thread(from)
            };
        }
        let mut counts: BTreeMap<Destination, usize> = BTreeMap::new();
        for m in &cited {
            *counts.entry(destination(*m)).or_default() += 1;
        }
        let most = counts.values().copied().max().unwrap_or(0);
        cited
            .iter()
            .min_by_key(|m| {
                let at = messages[m].sent_at;
                (counts[&destination(**m)] != most, at.is_none(), at, **m)
            })
            .map(|m| destination(*m))
            .expect("cited messages")
    };

    // Decisions and positions: (kind, id, thread, feature, where it goes).
    let mut homes: Vec<(&str, i64, i64, Option<i64>, Destination)> = Vec::new();
    for kind in ["decision", "position"] {
        for row in client
            .query(
                &format!(
                    "SELECT id, thread_id, feature_id FROM {kind}s WHERE thread_id = ANY ($1) ORDER BY id"
                ),
                &[&sources],
            )
            .await?
        {
            let (id, from): (i64, i64) = (row.get(0), row.get(1));
            let to = home(from, kind, id);
            homes.push((kind, id, from, row.get(2), to.clone()));
            if to != Destination::Thread(from) {
                let moves = if kind == "decision" {
                    &mut out.decisions
                } else {
                    &mut out.positions
                };
                moves.push(ExtractMove { id, from, to });
            }
        }
    }

    // A feature is linked to each thread that has its citations, decisions
    // or positions from the thread it was linked to. A feature merged into
    // another counts what its merge moved, which undo_merge moves back.
    let linked: Vec<(i64, i64)> = client
        .query(
            "SELECT feature_id, thread_id FROM feature_threads WHERE thread_id = ANY ($1)
             ORDER BY feature_id, thread_id",
            &[&sources],
        )
        .await?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let features: Vec<i64> = linked
        .iter()
        .map(|(f, _)| *f)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut merged_citations: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut merged_extracts: HashSet<(i64, String, i64)> = HashSet::new();
    for row in client
        .query(
            "SELECT l.from_id, 'citation', c.message_id FROM merge_log l
                 CROSS JOIN LATERAL jsonb_array_elements_text(l.moved -> 'citations') x (id)
                 JOIN citations c ON c.id = x.id::bigint
             WHERE l.kind = 'feature' AND l.reverted_at IS NULL AND l.from_id = ANY ($1)
             UNION ALL
             SELECT l.from_id, k.kind, x.id::bigint FROM merge_log l
                 CROSS JOIN LATERAL (VALUES ('decision', l.moved -> 'decisions'),
                                            ('position', l.moved -> 'positions')) k (kind, ids)
                 CROSS JOIN LATERAL jsonb_array_elements_text(k.ids) x (id)
             WHERE l.kind = 'feature' AND l.reverted_at IS NULL AND l.from_id = ANY ($1)
             ORDER BY 1, 2, 3",
            &[&features],
        )
        .await?
    {
        let (feature, kind, id): (i64, String, i64) = (row.get(0), row.get(1), row.get(2));
        if kind == "citation" {
            merged_citations.entry(feature).or_default().push(id);
        } else {
            merged_extracts.insert((feature, kind, id));
        }
    }
    // The messages of thread `from` that cite feature `feature`.
    let feature_cites = |feature: i64, from: i64| -> Vec<i64> {
        citations
            .get(&("feature".to_owned(), feature))
            .into_iter()
            .flatten()
            .chain(merged_citations.get(&feature).into_iter().flatten())
            .copied()
            .filter(|m| messages.get(m).is_some_and(|x| x.thread == from))
            .collect()
    };
    let mut link_homes: HashMap<(i64, i64), BTreeSet<Destination>> = HashMap::new();
    for &(feature, from) in &linked {
        let mut to: BTreeSet<Destination> = feature_cites(feature, from)
            .into_iter()
            .map(&destination)
            .collect();
        to.extend(
            homes
                .iter()
                .filter(|(kind, id, t, f, _)| {
                    *t == from
                        && (*f == Some(feature)
                            || merged_extracts.contains(&(feature, (*kind).to_owned(), *id)))
                })
                .map(|(_, _, _, _, d)| d.clone()),
        );
        if to.is_empty() {
            to.insert(if deleted.contains(&from) {
                successor(from)
            } else {
                Destination::Thread(from)
            });
        }
        let here = Destination::Thread(from);
        if to.len() != 1 || !to.contains(&here) {
            let keep = to.remove(&here) && !deleted.contains(&from);
            out.feature_links.push(FeatureLinks {
                feature,
                from,
                to: to.clone(),
                keep,
            });
            if keep {
                to.insert(here);
            }
        }
        link_homes.insert((feature, from), to);
    }

    // A feature's file goes to its linked thread whose messages show the
    // path most, else has most of its citations.
    let mut shows: HashMap<String, Vec<i64>> = HashMap::new();
    for row in client
        .query(
            "SELECT m.id, x.path FROM patches p JOIN messages m ON m.id = p.message_id,
                 unnest(p.paths) AS x (path)
             WHERE m.thread_id = ANY ($1)
             UNION ALL
             SELECT m.id, a.value FROM anchors a JOIN messages m ON m.id = a.message_id
             WHERE m.thread_id = ANY ($1) AND a.kind = 'file'",
            &[&sources],
        )
        .await?
    {
        shows.entry(row.get(1)).or_default().push(row.get(0));
    }
    for row in client
        .query(
            "SELECT feature_id, path, thread_id FROM feature_files WHERE thread_id = ANY ($1)
             ORDER BY feature_id, path",
            &[&sources],
        )
        .await?
    {
        let (feature, path, from): (i64, String, i64) = (row.get(0), row.get(1), row.get(2));
        let candidates: BTreeSet<Destination> = link_homes
            .get(&(feature, from))
            .cloned()
            .unwrap_or_else(|| {
                BTreeSet::from([if deleted.contains(&from) {
                    successor(from)
                } else {
                    Destination::Thread(from)
                }])
            });
        let score = |d: &Destination| {
            let showing = shows
                .get(&path)
                .into_iter()
                .flatten()
                .filter(|m| messages[*m].thread == from && destination(**m) == *d)
                .count();
            let citing = feature_cites(feature, from)
                .into_iter()
                .filter(|m| destination(*m) == *d)
                .count();
            (showing, citing)
        };
        let to = candidates
            .iter()
            .max_by(|a, b| score(a).cmp(&score(b)).then_with(|| b.cmp(a)))
            .cloned()
            .expect("a thread for the file");
        if to != Destination::Thread(from) {
            out.feature_files.push(FeatureFile {
                feature,
                path,
                from,
                to,
            });
        }
    }

    // Drafts of threads that go follow them; a feature merge's record of
    // the threads it linked follows the links.
    let gone: Vec<i64> = deleted.iter().copied().collect();
    for row in client
        .query(
            "SELECT id, thread_id FROM drafts WHERE thread_id = ANY ($1) ORDER BY id",
            &[&gone],
        )
        .await?
    {
        let from: i64 = row.get(1);
        out.drafts.push(ExtractMove {
            id: row.get(0),
            from,
            to: successor(from),
        });
    }
    let merges: Vec<(i64, i64, Vec<i64>)> = client
        .query(
            "SELECT id, into_id, moved -> 'threads_added' FROM merge_log
             WHERE kind = 'feature' AND reverted_at IS NULL
               AND EXISTS (SELECT FROM jsonb_array_elements(moved -> 'threads_added') e
                           WHERE e::bigint = ANY ($1))
             ORDER BY id",
            &[&sources],
        )
        .await?
        .iter()
        .map(|row| {
            let added: Vec<i64> =
                serde_json::from_value(row.get::<_, Value>(2)).unwrap_or_default();
            (row.get(0), row.get(1), added)
        })
        .collect();
    let into: Vec<i64> = merges.iter().map(|(_, f, _)| *f).collect();
    let mut links_now: HashMap<i64, HashSet<i64>> = HashMap::new();
    for row in client
        .query(
            "SELECT feature_id, thread_id FROM feature_threads WHERE feature_id = ANY ($1)",
            &[&into],
        )
        .await?
    {
        links_now.entry(row.get(0)).or_default().insert(row.get(1));
    }
    for (merge, feature, added) in &merges {
        for &from in added.iter().filter(|t| sources.contains(t)) {
            let Some(homes) = link_homes.get(&(*feature, from)) else {
                continue;
            };
            // What undo_merge unlinks: only links the merge added, not ones
            // the feature had besides.
            let to: BTreeSet<Destination> = homes
                .iter()
                .filter(|d| match d {
                    Destination::New(_) => true,
                    Destination::Thread(t) => {
                        added.contains(t) || !links_now.get(feature).is_some_and(|l| l.contains(t))
                    }
                })
                .cloned()
                .collect();
            if to != BTreeSet::from([Destination::Thread(from)]) {
                out.merges.push(MergeLinks {
                    merge: *merge,
                    feature: *feature,
                    from,
                    to,
                });
            }
        }
    }
    Ok(out)
}

/// Moves the extracts as planned, once the group's new threads exist.
async fn apply_extracts(
    client: &impl GenericClient,
    moves: &ExtractMoves,
    created: &BTreeMap<String, i64>,
) -> Result<()> {
    let id = |d: &Destination| rethread::resolve(d, created);
    for m in &moves.decisions {
        client
            .execute(
                "UPDATE decisions SET thread_id = $2, updated_at = now() WHERE id = $1 AND thread_id = $3",
                &[&m.id, &id(&m.to), &m.from],
            )
            .await?;
    }
    for m in &moves.positions {
        client
            .execute(
                "UPDATE positions SET thread_id = $2 WHERE id = $1 AND thread_id = $3",
                &[&m.id, &id(&m.to), &m.from],
            )
            .await?;
    }
    for link in &moves.feature_links {
        for to in &link.to {
            client
                .execute(
                    "INSERT INTO feature_threads (feature_id, thread_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    &[&link.feature, &id(to)],
                )
                .await?;
        }
        if !link.keep {
            client
                .execute(
                    "DELETE FROM feature_threads WHERE feature_id = $1 AND thread_id = $2",
                    &[&link.feature, &link.from],
                )
                .await?;
        }
    }
    for file in &moves.feature_files {
        client
            .execute(
                "UPDATE feature_files SET thread_id = $4 WHERE feature_id = $1 AND path = $2 AND thread_id = $3",
                &[&file.feature, &file.path, &file.from, &id(&file.to)],
            )
            .await?;
    }
    for draft in &moves.drafts {
        client
            .execute(
                "UPDATE drafts SET thread_id = $2 WHERE id = $1",
                &[&draft.id, &id(&draft.to)],
            )
            .await?;
    }
    for merge in &moves.merges {
        let added: Value = client
            .query_one(
                "SELECT moved -> 'threads_added' FROM merge_log WHERE id = $1 FOR UPDATE",
                &[&merge.merge],
            )
            .await?
            .get(0);
        let mut threads: Vec<i64> = serde_json::from_value(added).unwrap_or_default();
        threads.retain(|t| *t != merge.from);
        threads.extend(merge.to.iter().map(id));
        threads.sort_unstable();
        threads.dedup();
        client
            .execute(
                "UPDATE merge_log SET moved = jsonb_set(moved, '{threads_added}', $2) WHERE id = $1",
                &[&merge.merge, &json!(threads)],
            )
            .await?;
    }
    Ok(())
}
