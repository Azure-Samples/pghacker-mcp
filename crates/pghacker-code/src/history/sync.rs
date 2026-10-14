//! Applies new commits to a history: each new commit, in address order (the
//! order a build walks), through the same walker as a build, with its files
//! read from the tables and its changes written back. So a build up to some
//! commit followed by syncs gives the same history as one build, down to the
//! weave order: a new line's key falls between its anchor's and the next key
//! in the lineage, the place the build's linked list gives it. When two keys
//! are too close, the lineage is renumbered and `weave_epoch` bumped.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use gix::ObjectId;
use tokio::runtime::Handle;
use tokio_postgres::{Client, Transaction};

use super::addr::{self, Alive};
use super::graph::Graph;
use super::store::{WEAVE_STEP, readable_schema};
use super::walk::{File, Files, Ledger, NONE, Version, WalkStats, Walker};

/// What to sync.
#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub git_dir: PathBuf,
    /// The history's schema: pghacker once installed.
    pub schema: String,
    /// New commits per transaction.
    pub batch: usize,
}

/// What a sync did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SyncStats {
    pub commits: u64,
    pub new_lanes: Vec<String>,
    pub refs: u64,
    /// Lineages whose weave keys were renumbered.
    pub respread: u64,
    pub walk: WalkStats,
}

/// A new line instance, before its weave key is known.
struct NewLine {
    id: u32,
    lineage: u32,
    born: i64,
    after: u32,
    replaces: u32,
    text: String,
}

/// A sync's ledger: one commit's effects, until they are written.
#[derive(Default)]
struct MemLedger {
    next_instance: u32,
    next_version: u32,
    next_lineage: u32,
    lineages: Vec<(u32, String, i64)>,
    lines: Vec<NewLine>,
    kills: Vec<(u32, i64)>,
    versions: Vec<(u32, Version)>,
    version_kills: Vec<(u32, i64)>,
}

impl Ledger for MemLedger {
    fn new_lineage(&mut self, path: &str, born: i64) -> Result<u32> {
        let id = self.next_lineage;
        self.next_lineage += 1;
        self.lineages.push((id, path.to_owned(), born));
        Ok(id)
    }

    fn new_instance(
        &mut self,
        lineage: u32,
        born: i64,
        after: u32,
        replaces: u32,
        text: &str,
    ) -> Result<u32> {
        let id = self.next_instance;
        if id >= i32::MAX as u32 {
            bail!("too many line instances");
        }
        self.next_instance += 1;
        self.lines.push(NewLine {
            id,
            lineage,
            born,
            after,
            replaces,
            text: text.to_owned(),
        });
        Ok(id)
    }

    fn kill(&mut self, id: u32, at: i64) {
        self.kills.push((id, at));
    }

    fn new_version(&mut self, version: Version) -> Result<u32> {
        let id = self.next_version;
        self.next_version += 1;
        self.versions.push((id, version));
        Ok(id)
    }

    fn kill_version(&mut self, id: u32, at: i64) {
        self.version_kills.push((id, at));
    }
}

/// A commit to apply.
struct Pending {
    addr: i64,
    id: ObjectId,
    tree: ObjectId,
    parent_addr: Option<i64>,
    parent_tree: Option<ObjectId>,
    lane: i16,
    committed_at: i64,
    author: String,
    subject: String,
}

/// A lane as the tables have it.
#[derive(Clone, Debug)]
struct LaneRow {
    id: i16,
    name: String,
    fork_addr: Option<i64>,
    lo: i64,
    head_addr: i64,
}

/// Applies every commit the clone has and the history doesn't.
pub async fn sync(client: Client, opts: SyncOptions) -> Result<SyncStats> {
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || sync_blocking(&handle, client, &opts))
        .await
        .context("the sync panicked")?
}

fn sync_blocking(handle: &Handle, mut client: Client, opts: &SyncOptions) -> Result<SyncStats> {
    let s = readable_schema(&opts.schema)?.to_owned();
    let mut stats = SyncStats::default();
    let row = handle
        .block_on(client.query_opt(&format!("SELECT master, paths FROM {s}.code_history"), &[]))?
        .with_context(|| format!("schema {s} has no history to sync"))?;
    let (master, paths): (String, Vec<String>) = (row.get(0), row.get(1));
    let lanes: Vec<LaneRow> = handle
        .block_on(client.query(
            &format!("SELECT id, name, fork_addr, lo, head_addr FROM {s}.code_lanes ORDER BY id"),
            &[],
        ))?
        .iter()
        .map(|r| LaneRow {
            id: r.get(0),
            name: r.get(1),
            fork_addr: r.get(2),
            lo: r.get(3),
            head_addr: r.get(4),
        })
        .collect();
    let mut known: HashMap<ObjectId, i64> = HashMap::new();
    for r in
        handle.block_on(client.query(&format!("SELECT sha, addr FROM {s}.code_commits"), &[]))?
    {
        known.insert(
            ObjectId::from_hex(r.get::<_, &str>(0).as_bytes())?,
            r.get(1),
        );
    }
    let mut by_addr: HashMap<i64, ObjectId> = known.iter().map(|(k, v)| (*v, *k)).collect();

    let repo = super::open(&opts.git_dir)?;
    let graph = Graph::read(&repo, &master)?;
    let mut pending: Vec<Pending> = Vec::new();
    let mut new_lanes: Vec<LaneRow> = Vec::new();
    let tree_of = |id: &ObjectId| graph.commits[graph.index[id]].tree;
    let mut queue =
        |lane: i16, ids: &[usize], first_addr: i64, step: i64, parent: Option<(i64, ObjectId)>| {
            let mut parent = parent;
            for (j, &c) in ids.iter().enumerate() {
                let commit = &graph.commits[c];
                let addr = first_addr + j as i64 * step;
                pending.push(Pending {
                    addr,
                    id: commit.id,
                    tree: commit.tree,
                    parent_addr: parent.map(|p| p.0),
                    parent_tree: parent.map(|p| tree_of(&p.1)),
                    lane,
                    committed_at: commit.committed_at,
                    author: commit.author.clone(),
                    subject: commit.subject.clone(),
                });
                parent = Some((addr, commit.id));
            }
        };

    // Master: the tables' commits must be a prefix of the clone's.
    let master_lane = lanes
        .iter()
        .find(|l| l.fork_addr.is_none())
        .context("the history has no master lane")?;
    let git_master = &graph.lanes[0].commits;
    let have = (master_lane.head_addr / addr::MASTER_STEP + 1) as usize;
    let head_id = by_addr
        .get(&master_lane.head_addr)
        .context("the master head is not a commit")?;
    if git_master.len() < have || graph.commits[git_master[have - 1]].id != *head_id {
        bail!("{master} was rewritten: its commit {} is gone", head_id);
    }
    if git_master.len() > have {
        let parent = (master_lane.head_addr, *head_id);
        queue(
            master_lane.id,
            &git_master[have..],
            addr::master(have as u32),
            addr::MASTER_STEP,
            Some(parent),
        );
    }

    let mut next_lane = lanes.iter().map(|l| l.id).max().unwrap_or(0) + 1;
    let mut at_fork: HashMap<i64, u32> = HashMap::new();
    for l in &lanes {
        if let Some(f) = l.fork_addr {
            *at_fork.entry(f).or_default() += 1;
        }
    }
    for lane in &graph.lanes[1..] {
        let fork = addr::master(lane.fork.context("a branch without a fork")?);
        let ids = &lane.commits;
        match lanes.iter().find(|l| l.name == lane.name) {
            Some(row) => {
                if row.fork_addr != Some(fork) {
                    bail!(
                        "branch {} now forks elsewhere; rebuild the history",
                        lane.name
                    );
                }
                let have = if row.head_addr >= row.lo {
                    (row.head_addr - row.lo + 1) as usize
                } else {
                    0
                };
                if ids.len() < have {
                    bail!("branch {} lost commits; rebuild the history", lane.name);
                }
                if have > 0 {
                    let head = by_addr[&row.head_addr];
                    if graph.commits[ids[have - 1]].id != head {
                        bail!("branch {} was rewritten; rebuild the history", lane.name);
                    }
                }
                if ids.len() > have {
                    let parent_addr = if have > 0 { row.head_addr } else { fork };
                    let parent_id = if have > 0 {
                        by_addr[&row.head_addr]
                    } else {
                        graph.commits[graph.lanes[0].commits[(fork / addr::MASTER_STEP) as usize]]
                            .id
                    };
                    queue(
                        row.id,
                        &ids[have..],
                        row.lo + have as i64,
                        1,
                        Some((parent_addr, parent_id)),
                    );
                }
            }
            None => {
                let n = at_fork.entry(fork).or_default();
                let (lo, _) = addr::branch_block((fork / addr::MASTER_STEP) as u32, *n);
                *n += 1;
                let fork_id =
                    graph.commits[graph.lanes[0].commits[(fork / addr::MASTER_STEP) as usize]].id;
                let id = next_lane;
                next_lane += 1;
                // The head moves as the lane's commits are applied.
                new_lanes.push(LaneRow {
                    id,
                    name: lane.name.clone(),
                    fork_addr: Some(fork),
                    lo,
                    head_addr: fork,
                });
                queue(id, ids, lo, 1, Some((fork, fork_id)));
                stats.new_lanes.push(lane.name.clone());
            }
        }
    }
    pending.sort_by_key(|p| p.addr);
    for p in &pending {
        known.insert(p.id, p.addr);
        by_addr.insert(p.addr, p.id);
    }

    // New lanes first, so their commits have somewhere to go.
    {
        let tx = handle.block_on(client.transaction())?;
        for l in &new_lanes {
            let (lo, hi) = addr::descendants(l.lo);
            handle.block_on(tx.execute(
                &format!(
                    "INSERT INTO {s}.code_lanes (id, name, fork_addr, lo, hi, head_addr) \
                     VALUES ($1, $2, $3, $4, $5, $6)"
                ),
                &[&l.id, &l.name, &l.fork_addr, &lo, &hi, &l.head_addr],
            ))?;
        }
        handle.block_on(tx.commit())?;
    }

    let mut ledger = MemLedger::default();
    {
        let row = handle.block_on(client.query_one(
            &format!(
                "SELECT (SELECT coalesce(max(id) + 1, 0) FROM {s}.code_line_instances), \
                        (SELECT coalesce(max(id) + 1, 0) FROM {s}.code_file_versions), \
                        (SELECT coalesce(max(id) + 1, 0) FROM {s}.code_lineages)"
            ),
            &[],
        ))?;
        ledger.next_instance = row.get::<_, i32>(0) as u32;
        ledger.next_version = row.get::<_, i32>(1) as u32;
        ledger.next_lineage = row.get::<_, i32>(2) as u32;
    }
    let mut walker = Walker::with_ledger(&repo, &paths, ledger)?;
    for batch in pending.chunks(opts.batch.max(1)) {
        let tx = handle.block_on(client.transaction())?;
        let mut heads: HashMap<i16, i64> = HashMap::new();
        for p in batch {
            let changes = walker.tree_changes(p.parent_tree, p.tree)?;
            let touched = Walker::<MemLedger>::touched(&changes);
            let mut files = match p.parent_addr {
                Some(parent) => load_files(handle, &tx, &s, &touched, parent)?,
                None => Files::new(),
            };
            walker
                .apply_changes(&mut files, changes, p.addr)
                .with_context(|| format!("cannot apply commit {}", p.id))?;
            stats.respread += flush(handle, &tx, &s, &mut walker.ledger)?;
            let at = DateTime::from_timestamp(p.committed_at, 0).unwrap_or_default();
            handle.block_on(tx.execute(
                &format!(
                    "INSERT INTO {s}.code_commits (addr, sha, lane, committed_at, author_name, subject) \
                     VALUES ($1, $2, $3, $4, $5, $6)"
                ),
                &[&p.addr, &p.id.to_string(), &p.lane, &at, &p.author, &p.subject],
            ))?;
            stats.commits += 1;
            heads.insert(p.lane, p.addr);
        }
        for (lane, head) in heads {
            handle.block_on(tx.execute(
                &format!("UPDATE {s}.code_lanes SET head_addr = $2 WHERE id = $1"),
                &[&lane, &head],
            ))?;
        }
        handle.block_on(tx.commit())?;
        tracing::info!(
            commits = stats.commits,
            of = pending.len(),
            "synced commits"
        );
    }
    stats.walk = walker.stats.clone();

    // Refs as the clone has them, and the sync's mark.
    let tx = handle.block_on(client.transaction())?;
    handle.block_on(tx.execute(&format!("DELETE FROM {s}.code_refs"), &[]))?;
    for r in &graph.refs {
        let Some(&a) = known.get(&r.id) else {
            tracing::warn!(name = %r.name, "a ref names a commit the history lacks; skipped");
            continue;
        };
        handle.block_on(tx.execute(
            &format!("INSERT INTO {s}.code_refs (kind, name, addr) VALUES ($1, $2, $3)"),
            &[&r.kind, &r.name, &a],
        ))?;
        stats.refs += 1;
    }
    let head = graph.commits[*graph.lanes[0].commits.last().context("master is empty")?]
        .id
        .to_string();
    handle.block_on(tx.execute(
        &format!(
            "UPDATE {s}.code_history SET synced_at = now(), head = $1, \
             weave_epoch = weave_epoch + $2"
        ),
        &[&head, &(i32::from(stats.respread > 0))],
    ))?;
    handle.block_on(tx.commit())?;
    if stats.commits > 0 {
        handle.block_on(client.batch_execute(&format!(
            "ANALYZE {s}.code_line_instances, {s}.code_file_versions, {s}.code_commits"
        )))?;
    }
    Ok(stats)
}

/// The files at `paths` as they are at address `at`.
fn load_files(
    handle: &Handle,
    tx: &Transaction<'_>,
    s: &str,
    paths: &[String],
    at: i64,
) -> Result<Files> {
    let mut files = Files::new();
    let rows = handle.block_on(tx.query(
        &format!(
            "SELECT v.id, v.lineage, v.path, v.blob_sha FROM {s}.code_file_versions v \
             WHERE v.path = ANY($1) AND v.lo <= $2 AND v.hi > $2 \
               AND (v.alive IS NULL OR v.alive @> $2)"
        ),
        &[&paths, &at],
    ))?;
    if rows.is_empty() {
        return Ok(files);
    }
    let lineages: Vec<i32> = rows.iter().map(|r| r.get(1)).collect();
    let mut lines: HashMap<i32, Vec<u32>> = HashMap::new();
    for r in handle.block_on(tx.query(
        &format!(
            "SELECT l.lineage, l.id FROM {s}.code_line_instances l \
             WHERE l.lineage = ANY($1) AND l.hi > $2 AND l.lo <= $2 \
               AND (l.alive IS NULL OR l.alive @> $2) ORDER BY l.lineage, l.weave"
        ),
        &[&lineages, &at],
    ))? {
        lines
            .entry(r.get(0))
            .or_default()
            .push(r.get::<_, i32>(1) as u32);
    }
    for r in rows {
        let lineage: i32 = r.get(1);
        files.insert(
            r.get(2),
            File {
                lineage: lineage as u32,
                version: r.get::<_, i32>(0) as u32,
                blob: ObjectId::from_hex(r.get::<_, &str>(3).as_bytes())?,
                lines: Arc::from(lines.remove(&lineage).unwrap_or_default()),
            },
        );
    }
    Ok(files)
}

/// Where a run of new lines goes in its lineage's weave: after `anchor`'s
/// key (or first) and before the next key.
fn bounds(
    handle: &Handle,
    tx: &Transaction<'_>,
    s: &str,
    runs: &[(u32, u32)],
) -> Result<Vec<(Option<i64>, Option<i64>)>> {
    let lineages: Vec<i32> = runs.iter().map(|r| r.0 as i32).collect();
    let anchors: Vec<i32> = runs
        .iter()
        .map(|r| if r.1 == NONE { -1 } else { r.1 as i32 })
        .collect();
    let rows = handle.block_on(tx.query(
        &format!(
            "SELECT r.n, a.weave, \
                    CASE WHEN r.anchor < 0 \
                         THEN (SELECT min(w.weave) FROM {s}.code_line_instances w \
                               WHERE w.lineage = r.lineage) \
                         ELSE (SELECT min(w.weave) FROM {s}.code_line_instances w \
                               WHERE w.lineage = r.lineage AND w.weave > a.weave) END \
             FROM unnest($1::int4[], $2::int4[]) WITH ORDINALITY AS r(lineage, anchor, n) \
             LEFT JOIN {s}.code_line_instances a ON a.id = r.anchor ORDER BY r.n"
        ),
        &[&lineages, &anchors],
    ))?;
    Ok(rows.iter().map(|r| (r.get(1), r.get(2))).collect())
}

/// `m` keys strictly between the bounds, or None when there is no room.
fn allocate(lower: Option<i64>, upper: Option<i64>, m: usize) -> Option<Vec<i64>> {
    let m = m as i128;
    let step = i128::from(WEAVE_STEP);
    let keys: Vec<i128> = match (lower.map(i128::from), upper.map(i128::from)) {
        (None, None) => (1..=m).map(|i| i * step).collect(),
        (Some(lo), None) => (1..=m).map(|i| lo + i * step).collect(),
        (None, Some(hi)) => (1..=m).map(|i| hi - (m + 1 - i) * step).collect(),
        (Some(lo), Some(hi)) => {
            let gap = hi - lo;
            if gap < m + 1 {
                return None;
            }
            let step = gap / (m + 1);
            (1..=m).map(|i| lo + i * step).collect()
        }
    };
    keys.iter()
        .all(|&k| k > i128::from(i64::MIN) && k < i128::from(i64::MAX))
        .then(|| keys.into_iter().map(|k| k as i64).collect())
}

/// Writes one commit's effects; returns how many lineages were renumbered.
fn flush(handle: &Handle, tx: &Transaction<'_>, s: &str, ledger: &mut MemLedger) -> Result<u64> {
    let mut respread = 0;
    let lineages = std::mem::take(&mut ledger.lineages);
    if !lineages.is_empty() {
        let ids: Vec<i32> = lineages.iter().map(|l| l.0 as i32).collect();
        let paths: Vec<&str> = lineages.iter().map(|l| l.1.as_str()).collect();
        let born: Vec<i64> = lineages.iter().map(|l| l.2).collect();
        handle.block_on(tx.execute(
            &format!(
                "INSERT INTO {s}.code_lineages (id, first_path, born) \
                 SELECT * FROM unnest($1::int4[], $2::text[], $3::int8[])"
            ),
            &[&ids, &paths, &born],
        ))?;
    }

    // Runs of new lines: each starts after an old line (or first) and
    // continues line after line.
    let lines = std::mem::take(&mut ledger.lines);
    let new: HashSet<u32> = lines.iter().map(|l| l.id).collect();
    let mut runs: Vec<(u32, u32, Vec<usize>)> = Vec::new();
    for (k, l) in lines.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if new.contains(&l.after) && lines[*run.2.last().unwrap()].id == l.after => {
                run.2.push(k)
            }
            _ => {
                if new.contains(&l.after) {
                    bail!("line {} follows a new line out of order", l.id);
                }
                runs.push((l.lineage, l.after, vec![k]));
            }
        }
    }
    let mut keys = vec![0i64; lines.len()];
    let anchors: Vec<(u32, u32)> = runs.iter().map(|r| (r.0, r.1)).collect();
    let mut renumbered: HashSet<u32> = HashSet::new();
    'allocation: loop {
        let found = bounds(handle, tx, s, &anchors)?;
        for (i, run) in runs.iter().enumerate() {
            match allocate(found[i].0, found[i].1, run.2.len()) {
                Some(allocated) => {
                    for (&k, key) in run.2.iter().zip(allocated) {
                        keys[k] = key;
                    }
                }
                None if renumbered.insert(run.0) => {
                    // Renumbering moves every key in the lineage: start over.
                    renumber(handle, tx, s, run.0)?;
                    respread += 1;
                    continue 'allocation;
                }
                None => bail!("no room in lineage {} even after renumbering", run.0),
            }
        }
        break;
    }
    if !lines.is_empty() {
        let ids: Vec<i32> = lines.iter().map(|l| l.id as i32).collect();
        let lineage: Vec<i32> = lines.iter().map(|l| l.lineage as i32).collect();
        let lo: Vec<i64> = lines.iter().map(|l| l.born).collect();
        let hi: Vec<i64> = lines.iter().map(|l| addr::descendants(l.born).1).collect();
        let bytes: Vec<i32> = lines.iter().map(|l| l.text.len() as i32).collect();
        let replaces: Vec<Option<i32>> = lines
            .iter()
            .map(|l| (l.replaces != NONE).then_some(l.replaces as i32))
            .collect();
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        handle.block_on(tx.execute(
            &format!(
                "INSERT INTO {s}.code_line_instances \
                     (weave, lo, hi, id, lineage, bytes, replaces, line) \
                 SELECT * FROM unnest($1::int8[], $2::int8[], $3::int8[], $4::int4[], $5::int4[], \
                                      $6::int4[], $7::int4[], $8::text[])"
            ),
            &[&keys, &lo, &hi, &ids, &lineage, &bytes, &replaces, &text],
        ))?;
    }
    let kills = std::mem::take(&mut ledger.kills);
    die(handle, tx, &format!("{s}.code_line_instances"), &kills)?;

    let versions = std::mem::take(&mut ledger.versions);
    if !versions.is_empty() {
        let ids: Vec<i32> = versions.iter().map(|v| v.0 as i32).collect();
        let lineage: Vec<i32> = versions.iter().map(|v| v.1.lineage as i32).collect();
        let path: Vec<&str> = versions.iter().map(|v| v.1.path.as_str()).collect();
        let blob: Vec<String> = versions.iter().map(|v| v.1.blob.to_string()).collect();
        let lo: Vec<i64> = versions.iter().map(|v| v.1.born).collect();
        let hi: Vec<i64> = versions
            .iter()
            .map(|v| addr::descendants(v.1.born).1)
            .collect();
        let size: Vec<i32> = versions.iter().map(|v| v.1.size).collect();
        let count: Vec<i32> = versions.iter().map(|v| v.1.line_count).collect();
        let binary: Vec<bool> = versions.iter().map(|v| v.1.binary).collect();
        let newline: Vec<bool> = versions.iter().map(|v| v.1.final_newline).collect();
        let bom: Vec<bool> = versions.iter().map(|v| v.1.bom).collect();
        let subsystem: Vec<Option<String>> = versions
            .iter()
            .map(|v| crate::subsystem_of(&v.1.path))
            .collect();
        handle.block_on(tx.execute(
            &format!(
                "INSERT INTO {s}.code_file_versions (id, lineage, path, blob_sha, lo, hi, size, \
                     line_count, is_binary, final_newline, bom, subsystem) \
                 SELECT * FROM unnest($1::int4[], $2::int4[], $3::text[], $4::text[], $5::int8[], \
                     $6::int8[], $7::int4[], $8::int4[], $9::bool[], $10::bool[], $11::bool[], \
                     $12::text[])"
            ),
            &[
                &ids, &lineage, &path, &blob, &lo, &hi, &size, &count, &binary, &newline, &bom,
                &subsystem,
            ],
        ))?;
    }
    let kills = std::mem::take(&mut ledger.version_kills);
    die(handle, tx, &format!("{s}.code_file_versions"), &kills)?;
    Ok(respread)
}

/// Subtracts each death's descendants from the rows' alive sets.
fn die(handle: &Handle, tx: &Transaction<'_>, table: &str, kills: &[(u32, i64)]) -> Result<()> {
    if kills.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = kills.iter().map(|k| k.0 as i32).collect();
    let rows = handle.block_on(tx.query(
        &format!("SELECT id, lo, hi, alive::text FROM {table} WHERE id = ANY($1)"),
        &[&ids],
    ))?;
    let mut alive: HashMap<i32, Alive> = HashMap::new();
    for r in rows {
        let ranges = match r.get::<_, Option<String>>(3) {
            Some(text) => Alive::parse(&text)?,
            None => Alive(vec![(r.get(1), r.get(2))]),
        };
        alive.insert(r.get(0), ranges);
    }
    for &(id, at) in kills {
        alive
            .get_mut(&(id as i32))
            .with_context(|| format!("{table} has no row {id}"))?
            .subtract(addr::descendants(at));
    }
    let mut ids = Vec::new();
    let mut his = Vec::new();
    let mut texts: Vec<Option<String>> = Vec::new();
    for (id, a) in alive {
        if a.0.is_empty() {
            bail!("{table} row {id} would be alive nowhere");
        }
        ids.push(id);
        his.push(a.hull().1);
        texts.push(a.has_holes().then(|| a.to_sql()));
    }
    handle.block_on(tx.execute(
        &format!(
            "UPDATE {table} t SET hi = u.hi, alive = u.alive::int8multirange \
             FROM unnest($1::int4[], $2::int8[], $3::text[]) AS u(id, hi, alive) WHERE t.id = u.id"
        ),
        &[&ids, &his, &texts],
    ))?;
    Ok(())
}

/// Renumbers a lineage's weave keys, keeping their order.
fn renumber(handle: &Handle, tx: &Transaction<'_>, s: &str, lineage: u32) -> Result<()> {
    tracing::info!(lineage, "renumbering a lineage's weave keys");
    handle.block_on(tx.execute(
        &format!(
            "UPDATE {s}.code_line_instances t SET weave = r.n * $2 \
             FROM (SELECT id, row_number() OVER (ORDER BY weave) AS n \
                   FROM {s}.code_line_instances WHERE lineage = $1) r \
             WHERE t.id = r.id"
        ),
        &[&(lineage as i32), &WEAVE_STEP],
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_keys_between_neighbours() {
        assert_eq!(
            allocate(None, None, 2),
            Some(vec![WEAVE_STEP, 2 * WEAVE_STEP])
        );
        assert_eq!(allocate(Some(10), None, 1), Some(vec![10 + WEAVE_STEP]));
        assert_eq!(
            allocate(None, Some(0), 2),
            Some(vec![-2 * WEAVE_STEP, -WEAVE_STEP])
        );
        assert_eq!(allocate(Some(0), Some(10), 4), Some(vec![2, 4, 6, 8]));
        assert_eq!(allocate(Some(0), Some(3), 2), Some(vec![1, 2]));
        assert_eq!(allocate(Some(0), Some(2), 2), None);
        assert_eq!(allocate(Some(i64::MAX - 5), None, 1), None);
        assert_eq!(allocate(None, Some(i64::MIN + 5), 1), None);
    }
}
