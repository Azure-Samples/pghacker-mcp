//! Projects the tables into the AGE graph `pghacker_graph`. Writes go straight
//! into AGE's label tables, which is much faster than Cypher MERGE, and
//! `graph_nodes` and `graph_edges` record which vertex and edge each key
//! became. The graph never reads `author_emails`. Text is as its table
//! stores it, email addresses included; mail stored before ingest kept them
//! shows `[email]` until `pghacker-indexer restore-addresses` restores it.
//!
//! Triggers on the tables record in `graph_changes` the keys of the vertices
//! and edges that each change may affect (migration 0010). The commit
//! history's tables, whose commits and file versions give the Commit, File
//! and Subsystem vertices and their edges, have none: its writers record
//! what they change themselves, with migration 0014's functions.
//! [`sync_changes`] recomputes just those from the tables, in bounded
//! batches:
//!
//! - A vertex is wanted if its row exists and isn't merged; a file, release,
//!   subsystem, commit or CommitFest entry if any row names it. A file is
//!   `loaded` when the commit history has it.
//! - An edge is wanted if a row implies it and both its vertices are in the
//!   graph.
//! - A vertex that leaves the graph takes its edges with it, and one that
//!   enters it brings the edges that were waiting for it, such as a reply's to
//!   a parent that arrived later.
//! - A commit's COMMITTED edge comes from the one author with the commit's
//!   author name, so the authors' names decide it: `graph_committers` keeps
//!   each name's author, and `COMMITTER_CHANGES` compares it in each batch.
//!
//! [`sync`] recomputes the whole graph and diffs it against the bookkeeping:
//! once the change capture is installed, after a TRUNCATE (as a restore does),
//! on request, and as a periodic reconcile. [`run`] decides which, for the job.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio_postgres::{Client, GenericClient, Transaction};

use pghacker_core::{EDGE_LABELS, GRAPH, VERTEX_LABELS};

/// The change entries that a batch applies at most.
pub const BATCH: i64 = 1000;

/// What the graph projects, numbered: raise it when a change to the
/// projection needs a full sync to apply. A full sync records it in
/// `graph_sync_state`, and [`run`] runs one while the graph has an older one.
pub const PROJECTION: i32 = 2;

#[derive(Debug, Default, Serialize)]
pub struct SyncStats {
    /// `full` or `incremental`.
    pub mode: &'static str,
    /// The change entries applied: a batch's, or those a full sync covered.
    pub changes: u64,
    pub nodes_added: u64,
    pub nodes_updated: u64,
    pub nodes_removed: u64,
    pub edges_added: u64,
    pub edges_removed: u64,
    pub ms: u64,
}

impl SyncStats {
    /// The vertices and edges added, updated or removed.
    pub fn changed(&self) -> u64 {
        self.nodes_added
            + self.nodes_updated
            + self.nodes_removed
            + self.edges_added
            + self.edges_removed
    }
}

const LOCK: &str = "SELECT pg_advisory_xact_lock(hashtext('pghacker.graph_sync'))";

// Each vertex's properties and each edge's keys, shared by the full sync and
// the batches. The triggers in migration 0010 build the same keys.

macro_rules! author_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'author:' || a.id, 'id', a.id, 'name', a.display_name, 'messages', a.message_count,
    'first_seen', a.first_seen, 'last_seen', a.last_seen))"
    };
}

macro_rules! thread_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'thread:' || t.id, 'id', t.id, 'subject', t.subject, 'root_message_id', t.root_message_id,
    'messages', t.message_count, 'started_at', t.started_at, 'last_at', t.last_at))"
    };
}

macro_rules! message_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'message:' || m.message_id, 'id', m.id, 'message_id', m.message_id, 'subject', m.subject,
    'sent_at', m.sent_at, 'has_patch', m.has_patch, 'thread_id', m.thread_id, 'author_id', m.author_id))"
    };
}

macro_rules! feature_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'feature:' || f.slug, 'id', f.id, 'slug', f.slug, 'name', f.name, 'status', f.status,
    'release', f.release))"
    };
}

macro_rules! decision_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'decision:' || d.id, 'id', d.id, 'title', d.title, 'outcome', d.outcome,
    'decided_at', d.decided_at, 'thread_id', d.thread_id))"
    };
}

macro_rules! position_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'position:' || p.id, 'id', p.id, 'stance', p.stance, 'summary', p.summary,
    'author_id', p.author_id, 'thread_id', p.thread_id))"
    };
}

macro_rules! release_props {
    () => {
        "jsonb_build_object('key', 'release:' || r.v, 'version', r.v)"
    };
}

macro_rules! subsystem_props {
    () => {
        "jsonb_build_object('key', 'subsystem:' || x.subsystem, 'name', x.subsystem)"
    };
}

macro_rules! file_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'file:' || p.path, 'path', p.path, 'subsystem', p.subsystem, 'loaded', p.loaded))"
    };
}

/// The properties of commit `x.value`, from `commit_rows`: only its key and
/// SHA when only mail names it.
macro_rules! commit_props {
    () => {
        "jsonb_strip_nulls(jsonb_build_object(
    'key', 'commit:' || x.value, 'sha', x.value, 'subject', c.subject, 'committed_at', c.committed_at,
    'author', c.author_name, 'branch', l.name, 'backpatch', r.sha))"
    };
}

/// The history's row of commit `x.value`, its lane, and its back-patch
/// group's first commit.
macro_rules! commit_rows {
    () => {
        "LEFT JOIN code_commits c ON c.sha = x.value LEFT JOIN code_lanes l ON l.id = c.lane
LEFT JOIN code_commits r ON r.addr = c.backpatch"
    };
}

macro_rules! cf_props {
    () => {
        "jsonb_build_object(
    'key', 'cf:' || x.value, 'entry', x.value, 'url', 'https://commitfest.postgresql.org/patch/' || x.value || '/')"
    };
}

/// The key of the vertex that anchor `a` mentions: a commit by its full SHA
/// once resolve_anchors has found it in the history, else as written.
macro_rules! anchor_key {
    () => {
        "CASE a.kind WHEN 'file' THEN 'file:' || a.value WHEN 'commit' THEN 'commit:' || coalesce(a.resolved, a.value)
     WHEN 'commitfest' THEN 'cf:' || a.value ELSE 'release:' || a.value END"
    };
}

/// The lower-cased names of the authors whose keys are in a text[]
/// parameter: their display names and the names they sent mail under.
macro_rules! names_of {
    ($param:literal) => {
        concat!(
            "SELECT DISTINCT lower(n.name) AS name FROM (
        SELECT a.display_name AS name FROM authors a WHERE a.id = ANY (",
            ids!("author:", $param),
            ")
        UNION ALL SELECT an.name FROM author_names an WHERE an.author_id = ANY (",
            ids!("author:", $param),
            ")) n"
        )
    };
}

/// The key of the extract that citation `c` supports; NULL for a feature that
/// doesn't exist.
macro_rules! extract_key {
    () => {
        "CASE c.extract_kind WHEN 'feature' THEN 'feature:' || f.slug ELSE c.extract_kind || ':' || c.extract_id END"
    };
}

/// The numeric IDs in the keys with a prefix, from a text[] parameter.
macro_rules! ids {
    ($prefix:literal, $param:literal) => {
        concat!(
            "ARRAY(SELECT substr(k, length('",
            $prefix,
            "') + 1)::bigint FROM unnest(",
            $param,
            "::text[]) k WHERE k ~ '^",
            $prefix,
            "[0-9]+$')"
        )
    };
}

/// The names in the keys with a prefix, from a text[] parameter.
macro_rules! names {
    ($prefix:literal, $param:literal) => {
        concat!(
            "ARRAY(SELECT substr(k, length('",
            $prefix,
            "') + 1) FROM unnest(",
            $param,
            "::text[]) k WHERE starts_with(k, '",
            $prefix,
            "'))"
        )
    };
}

const WANT_NODES: &str = concat!(
    "INSERT INTO want_nodes (key, label, props)
SELECT 'author:' || a.id, 'Author', ",
    author_props!(),
    "
FROM authors a WHERE a.merged_into IS NULL
UNION ALL
SELECT 'thread:' || t.id, 'Thread', ",
    thread_props!(),
    "
FROM threads t
UNION ALL
SELECT 'message:' || m.message_id, 'Message', ",
    message_props!(),
    "
FROM messages m
UNION ALL
SELECT 'feature:' || f.slug, 'Feature', ",
    feature_props!(),
    "
FROM features f WHERE f.merged_into IS NULL
UNION ALL
SELECT 'decision:' || d.id, 'Decision', ",
    decision_props!(),
    "
FROM decisions d WHERE d.merged_into IS NULL
UNION ALL
SELECT 'position:' || p.id, 'Position', ",
    position_props!(),
    "
FROM positions p
UNION ALL
SELECT 'release:' || r.v, 'Release', ",
    release_props!(),
    "
FROM (SELECT release AS v FROM features WHERE release IS NOT NULL AND merged_into IS NULL
      UNION SELECT value FROM anchors WHERE kind = 'release') r
UNION ALL
SELECT 'subsystem:' || x.subsystem, 'Subsystem', ",
    subsystem_props!(),
    "
FROM (SELECT DISTINCT v.subsystem FROM code_file_versions v WHERE v.subsystem IS NOT NULL) x
UNION ALL
SELECT 'file:' || p.path, 'File', ",
    file_props!(),
    "
FROM (
    SELECT path, max(subsystem) AS subsystem, bool_or(loaded) AS loaded FROM (
        SELECT v.path, max(v.subsystem) AS subsystem, true AS loaded FROM code_file_versions v GROUP BY v.path
        UNION ALL SELECT a.value, NULL, false FROM anchors a WHERE a.kind = 'file'
        UNION ALL SELECT ff.path, NULL, false FROM feature_files ff
    ) x GROUP BY path
) p
UNION ALL
SELECT 'commit:' || x.value, 'Commit', ",
    commit_props!(),
    "
FROM (SELECT sha AS value FROM code_commits
      UNION SELECT coalesce(resolved, value) FROM anchors WHERE kind = 'commit') x
",
    commit_rows!(),
    "
UNION ALL
SELECT 'cf:' || x.value, 'CommitFestEntry', ",
    cf_props!(),
    "
FROM (SELECT DISTINCT value FROM anchors WHERE kind = 'commitfest') x
ON CONFLICT (key) DO NOTHING"
);

const WANT_EDGES: &str = concat!(
    "INSERT INTO want_edges (label, start_key, end_key)
SELECT 'AUTHORED', 'author:' || m.author_id, 'message:' || m.message_id FROM messages m WHERE m.author_id IS NOT NULL
UNION ALL
SELECT 'REPLIES_TO', 'message:' || m.message_id, 'message:' || m.parent_message_id
FROM messages m WHERE m.parent_message_id IS NOT NULL
UNION ALL
SELECT 'IN_THREAD', 'message:' || m.message_id, 'thread:' || m.thread_id FROM messages m
UNION ALL
SELECT 'MENTIONS', 'message:' || m.message_id, ", anchor_key!(), "
FROM anchors a JOIN messages m ON m.id = a.message_id
UNION ALL
SELECT DISTINCT 'PART_OF', 'file:' || v.path, 'subsystem:' || v.subsystem
FROM code_file_versions v WHERE v.subsystem IS NOT NULL
UNION ALL
SELECT 'DISCUSSES', 'thread:' || ft.thread_id, 'feature:' || f.slug
FROM feature_threads ft JOIN features f ON f.id = ft.feature_id
UNION ALL
SELECT 'DECIDES', 'decision:' || d.id, 'feature:' || f.slug
FROM decisions d JOIN features f ON f.id = d.feature_id
UNION ALL
SELECT 'TARGETS', 'feature:' || f.slug, 'release:' || f.release FROM features f WHERE f.release IS NOT NULL
UNION ALL
SELECT 'IMPLEMENTS', 'file:' || ff.path, 'feature:' || f.slug
FROM feature_files ff JOIN features f ON f.id = ff.feature_id
UNION ALL
SELECT 'HOLDS', 'author:' || p.author_id, 'position:' || p.id FROM positions p
UNION ALL
SELECT 'ABOUT', 'position:' || p.id, 'feature:' || f.slug FROM positions p JOIN features f ON f.id = p.feature_id
UNION ALL
SELECT 'ABOUT', 'position:' || p.id, 'decision:' || p.decision_id FROM positions p WHERE p.decision_id IS NOT NULL
UNION ALL
SELECT 'EVIDENCED_BY', ", extract_key!(), ", 'message:' || m.message_id
FROM citations c JOIN messages m ON m.id = c.message_id
LEFT JOIN features f ON c.extract_kind = 'feature' AND f.id = c.extract_id
WHERE c.extract_kind <> 'feature' OR f.id IS NOT NULL
UNION ALL
SELECT 'MODIFIES', 'commit:' || c.sha, 'file:' || f.path FROM code_commit_files f JOIN code_commits c ON c.addr = f.addr
UNION ALL
SELECT DISTINCT 'DISCUSSED_IN', 'commit:' || t.sha, 'thread:' || t.thread_id FROM code_commit_threads t
UNION ALL
SELECT 'BACKPATCH_OF', 'commit:' || c.sha, 'commit:' || r.sha
FROM code_commits c JOIN code_commits r ON r.addr = c.backpatch WHERE r.addr <> c.addr
UNION ALL
SELECT 'COMMITTED', w.author_key, 'commit:' || c.sha
FROM want_committers w JOIN code_commits c ON lower(c.author_name) = w.name
ON CONFLICT DO NOTHING"
);

/// Each author name in the history, lower-cased, with the key of the one
/// author that has it (`graph_committer`, migration 0014). A full sync takes
/// both its COMMITTED edges and `graph_committers` from this one reading.
const WANT_COMMITTERS: &str = "CREATE TEMP TABLE want_committers ON COMMIT DROP AS
SELECT n.name, 'author:' || g.author_id AS author_key
FROM (SELECT DISTINCT lower(author_name) AS name FROM code_commits) n
CROSS JOIN LATERAL graph_committer(n.name) g";

/// The wanted vertices among the keys in $1, as WANT_NODES computes them.
const KEYED_NODES: &str = concat!(
    "SELECT 'author:' || a.id, 'Author', ",
    author_props!(),
    "
FROM authors a WHERE a.merged_into IS NULL AND a.id = ANY (",
    ids!("author:", "$1"),
    ")
UNION ALL
SELECT 'thread:' || t.id, 'Thread', ",
    thread_props!(),
    "
FROM threads t WHERE t.id = ANY (",
    ids!("thread:", "$1"),
    ")
UNION ALL
SELECT 'message:' || m.message_id, 'Message', ",
    message_props!(),
    "
FROM messages m WHERE m.message_id = ANY (",
    names!("message:", "$1"),
    ")
UNION ALL
SELECT 'feature:' || f.slug, 'Feature', ",
    feature_props!(),
    "
FROM features f WHERE f.merged_into IS NULL AND f.slug = ANY (",
    names!("feature:", "$1"),
    ")
UNION ALL
SELECT 'decision:' || d.id, 'Decision', ",
    decision_props!(),
    "
FROM decisions d WHERE d.merged_into IS NULL AND d.id = ANY (",
    ids!("decision:", "$1"),
    ")
UNION ALL
SELECT 'position:' || p.id, 'Position', ",
    position_props!(),
    "
FROM positions p WHERE p.id = ANY (",
    ids!("position:", "$1"),
    ")
UNION ALL
SELECT 'release:' || r.v, 'Release', ",
    release_props!(),
    "
FROM unnest(",
    names!("release:", "$1"),
    ") r (v)
WHERE EXISTS (SELECT FROM features f WHERE f.release = r.v AND f.merged_into IS NULL)
   OR EXISTS (SELECT FROM anchors a WHERE a.kind = 'release' AND a.value = r.v)
UNION ALL
SELECT 'subsystem:' || x.subsystem, 'Subsystem', ",
    subsystem_props!(),
    "
FROM unnest(",
    names!("subsystem:", "$1"),
    ") x (subsystem)
WHERE EXISTS (SELECT FROM code_file_versions v WHERE v.subsystem = x.subsystem)
UNION ALL
SELECT 'file:' || p.path, 'File', ",
    file_props!(),
    "
FROM (SELECT x.path, c.subsystem, c.loaded FROM unnest(",
    names!("file:", "$1"),
    ") x (path)
      CROSS JOIN LATERAL (SELECT max(v.subsystem) AS subsystem, count(*) > 0 AS loaded
                          FROM code_file_versions v WHERE v.path = x.path) c) p
WHERE p.loaded OR EXISTS (SELECT FROM anchors a WHERE a.kind = 'file' AND a.value = p.path)
   OR EXISTS (SELECT FROM feature_files ff WHERE ff.path = p.path)
UNION ALL
SELECT 'commit:' || x.value, 'Commit', ",
    commit_props!(),
    "
FROM unnest(",
    names!("commit:", "$1"),
    ") x (value)
",
    commit_rows!(),
    "
WHERE c.addr IS NOT NULL
   OR EXISTS (SELECT FROM anchors a WHERE a.kind = 'commit' AND coalesce(a.resolved, a.value) = x.value)
UNION ALL
SELECT 'cf:' || x.value, 'CommitFestEntry', ",
    cf_props!(),
    "
FROM unnest(",
    names!("cf:", "$1"),
    ") x (value)
WHERE EXISTS (SELECT FROM anchors a WHERE a.kind = 'commitfest' AND a.value = x.value)"
);

/// An edge label's rows, as WANT_EDGES reads them.
struct EdgeSource {
    label: &'static str,
    /// The key prefixes of its start and end vertices.
    starts: &'static [&'static str],
    ends: &'static [&'static str],
    /// Whether an edge's rows are found by its end key rather than its start
    /// key: the key with the fewest edges, such as the message of AUTHORED.
    by_end: bool,
    /// Its edges (start key, end key) that start at a key in $1 or end at one
    /// in $2.
    sql: &'static str,
}

const EDGE_SOURCES: &[EdgeSource] = &[
    EdgeSource {
        label: "AUTHORED",
        starts: &["author:"],
        ends: &["message:"],
        by_end: true,
        sql: concat!(
            "SELECT 'author:' || m.author_id, 'message:' || m.message_id FROM messages m
WHERE m.author_id = ANY (", ids!("author:", "$1"), ")
UNION ALL
SELECT 'author:' || m.author_id, 'message:' || m.message_id FROM messages m
WHERE m.author_id IS NOT NULL AND m.message_id = ANY (", names!("message:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "REPLIES_TO",
        starts: &["message:"],
        ends: &["message:"],
        by_end: false,
        sql: concat!(
            "SELECT 'message:' || m.message_id, 'message:' || m.parent_message_id FROM messages m
WHERE m.parent_message_id IS NOT NULL AND m.message_id = ANY (", names!("message:", "$1"), ")
UNION ALL
SELECT 'message:' || m.message_id, 'message:' || m.parent_message_id FROM messages m
WHERE m.parent_message_id = ANY (", names!("message:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "IN_THREAD",
        starts: &["message:"],
        ends: &["thread:"],
        by_end: false,
        sql: concat!(
            "SELECT 'message:' || m.message_id, 'thread:' || m.thread_id FROM messages m
WHERE m.message_id = ANY (", names!("message:", "$1"), ")
UNION ALL
SELECT 'message:' || m.message_id, 'thread:' || m.thread_id FROM messages m
WHERE m.thread_id = ANY (", ids!("thread:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "MENTIONS",
        starts: &["message:"],
        ends: &["file:", "commit:", "cf:", "release:"],
        by_end: false,
        sql: concat!(
            "SELECT 'message:' || m.message_id, ", anchor_key!(), "
FROM messages m JOIN anchors a ON a.message_id = m.id
WHERE m.message_id = ANY (", names!("message:", "$1"), ")
UNION ALL
SELECT 'message:' || m.message_id, ", anchor_key!(), "
FROM (SELECT CASE WHEN starts_with(k, 'file:') THEN 'file' WHEN starts_with(k, 'cf:') THEN 'commitfest'
                  WHEN starts_with(k, 'release:') THEN 'release' END AS kind,
             substr(k, strpos(k, ':') + 1) AS value
      FROM unnest($2::text[]) k) t
JOIN anchors a ON a.kind = t.kind AND a.value = t.value
JOIN messages m ON m.id = a.message_id
UNION ALL
SELECT 'message:' || m.message_id, ", anchor_key!(), "
FROM anchors a JOIN messages m ON m.id = a.message_id
WHERE a.kind = 'commit' AND coalesce(a.resolved, a.value) = ANY (", names!("commit:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "PART_OF",
        starts: &["file:"],
        ends: &["subsystem:"],
        by_end: false,
        sql: concat!(
            "SELECT DISTINCT 'file:' || v.path, 'subsystem:' || v.subsystem FROM code_file_versions v
WHERE v.subsystem IS NOT NULL AND v.path = ANY (", names!("file:", "$1"), ")
UNION ALL
SELECT DISTINCT 'file:' || v.path, 'subsystem:' || v.subsystem FROM code_file_versions v
WHERE v.subsystem = ANY (", names!("subsystem:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "DISCUSSES",
        starts: &["thread:"],
        ends: &["feature:"],
        by_end: false,
        sql: concat!(
            "SELECT 'thread:' || ft.thread_id, 'feature:' || f.slug
FROM feature_threads ft JOIN features f ON f.id = ft.feature_id
WHERE ft.thread_id = ANY (", ids!("thread:", "$1"), ")
UNION ALL
SELECT 'thread:' || ft.thread_id, 'feature:' || f.slug
FROM features f JOIN feature_threads ft ON ft.feature_id = f.id
WHERE f.slug = ANY (", names!("feature:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "DECIDES",
        starts: &["decision:"],
        ends: &["feature:"],
        by_end: false,
        sql: concat!(
            "SELECT 'decision:' || d.id, 'feature:' || f.slug FROM decisions d JOIN features f ON f.id = d.feature_id
WHERE d.id = ANY (", ids!("decision:", "$1"), ")
UNION ALL
SELECT 'decision:' || d.id, 'feature:' || f.slug FROM features f JOIN decisions d ON d.feature_id = f.id
WHERE f.slug = ANY (", names!("feature:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "TARGETS",
        starts: &["feature:"],
        ends: &["release:"],
        by_end: false,
        sql: concat!(
            "SELECT 'feature:' || f.slug, 'release:' || f.release FROM features f
WHERE f.release IS NOT NULL AND f.slug = ANY (", names!("feature:", "$1"), ")
UNION ALL
SELECT 'feature:' || f.slug, 'release:' || f.release FROM features f
WHERE f.release = ANY (", names!("release:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "IMPLEMENTS",
        starts: &["file:"],
        ends: &["feature:"],
        by_end: true,
        sql: concat!(
            "SELECT 'file:' || ff.path, 'feature:' || f.slug FROM feature_files ff JOIN features f ON f.id = ff.feature_id
WHERE ff.path = ANY (", names!("file:", "$1"), ")
UNION ALL
SELECT 'file:' || ff.path, 'feature:' || f.slug FROM features f JOIN feature_files ff ON ff.feature_id = f.id
WHERE f.slug = ANY (", names!("feature:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "HOLDS",
        starts: &["author:"],
        ends: &["position:"],
        by_end: true,
        sql: concat!(
            "SELECT 'author:' || p.author_id, 'position:' || p.id FROM positions p
WHERE p.author_id = ANY (", ids!("author:", "$1"), ")
UNION ALL
SELECT 'author:' || p.author_id, 'position:' || p.id FROM positions p
WHERE p.id = ANY (", ids!("position:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "ABOUT",
        starts: &["position:"],
        ends: &["feature:", "decision:"],
        by_end: false,
        sql: concat!(
            "SELECT 'position:' || p.id, 'feature:' || f.slug FROM positions p JOIN features f ON f.id = p.feature_id
WHERE p.id = ANY (", ids!("position:", "$1"), ")
UNION ALL
SELECT 'position:' || p.id, 'decision:' || p.decision_id FROM positions p
WHERE p.decision_id IS NOT NULL AND p.id = ANY (", ids!("position:", "$1"), ")
UNION ALL
SELECT 'position:' || p.id, 'feature:' || f.slug FROM features f JOIN positions p ON p.feature_id = f.id
WHERE f.slug = ANY (", names!("feature:", "$2"), ")
UNION ALL
SELECT 'position:' || p.id, 'decision:' || p.decision_id FROM positions p
WHERE p.decision_id = ANY (", ids!("decision:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "EVIDENCED_BY",
        starts: &["feature:", "decision:", "position:"],
        ends: &["message:"],
        by_end: true,
        sql: concat!(
            "SELECT ", extract_key!(), ", 'message:' || m.message_id
FROM citations c JOIN messages m ON m.id = c.message_id
LEFT JOIN features f ON c.extract_kind = 'feature' AND f.id = c.extract_id
WHERE (c.extract_kind = 'feature'
       AND c.extract_id = ANY (ARRAY(SELECT x.id FROM features x WHERE x.slug = ANY (", names!("feature:", "$1"), "))))
   OR (c.extract_kind = 'decision' AND c.extract_id = ANY (", ids!("decision:", "$1"), "))
   OR (c.extract_kind = 'position' AND c.extract_id = ANY (", ids!("position:", "$1"), "))
UNION ALL
SELECT ", extract_key!(), ", 'message:' || m.message_id
FROM messages m JOIN citations c ON c.message_id = m.id
LEFT JOIN features f ON c.extract_kind = 'feature' AND f.id = c.extract_id
WHERE m.message_id = ANY (", names!("message:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "MODIFIES",
        starts: &["commit:"],
        ends: &["file:"],
        by_end: false,
        sql: concat!(
            "SELECT 'commit:' || c.sha, 'file:' || f.path FROM code_commits c JOIN code_commit_files f ON f.addr = c.addr
WHERE c.sha = ANY (", names!("commit:", "$1"), ")
UNION ALL
SELECT 'commit:' || c.sha, 'file:' || f.path FROM code_commit_files f JOIN code_commits c ON c.addr = f.addr
WHERE f.path = ANY (", names!("file:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "DISCUSSED_IN",
        starts: &["commit:"],
        ends: &["thread:"],
        by_end: false,
        sql: concat!(
            "SELECT DISTINCT 'commit:' || t.sha, 'thread:' || t.thread_id FROM code_commit_threads t
WHERE t.sha = ANY (", names!("commit:", "$1"), ")
UNION ALL
SELECT DISTINCT 'commit:' || t.sha, 'thread:' || t.thread_id FROM code_commit_threads t
WHERE t.thread_id = ANY (", ids!("thread:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "BACKPATCH_OF",
        starts: &["commit:"],
        ends: &["commit:"],
        by_end: false,
        sql: concat!(
            "SELECT 'commit:' || c.sha, 'commit:' || r.sha FROM code_commits c JOIN code_commits r ON r.addr = c.backpatch
WHERE r.addr <> c.addr AND c.sha = ANY (", names!("commit:", "$1"), ")
UNION ALL
SELECT 'commit:' || c.sha, 'commit:' || r.sha FROM code_commits r JOIN code_commits c ON c.backpatch = r.addr
WHERE c.addr <> r.addr AND r.sha = ANY (", names!("commit:", "$2"), ")"
        ),
    },
    EdgeSource {
        label: "COMMITTED",
        starts: &["author:"],
        ends: &["commit:"],
        by_end: true,
        sql: concat!(
            "WITH commits AS (SELECT c.sha, lower(c.author_name) AS name FROM code_commits c
                 WHERE c.sha = ANY (", names!("commit:", "$2"), "))
SELECT 'author:' || m.author_id, 'commit:' || c.sha
FROM (", names_of!("$1"), ") x
CROSS JOIN LATERAL graph_committer(x.name) m
JOIN code_commits c ON lower(c.author_name) = x.name
WHERE m.author_id = ANY (", ids!("author:", "$1"), ")
UNION ALL
SELECT 'author:' || m.author_id, 'commit:' || c.sha
FROM (SELECT DISTINCT name FROM commits) x CROSS JOIN LATERAL graph_committer(x.name) m
JOIN commits c ON c.name = x.name"
        ),
    },
];

/// The author names whose committer changed: each name in the history whose
/// one author now (`graph_committer`) differs from the one
/// `graph_committers` keeps, as (name, was, now), keeping the new one. A
/// commit's COMMITTED edge comes from the one author with its author name,
/// so it follows the authors' names, not a row of its own: each batch
/// compares them all, a few hundred index lookups for PostgreSQL's 61
/// names. A batch runs whenever a change is captured, and every writer of
/// author_names also changes a captured column of authors
/// (count_author_message, merge_authors, undo_merge) or records the author's
/// key itself (restore_addresses), so a name whose author changes is
/// compared by the batch that applies that change.
const COMMITTER_CHANGES: &str = "WITH RECURSIVE names (name) AS (
    (SELECT lower(author_name) FROM code_commits ORDER BY 1 LIMIT 1)
    UNION ALL
    SELECT x.name FROM names n CROSS JOIN LATERAL (
        SELECT lower(c.author_name) AS name FROM code_commits c
        WHERE lower(c.author_name) > n.name ORDER BY 1 LIMIT 1) x
), changed AS (
    SELECT n.name, k.author_key AS was, 'author:' || m.author_id AS now
    FROM names n
    LEFT JOIN LATERAL graph_committer(n.name) m ON true
    LEFT JOIN graph_committers k ON k.name = n.name
    WHERE ('author:' || m.author_id) IS DISTINCT FROM k.author_key
), kept AS (
    INSERT INTO graph_committers (name, author_key) SELECT name, now FROM changed WHERE now IS NOT NULL
    ON CONFLICT (name) DO UPDATE SET author_key = EXCLUDED.author_key
), dropped AS (
    DELETE FROM graph_committers k USING changed x WHERE k.name = x.name AND x.now IS NULL
)
SELECT name, was, now FROM changed";

/// The COMMITTED edges to recompute for the names in $1 whose committer was
/// $2 and is $3: from both, into every commit of the name. An author not in
/// the graph yet gets its edges when its vertex enters.
const COMMITTER_EDGES: &str = "SELECT DISTINCT k.key, 'commit:' || c.sha
FROM unnest($1::text[], $2::text[], $3::text[]) x (name, was, now)
CROSS JOIN LATERAL (VALUES (x.was), (x.now)) k (key)
JOIN code_commits c ON lower(c.author_name) = x.name
WHERE k.key IS NOT NULL";

/// Edge labels that an older image doesn't know. Its full sync drops their
/// bookkeeping along with the commits' vertices but leaves their AGE rows,
/// which a full sync removes.
const LATER_EDGE_LABELS: &[&str] = &["BACKPATCH_OF", "COMMITTED"];

fn edge_source(label: &str) -> Option<&'static EdgeSource> {
    EDGE_SOURCES.iter().find(|source| source.label == label)
}

/// AGE's id and id sequence of each label.
async fn labels(tx: &Transaction<'_>) -> Result<HashMap<String, (i32, String)>> {
    let rows = tx
        .query(
            "SELECT l.name::text, l.id::int, l.seq_name::text FROM ag_catalog.ag_label l
             JOIN ag_catalog.ag_graph g ON g.graphid = l.graph WHERE g.name = $1",
            &[&GRAPH],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let seq: String = row.get(2);
            (row.get(0), (row.get(1), format!("{GRAPH}.\"{seq}\"")))
        })
        .collect())
}

fn label_id<'a>(
    labels: &'a HashMap<String, (i32, String)>,
    label: &str,
) -> Result<&'a (i32, String)> {
    labels
        .get(label)
        .with_context(|| format!("the graph has no label {label}; run bootstrap"))
}

/// Records a run in `graph_sync_runs`, keeping the last 100.
async fn record(tx: &Transaction<'_>, stats: &SyncStats) -> Result<()> {
    let summary = serde_json::to_value(stats)?;
    tx.execute(
        "INSERT INTO graph_sync_runs (finished_at, stats) VALUES (now(), $1)",
        &[&summary],
    )
    .await?;
    tx.execute(
        "DELETE FROM graph_sync_runs WHERE id < (SELECT max(id) - 100 FROM graph_sync_runs)",
        &[],
    )
    .await?;
    Ok(())
}

type Edge = (String, String);

async fn add_vertices(
    tx: &Transaction<'_>,
    labels: &HashMap<String, (i32, String)>,
    label: &str,
    vertices: &[(String, Value)],
) -> Result<u64> {
    let (label_id, seq) = label_id(labels, label)?;
    let (keys, props): (Vec<&str>, Vec<&Value>) = vertices
        .iter()
        .map(|(key, props)| (key.as_str(), props))
        .unzip();
    Ok(tx
        .execute(
            &format!(
                "WITH new AS (
                     SELECT u.key, u.props, ag_catalog._graphid($3, nextval($4::text::regclass)) AS gid
                     FROM unnest($1::text[], $2::jsonb[]) u (key, props)
                 ), ins AS (
                     INSERT INTO {GRAPH}.\"{label}\" (id, properties)
                     SELECT gid, props::text::ag_catalog.agtype FROM new
                 )
                 INSERT INTO graph_nodes (key, label, graph_id, props) SELECT key, $5, gid, props FROM new"
            ),
            &[&keys, &props, label_id, seq, &label],
        )
        .await?)
}

async fn update_vertices(
    tx: &Transaction<'_>,
    label: &str,
    vertices: &[(String, Value)],
) -> Result<u64> {
    let (keys, props): (Vec<&str>, Vec<&Value>) = vertices
        .iter()
        .map(|(key, props)| (key.as_str(), props))
        .unzip();
    Ok(tx
        .execute(
            &format!(
                "WITH upd AS (
                     UPDATE graph_nodes g SET props = u.props, updated_at = now()
                     FROM unnest($1::text[], $2::jsonb[]) u (key, props)
                     WHERE g.key = u.key
                     RETURNING g.graph_id, u.props
                 )
                 UPDATE {GRAPH}.\"{label}\" v SET properties = upd.props::text::ag_catalog.agtype
                 FROM upd WHERE v.id = upd.graph_id"
            ),
            &[&keys, &props],
        )
        .await?)
}

/// Removes vertices whose edges are gone.
async fn remove_vertices(tx: &Transaction<'_>, label: &str, keys: &[String]) -> Result<u64> {
    Ok(tx
        .execute(
            &format!(
                "WITH gone AS (DELETE FROM graph_nodes g WHERE g.key = ANY ($1) RETURNING g.graph_id)
                 DELETE FROM {GRAPH}.\"{label}\" v USING gone WHERE v.id = gone.graph_id"
            ),
            &[&keys],
        )
        .await?)
}

/// Adds edges between vertices in the graph.
async fn add_edges(
    tx: &Transaction<'_>,
    labels: &HashMap<String, (i32, String)>,
    label: &str,
    edges: &BTreeSet<Edge>,
) -> Result<u64> {
    let (label_id, seq) = label_id(labels, label)?;
    let (starts, ends): (Vec<&str>, Vec<&str>) =
        edges.iter().map(|(s, e)| (s.as_str(), e.as_str())).unzip();
    Ok(tx
        .execute(
            &format!(
                "WITH new AS (
                     SELECT u.start_key, u.end_key, s.graph_id AS sid, e.graph_id AS eid,
                            ag_catalog._graphid($3, nextval($4::text::regclass)) AS gid
                     FROM unnest($1::text[], $2::text[]) u (start_key, end_key)
                     JOIN graph_nodes s ON s.key = u.start_key
                     JOIN graph_nodes e ON e.key = u.end_key
                 ), ins AS (
                     INSERT INTO {GRAPH}.\"{label}\" (id, start_id, end_id, properties)
                     SELECT gid, sid, eid, '{{}}'::ag_catalog.agtype FROM new
                 )
                 INSERT INTO graph_edges (label, start_key, end_key, graph_id)
                 SELECT $5, start_key, end_key, gid FROM new"
            ),
            &[&starts, &ends, label_id, seq, &label],
        )
        .await?)
}

async fn remove_edges(tx: &Transaction<'_>, label: &str, edges: &BTreeSet<Edge>) -> Result<u64> {
    let (starts, ends): (Vec<&str>, Vec<&str>) =
        edges.iter().map(|(s, e)| (s.as_str(), e.as_str())).unzip();
    Ok(tx
        .execute(
            &format!(
                "WITH gone AS (
                     DELETE FROM graph_edges g USING unnest($1::text[], $2::text[]) u (start_key, end_key)
                     WHERE g.label = $3 AND g.start_key = u.start_key AND g.end_key = u.end_key
                     RETURNING g.graph_id
                 )
                 DELETE FROM {GRAPH}.\"{label}\" e USING gone WHERE e.id = gone.graph_id"
            ),
            &[&starts, &ends, &label],
        )
        .await?)
}

/// Asks for a full sync, which the graph sync job runs next.
pub async fn request_full(client: &impl GenericClient, reason: &str) -> Result<()> {
    client
        .execute("SELECT graph_request_full($1)", &[&reason])
        .await?;
    Ok(())
}

/// Recomputes the whole graph and brings it up to date, in one transaction.
/// It answers the requests for a full sync made before it, applies the change
/// entries committed before it, recounts the labels, and records its
/// [`PROJECTION`].
pub async fn sync(client: &mut Client) -> Result<SyncStats> {
    let started = Instant::now();
    let tx = client.transaction().await?;
    tx.batch_execute(&format!(
        "{LOCK};
         CREATE TEMP TABLE want_nodes (key text PRIMARY KEY, label text NOT NULL, props jsonb NOT NULL) ON COMMIT DROP;
         CREATE TEMP TABLE want_edges (label text, start_key text, end_key text,
                                       PRIMARY KEY (label, start_key, end_key)) ON COMMIT DROP;"
    ))
    .await?;
    // Requests and changes committed before the tables are read below; the
    // ones that come later are left for the next sync.
    let requested: i64 = tx
        .query_one(
            "SELECT coalesce((SELECT full_requested FROM graph_sync_state), 0)",
            &[],
        )
        .await?
        .get(0);
    let mut stats = SyncStats {
        mode: "full",
        changes: tx.execute("DELETE FROM graph_changes", &[]).await?,
        ..SyncStats::default()
    };
    tx.batch_execute(WANT_NODES)
        .await
        .context("cannot compute the graph's vertices")?;
    tx.batch_execute(WANT_COMMITTERS)
        .await
        .context("cannot compute the committers")?;
    tx.batch_execute(WANT_EDGES)
        .await
        .context("cannot compute the graph's edges")?;
    tx.batch_execute(
        "DELETE FROM want_edges w
         WHERE NOT EXISTS (SELECT FROM want_nodes n WHERE n.key = w.start_key)
            OR NOT EXISTS (SELECT FROM want_nodes n WHERE n.key = w.end_key);
         ANALYZE want_nodes; ANALYZE want_edges;
         DELETE FROM graph_committers;
         INSERT INTO graph_committers (name, author_key) SELECT name, author_key FROM want_committers;",
    )
    .await?;
    let labels = labels(&tx).await?;

    for label in LATER_EDGE_LABELS {
        stats.edges_removed += tx
            .execute(
                &format!(
                    "DELETE FROM {GRAPH}.\"{label}\" e
                     WHERE NOT EXISTS (SELECT FROM graph_edges g WHERE g.graph_id = e.id)"
                ),
                &[],
            )
            .await?;
    }
    for label in EDGE_LABELS {
        stats.edges_removed += tx
            .execute(
                &format!(
                    "DELETE FROM {GRAPH}.\"{label}\" e USING graph_edges g
                     WHERE g.label = $1 AND e.id = g.graph_id
                       AND NOT EXISTS (SELECT FROM want_edges w
                                       WHERE w.label = g.label AND w.start_key = g.start_key AND w.end_key = g.end_key)"
                ),
                &[label],
            )
            .await?;
        tx.execute(
            "DELETE FROM graph_edges g WHERE g.label = $1
               AND NOT EXISTS (SELECT FROM want_edges w
                               WHERE w.label = g.label AND w.start_key = g.start_key AND w.end_key = g.end_key)",
            &[label],
        )
        .await?;
    }
    for label in VERTEX_LABELS {
        // Edges of a removed vertex were removed above: they're only wanted between wanted vertices.
        stats.nodes_removed += tx
            .execute(
                &format!(
                    "DELETE FROM {GRAPH}.\"{label}\" v USING graph_nodes g
                     WHERE g.label = $1 AND v.id = g.graph_id
                       AND NOT EXISTS (SELECT FROM want_nodes w WHERE w.key = g.key)"
                ),
                &[label],
            )
            .await?;
        tx.execute(
            "DELETE FROM graph_nodes g WHERE g.label = $1 AND NOT EXISTS (SELECT FROM want_nodes w WHERE w.key = g.key)",
            &[label],
        )
        .await?;
        stats.nodes_updated += tx
            .execute(
                &format!(
                    "UPDATE {GRAPH}.\"{label}\" v SET properties = w.props::text::ag_catalog.agtype
                     FROM graph_nodes g JOIN want_nodes w ON w.key = g.key
                     WHERE g.label = $1 AND v.id = g.graph_id AND g.props IS DISTINCT FROM w.props"
                ),
                &[label],
            )
            .await?;
        tx.execute(
            "UPDATE graph_nodes g SET props = w.props, updated_at = now() FROM want_nodes w
             WHERE w.key = g.key AND g.label = $1 AND g.props IS DISTINCT FROM w.props",
            &[label],
        )
        .await?;
        let (label_id, seq) = label_id(&labels, label)?;
        stats.nodes_added += tx
            .execute(
                &format!(
                    "WITH new AS (
                         SELECT w.key, w.props, ag_catalog._graphid($2, nextval($3::text::regclass)) AS gid
                         FROM want_nodes w
                         WHERE w.label = $1 AND NOT EXISTS (SELECT FROM graph_nodes g WHERE g.key = w.key)
                     ), ins AS (
                         INSERT INTO {GRAPH}.\"{label}\" (id, properties)
                         SELECT gid, props::text::ag_catalog.agtype FROM new
                     )
                     INSERT INTO graph_nodes (key, label, graph_id, props) SELECT key, $1, gid, props FROM new"
                ),
                &[label, label_id, seq],
            )
            .await?;
    }
    for label in EDGE_LABELS {
        let (label_id, seq) = label_id(&labels, label)?;
        stats.edges_added += tx
            .execute(
                &format!(
                    "WITH new AS (
                         SELECT w.start_key, w.end_key, s.graph_id AS sid, e.graph_id AS eid,
                                ag_catalog._graphid($2, nextval($3::text::regclass)) AS gid
                         FROM want_edges w
                         JOIN graph_nodes s ON s.key = w.start_key
                         JOIN graph_nodes e ON e.key = w.end_key
                         WHERE w.label = $1 AND NOT EXISTS (
                             SELECT FROM graph_edges g
                             WHERE g.label = w.label AND g.start_key = w.start_key AND g.end_key = w.end_key)
                     ), ins AS (
                         INSERT INTO {GRAPH}.\"{label}\" (id, start_id, end_id, properties)
                         SELECT gid, sid, eid, '{{}}'::ag_catalog.agtype FROM new
                     )
                     INSERT INTO graph_edges (label, start_key, end_key, graph_id)
                     SELECT $1, start_key, end_key, gid FROM new"
                ),
                &[label, label_id, seq],
            )
            .await?;
    }

    // The graph is now what the want tables hold: count it from them.
    let (names, kinds): (Vec<&str>, Vec<&str>) = VERTEX_LABELS
        .iter()
        .map(|label| (*label, "vertex"))
        .chain(EDGE_LABELS.iter().map(|label| (*label, "edge")))
        .unzip();
    tx.execute(
        "INSERT INTO graph_label_counts (label, kind, count)
         SELECT l.label, l.kind, coalesce(n.count, 0)
         FROM unnest($1::text[], $2::text[]) l (label, kind)
         LEFT JOIN (SELECT label, count(*) AS count FROM want_nodes GROUP BY label
                    UNION ALL SELECT label, count(*) FROM want_edges GROUP BY label) n ON n.label = l.label
         ON CONFLICT (label) DO UPDATE SET kind = EXCLUDED.kind, count = EXCLUDED.count",
        &[&names, &kinds],
    )
    .await?;
    tx.execute(
        "DELETE FROM graph_label_counts WHERE label <> ALL ($1)",
        &[&names],
    )
    .await?;
    tx.execute(
        "INSERT INTO graph_sync_state AS s (full_requested, full_done, last_full_at, projection)
         VALUES ($1, $1, now(), $2)
         ON CONFLICT (id) DO UPDATE
             SET full_done = greatest(s.full_done, $1), last_full_at = now(), projection = $2",
        &[&requested, &PROJECTION],
    )
    .await?;
    stats.ms = started.elapsed().as_millis() as u64;
    record(&tx, &stats).await?;
    tx.commit().await?;
    if stats.changed() > 0 {
        tracing::info!(?stats, "synced the graph");
    }
    Ok(stats)
}

/// Applies up to `limit` change entries, the oldest committed, in one
/// transaction: recomputes the vertices and edges they name from the tables,
/// and updates the graph where it differs.
pub async fn sync_changes(client: &mut Client, limit: i64) -> Result<SyncStats> {
    let started = Instant::now();
    let mut stats = SyncStats {
        mode: "incremental",
        ..SyncStats::default()
    };
    let tx = client.transaction().await?;
    tx.batch_execute(LOCK).await?;
    // Entries are read by what's committed, not by ID: a transaction can
    // commit an entry with a lower ID after one with a higher ID was applied.
    let rows = tx
        .query(
            "SELECT id, label, key, end_key FROM graph_changes ORDER BY id LIMIT $1",
            &[&limit],
        )
        .await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(stats);
    }
    let mut ids = Vec::with_capacity(rows.len());
    let mut keys = BTreeSet::new();
    let mut edges: BTreeMap<String, BTreeSet<Edge>> = BTreeMap::new();
    for row in &rows {
        ids.push(row.get::<_, i64>(0));
        let key: String = row.get(2);
        match row.get::<_, Option<String>>(1) {
            None => {
                keys.insert(key);
            }
            Some(label) => {
                if edge_source(&label).is_none() && !EDGE_LABELS.contains(&label.as_str()) {
                    bail!("a change names an unknown edge label {label}");
                }
                let end: Option<String> = row.get(3);
                let end = end.context("an edge change has no end key")?;
                edges.entry(label).or_default().insert((key, end));
            }
        }
    }
    stats.changes = ids.len() as u64;
    let labels = labels(&tx).await?;
    let mut counts: HashMap<String, i64> = HashMap::new();

    // The vertices: wanted ones from the tables against those in the graph.
    let keys: Vec<String> = keys.into_iter().collect();
    let mut wanted: HashMap<String, (String, Value)> = HashMap::new();
    let mut present: HashMap<String, (String, Value)> = HashMap::new();
    if !keys.is_empty() {
        for row in tx
            .query(KEYED_NODES, &[&keys])
            .await
            .context("cannot compute the changed vertices")?
        {
            wanted.insert(row.get(0), (row.get(1), row.get(2)));
        }
        for row in tx
            .query(
                "SELECT key, label, props FROM graph_nodes WHERE key = ANY ($1)",
                &[&keys],
            )
            .await?
        {
            present.insert(row.get(0), (row.get(1), row.get(2)));
        }
    }
    let mut added: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    let mut updated: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    let mut removed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in &keys {
        match (wanted.remove(key), present.get(key)) {
            (Some((label, props)), None) => {
                added.entry(label).or_default().push((key.clone(), props))
            }
            (Some((label, props)), Some((_, old))) if props != *old => {
                updated.entry(label).or_default().push((key.clone(), props))
            }
            (None, Some((label, _))) => removed.entry(label.clone()).or_default().push(key.clone()),
            _ => {}
        }
    }
    if !removed.is_empty() {
        // A vertex leaves with its edges.
        let gone: Vec<&String> = removed.values().flatten().collect();
        let mut incident: BTreeMap<String, BTreeSet<Edge>> = BTreeMap::new();
        for row in tx
            .query(
                "SELECT label, start_key, end_key FROM graph_edges WHERE start_key = ANY ($1) OR end_key = ANY ($1)",
                &[&gone],
            )
            .await?
        {
            incident
                .entry(row.get(0))
                .or_default()
                .insert((row.get(1), row.get(2)));
        }
        for (label, set) in &incident {
            let n = remove_edges(&tx, label, set).await?;
            stats.edges_removed += n;
            *counts.entry(label.clone()).or_default() -= n as i64;
        }
        for (label, keys) in &removed {
            let n = remove_vertices(&tx, label, keys).await?;
            stats.nodes_removed += n;
            *counts.entry(label.clone()).or_default() -= n as i64;
        }
    }
    for (label, vertices) in &updated {
        stats.nodes_updated += update_vertices(&tx, label, vertices).await?;
    }
    let mut entered: Vec<String> = Vec::new();
    for (label, vertices) in &added {
        let n = add_vertices(&tx, &labels, label, vertices).await?;
        stats.nodes_added += n;
        *counts.entry(label.clone()).or_default() += n as i64;
        entered.extend(vertices.iter().map(|(key, _)| key.clone()));
    }

    // The authors' names, as the vertices now stand, decide who committed
    // the commits of a name.
    let flips = tx
        .query(COMMITTER_CHANGES, &[])
        .await
        .context("cannot compare the committers")?;
    if !flips.is_empty() {
        let (mut names, mut was, mut now) = (Vec::new(), Vec::new(), Vec::new());
        for row in &flips {
            let (name, before, after): (String, Option<String>, Option<String>) =
                (row.get(0), row.get(1), row.get(2));
            tracing::info!(%name, was = ?before, now = ?after, "a committer's name changed author");
            names.push(name);
            was.push(before);
            now.push(after);
        }
        for row in tx
            .query(COMMITTER_EDGES, &[&names, &was, &now])
            .await
            .context("cannot find the committers' commits")?
        {
            edges
                .entry("COMMITTED".to_owned())
                .or_default()
                .insert((row.get(0), row.get(1)));
        }
    }

    // The edges: those the changes name, found again from their rows, and the
    // edges of the vertices that entered the graph.
    let mut starts: HashMap<&str, BTreeSet<String>> = HashMap::new();
    let mut ends: HashMap<&str, BTreeSet<String>> = HashMap::new();
    for (label, set) in &edges {
        let Some(source) = edge_source(label) else {
            continue;
        };
        for (start, end) in set {
            if source.by_end {
                ends.entry(source.label).or_default().insert(end.clone());
            } else {
                starts
                    .entry(source.label)
                    .or_default()
                    .insert(start.clone());
            }
        }
    }
    for key in &entered {
        for source in EDGE_SOURCES {
            if source.starts.iter().any(|prefix| key.starts_with(prefix)) {
                starts.entry(source.label).or_default().insert(key.clone());
            }
            if source.ends.iter().any(|prefix| key.starts_with(prefix)) {
                ends.entry(source.label).or_default().insert(key.clone());
            }
        }
    }
    let mut implied: BTreeMap<String, BTreeSet<Edge>> = BTreeMap::new();
    for source in EDGE_SOURCES {
        let from: Vec<&String> = starts
            .get(source.label)
            .map(|set| set.iter().collect())
            .unwrap_or_default();
        let to: Vec<&String> = ends
            .get(source.label)
            .map(|set| set.iter().collect())
            .unwrap_or_default();
        if from.is_empty() && to.is_empty() {
            continue;
        }
        let set = implied.entry(source.label.to_owned()).or_default();
        for row in tx
            .query(source.sql, &[&from, &to])
            .await
            .with_context(|| format!("cannot compute the changed {} edges", source.label))?
        {
            if let (Some(start), Some(end)) = (
                row.get::<_, Option<String>>(0),
                row.get::<_, Option<String>>(1),
            ) {
                set.insert((start, end));
            }
        }
    }
    let endpoints: BTreeSet<&String> = implied
        .values()
        .flatten()
        .flat_map(|(start, end)| [start, end])
        .collect();
    let mut in_graph: HashSet<String> = HashSet::new();
    if !endpoints.is_empty() {
        let endpoints: Vec<&String> = endpoints.into_iter().collect();
        for row in tx
            .query(
                "SELECT key FROM graph_nodes WHERE key = ANY ($1)",
                &[&endpoints],
            )
            .await?
        {
            in_graph.insert(row.get(0));
        }
    }
    let touched: BTreeSet<&String> = edges.keys().chain(implied.keys()).collect();
    for label in touched {
        let want: BTreeSet<Edge> = implied
            .get(label)
            .map(|set| {
                set.iter()
                    .filter(|(start, end)| in_graph.contains(start) && in_graph.contains(end))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let candidates: BTreeSet<&Edge> = edges
            .get(label)
            .into_iter()
            .flatten()
            .chain(implied.get(label).into_iter().flatten())
            .collect();
        let (from, to): (Vec<&str>, Vec<&str>) = candidates
            .iter()
            .map(|(start, end)| (start.as_str(), end.as_str()))
            .unzip();
        let have: BTreeSet<Edge> = tx
            .query(
                "SELECT g.start_key, g.end_key FROM unnest($2::text[], $3::text[]) u (start_key, end_key)
                 JOIN graph_edges g ON g.label = $1 AND g.start_key = u.start_key AND g.end_key = u.end_key",
                &[label, &from, &to],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let gone: BTreeSet<Edge> = have.difference(&want).cloned().collect();
        let new: BTreeSet<Edge> = want.difference(&have).cloned().collect();
        if !gone.is_empty() {
            let n = remove_edges(&tx, label, &gone).await?;
            stats.edges_removed += n;
            *counts.entry(label.clone()).or_default() -= n as i64;
        }
        if !new.is_empty() {
            let n = add_edges(&tx, &labels, label, &new).await?;
            stats.edges_added += n;
            *counts.entry(label.clone()).or_default() += n as i64;
        }
    }

    for (label, delta) in counts.iter().filter(|(_, delta)| **delta != 0) {
        let kept = tx
            .execute(
                "UPDATE graph_label_counts SET count = count + $2 WHERE label = $1",
                &[label, delta],
            )
            .await?;
        // As an older image's full sync leaves the labels it doesn't know.
        if kept == 0 {
            request_full(&tx, &format!("the graph keeps no count of {label}")).await?;
        }
    }
    tx.execute("DELETE FROM graph_changes WHERE id = ANY ($1)", &[&ids])
        .await?;
    stats.ms = started.elapsed().as_millis() as u64;
    if stats.changed() > 0 {
        record(&tx, &stats).await?;
    }
    tx.commit().await?;
    if stats.changed() > 0 {
        tracing::info!(?stats, "synced the graph");
    }
    Ok(stats)
}

/// What [`run`] did.
#[derive(Debug, Default, Serialize)]
pub struct RunReport {
    /// The full sync, if one ran.
    pub full: Option<SyncStats>,
    /// The batches of changes applied, and their totals.
    pub batches: u64,
    pub changes: u64,
    pub nodes_added: u64,
    pub nodes_updated: u64,
    pub nodes_removed: u64,
    pub edges_added: u64,
    pub edges_removed: u64,
    /// Whether change entries are waiting.
    pub more: bool,
}

/// Whether change entries are waiting.
pub async fn pending(client: &impl GenericClient) -> Result<bool> {
    Ok(client
        .query_one("SELECT EXISTS (SELECT FROM graph_changes)", &[])
        .await?
        .get(0))
}

/// The graph sync job's run: a full sync if one was requested, the graph has
/// an older projection or a label has no kept count, or if `reconcile_every`
/// has passed since the last one and no change is waiting; else batches of
/// changes until none is left or `budget` is spent.
pub async fn run(
    client: &mut Client,
    budget: Duration,
    reconcile_every: Option<Duration>,
) -> Result<RunReport> {
    let started = Instant::now();
    let mut report = RunReport::default();
    let registered: Vec<&str> = VERTEX_LABELS.iter().chain(EDGE_LABELS).copied().collect();
    let state = client
        .query_opt(
            "SELECT full_requested > full_done OR projection < $1
                    OR EXISTS (SELECT FROM unnest($2::text[]) l (label)
                               WHERE NOT EXISTS (SELECT FROM graph_label_counts c WHERE c.label = l.label)),
                    CASE WHEN full_requested > full_done THEN full_reason
                         WHEN projection < $1 THEN 'the projection changed'
                         ELSE 'a label has no count, as after an older image''s full sync' END,
                    last_full_at
             FROM graph_sync_state",
            &[&PROJECTION, &registered],
        )
        .await?;
    let (requested, reason, last_full) = match &state {
        Some(row) => (
            row.get::<_, bool>(0),
            row.get::<_, Option<String>>(1),
            row.get::<_, Option<DateTime<Utc>>>(2),
        ),
        None => (true, Some("the sync state is missing".to_owned()), None),
    };
    let due = reconcile_every.is_some_and(|every| {
        last_full.is_none_or(|at| {
            Utc::now()
                .signed_duration_since(at)
                .to_std()
                .unwrap_or_default()
                >= every
        })
    });
    if requested {
        tracing::info!(
            reason = reason.as_deref().unwrap_or("requested"),
            "running a full graph sync"
        );
        report.full = Some(sync(client).await?);
    } else if due && !pending(&*client).await? {
        tracing::info!("reconciling the graph with a full sync");
        let stats = sync(client).await?;
        if stats.changed() > 0 {
            tracing::warn!(
                ?stats,
                "the reconcile changed the graph: the change capture missed something, or changes committed while it ran"
            );
        }
        report.full = Some(stats);
    } else {
        loop {
            let stats = sync_changes(client, BATCH).await?;
            report.batches += 1;
            report.changes += stats.changes;
            report.nodes_added += stats.nodes_added;
            report.nodes_updated += stats.nodes_updated;
            report.nodes_removed += stats.nodes_removed;
            report.edges_added += stats.edges_added;
            report.edges_removed += stats.edges_removed;
            if stats.changes < BATCH as u64 || started.elapsed() >= budget {
                break;
            }
        }
    }
    report.more = pending(&*client).await?;
    Ok(report)
}

/// How the graph differs from the tables, and from its own bookkeeping:
/// examples of what a full sync would change, AGE rows that don't match
/// `graph_nodes` and `graph_edges`, and label counts that are off.
#[derive(Debug, Default, Serialize)]
pub struct Drift {
    /// Vertices the tables want that the graph lacks, or has with other
    /// properties; vertices it has that they don't want.
    pub missing_nodes: Vec<String>,
    pub changed_nodes: Vec<String>,
    pub extra_nodes: Vec<String>,
    /// Edges, as `LABEL start -> end`.
    pub missing_edges: Vec<String>,
    pub extra_edges: Vec<String>,
    /// Bookkeeping rows without their AGE row or with another one, and AGE
    /// rows without bookkeeping.
    pub age: Vec<String>,
    /// `label: kept, actual`.
    pub counts: Vec<String>,
    /// Author names whose kept committer differs, as `name: kept, wanted`.
    pub committers: Vec<String>,
}

impl Drift {
    pub fn is_empty(&self) -> bool {
        self.missing_nodes.is_empty()
            && self.changed_nodes.is_empty()
            && self.extra_nodes.is_empty()
            && self.missing_edges.is_empty()
            && self.extra_edges.is_empty()
            && self.age.is_empty()
            && self.counts.is_empty()
            && self.committers.is_empty()
    }
}

/// The examples of each kind of drift that [`check`] lists.
const DRIFT_EXAMPLES: i64 = 20;

/// Computes the whole graph from the tables, as a full sync does, and
/// compares; changes nothing. As costly as a full sync's reads.
pub async fn check(client: &mut Client) -> Result<Drift> {
    let tx = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .start()
        .await?;
    tx.batch_execute(
        "CREATE TEMP TABLE want_nodes (key text PRIMARY KEY, label text NOT NULL, props jsonb NOT NULL) ON COMMIT DROP;
         CREATE TEMP TABLE want_edges (label text, start_key text, end_key text,
                                       PRIMARY KEY (label, start_key, end_key)) ON COMMIT DROP;",
    )
    .await?;
    tx.batch_execute(WANT_NODES).await?;
    tx.batch_execute(WANT_COMMITTERS).await?;
    tx.batch_execute(WANT_EDGES).await?;
    tx.batch_execute(
        "DELETE FROM want_edges w
         WHERE NOT EXISTS (SELECT FROM want_nodes n WHERE n.key = w.start_key)
            OR NOT EXISTS (SELECT FROM want_nodes n WHERE n.key = w.end_key);
         ANALYZE want_nodes; ANALYZE want_edges;",
    )
    .await?;
    let list = |rows: Vec<tokio_postgres::Row>| -> Vec<String> {
        rows.iter().map(|row| row.get(0)).collect()
    };
    let mut drift = Drift {
        committers: list(
            tx.query(
                "SELECT coalesce(k.name, w.name) || ': ' || coalesce(k.author_key, 'none') || ', '
                        || coalesce(w.author_key, 'none')
                 FROM graph_committers k FULL JOIN want_committers w ON w.name = k.name
                 WHERE k.author_key IS DISTINCT FROM w.author_key ORDER BY 1 LIMIT $1",
                &[&DRIFT_EXAMPLES],
            )
            .await?,
        ),
        missing_nodes: list(
            tx.query(
                "SELECT w.key FROM want_nodes w WHERE NOT EXISTS (SELECT FROM graph_nodes g WHERE g.key = w.key)
                 ORDER BY w.key LIMIT $1",
                &[&DRIFT_EXAMPLES],
            )
            .await?,
        ),
        changed_nodes: list(
            tx.query(
                "SELECT w.key FROM want_nodes w JOIN graph_nodes g ON g.key = w.key
                 WHERE g.label <> w.label OR g.props IS DISTINCT FROM w.props ORDER BY w.key LIMIT $1",
                &[&DRIFT_EXAMPLES],
            )
            .await?,
        ),
        extra_nodes: list(
            tx.query(
                "SELECT g.key FROM graph_nodes g WHERE NOT EXISTS (SELECT FROM want_nodes w WHERE w.key = g.key)
                 ORDER BY g.key LIMIT $1",
                &[&DRIFT_EXAMPLES],
            )
            .await?,
        ),
        missing_edges: list(
            tx.query(
                "SELECT w.label || ' ' || w.start_key || ' -> ' || w.end_key FROM want_edges w
                 WHERE NOT EXISTS (SELECT FROM graph_edges g
                                   WHERE g.label = w.label AND g.start_key = w.start_key AND g.end_key = w.end_key)
                 ORDER BY 1 LIMIT $1",
                &[&DRIFT_EXAMPLES],
            )
            .await?,
        ),
        extra_edges: list(
            tx.query(
                "SELECT g.label || ' ' || g.start_key || ' -> ' || g.end_key FROM graph_edges g
                 WHERE NOT EXISTS (SELECT FROM want_edges w
                                   WHERE w.label = g.label AND w.start_key = g.start_key AND w.end_key = g.end_key)
                 ORDER BY 1 LIMIT $1",
                &[&DRIFT_EXAMPLES],
            )
            .await?,
        ),
        ..Drift::default()
    };
    for label in VERTEX_LABELS {
        drift.age.extend(list(
            tx.query(
                &format!(
                    "SELECT 'vertex ' || g.key FROM graph_nodes g LEFT JOIN ONLY {GRAPH}.\"{label}\" v ON v.id = g.graph_id
                     WHERE g.label = $1 AND (v.id IS NULL OR v.properties::text::jsonb IS DISTINCT FROM g.props)
                     UNION ALL
                     SELECT '{label} vertex ' || v.id::text || ' has no bookkeeping' FROM ONLY {GRAPH}.\"{label}\" v
                     WHERE NOT EXISTS (SELECT FROM graph_nodes g WHERE g.graph_id = v.id AND g.label = $1)
                     LIMIT $2"
                ),
                &[label, &DRIFT_EXAMPLES],
            )
            .await?,
        ));
    }
    for label in EDGE_LABELS {
        drift.age.extend(list(
            tx.query(
                &format!(
                    "SELECT 'edge ' || g.label || ' ' || g.start_key || ' -> ' || g.end_key
                     FROM graph_edges g
                     JOIN graph_nodes s ON s.key = g.start_key
                     JOIN graph_nodes e ON e.key = g.end_key
                     LEFT JOIN ONLY {GRAPH}.\"{label}\" x ON x.id = g.graph_id
                     WHERE g.label = $1 AND (x.id IS NULL OR x.start_id <> s.graph_id OR x.end_id <> e.graph_id)
                     UNION ALL
                     SELECT '{label} edge ' || x.id::text || ' has no bookkeeping' FROM ONLY {GRAPH}.\"{label}\" x
                     WHERE NOT EXISTS (SELECT FROM graph_edges g WHERE g.graph_id = x.id AND g.label = $1)
                     LIMIT $2"
                ),
                &[label, &DRIFT_EXAMPLES],
            )
            .await?,
        ));
    }
    // Counts, once a full sync keeps them.
    drift.counts = list(
        tx.query(
            "WITH actual AS (
                 SELECT label, count(*) AS n FROM graph_nodes GROUP BY label
                 UNION ALL SELECT label, count(*) FROM graph_edges GROUP BY label
             )
             SELECT coalesce(c.label, a.label) || ': ' || coalesce(c.count::text, 'none') || ', ' || coalesce(a.n, 0)
             FROM graph_label_counts c FULL JOIN actual a ON a.label = c.label
             WHERE EXISTS (SELECT FROM graph_label_counts) AND c.count IS DISTINCT FROM coalesce(a.n, 0)
             ORDER BY 1",
            &[],
        )
        .await?,
    );
    tx.rollback().await?;
    Ok(drift)
}
