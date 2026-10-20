// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Code search over the commit history, which the code tools read: the
//! build in the schema PGHACKER_CODE_HISTORY names, else the history
//! installed in `pghacker`.
//!
//! The tools read a commit's file versions and line instances. A file's id
//! is its version's id. A version's lines are its lineage's instances alive
//! at the version's birth, in weave order; their weave keys are cached per
//! version, which gives line numbers, and every commit where the version is
//! alive shares the list.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

use crate::engine::{Budget, Candidates, LineQuery, Row, is_timeout};
use crate::revision::FileEntry;

pub use pghacker_core::code::HISTORY_ENV;

/// Where a commit is in a history build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub schema: String,
    pub addr: i64,
    /// The database, the build and its weave numbering: cached line keys
    /// are only reused while these stay the same.
    pub build: String,
}

/// The schema of the history the code tools read: PGHACKER_CODE_HISTORY's,
/// else `pghacker`.
pub fn schema_from_env() -> &'static str {
    pghacker_core::code::history_schema()
}

pub(crate) fn checked(schema: &str) -> Result<&str> {
    let ok = !schema.is_empty()
        && schema.len() <= 63
        && schema.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && schema
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        bail!("{schema:?} isn't a history schema name");
    }
    Ok(schema)
}

/// Where a row is alive: sorted, disjoint [lo, hi) spans, as `lo`, `hi`
/// and `alive` store them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spans(pub Vec<(i64, i64)>);

impl Spans {
    /// PostgreSQL's int8multirange text form: `{[1,5),[7,)}`.
    pub fn parse(text: &str) -> Result<Self> {
        let inner = text
            .trim()
            .strip_prefix('{')
            .and_then(|t| t.strip_suffix('}'))
            .with_context(|| format!("not a multirange: {text}"))?;
        let mut out = Vec::new();
        for part in inner.split("),").filter(|p| !p.is_empty()) {
            let part = part.trim_end_matches(')');
            let (lo, hi) = part
                .strip_prefix('[')
                .and_then(|p| p.split_once(','))
                .with_context(|| format!("not a range: {part}"))?;
            let hi = if hi.is_empty() { i64::MAX } else { hi.parse()? };
            out.push((lo.parse()?, hi));
        }
        Ok(Spans(out))
    }

    pub fn contains(&self, addr: i64) -> bool {
        self.0.iter().any(|&(a, b)| a <= addr && addr < b)
    }
}

/// SQL that row `alias` is alive at the address in parameter `param`.
pub fn alive_at(alias: &str, param: &str) -> String {
    format!(
        "{alias}.lo <= {param} AND {alias}.hi > {param} \
         AND ({alias}.alive IS NULL OR {alias}.alive @> {param})"
    )
}

/// The commit's files and the path prefixes the build kept.
pub async fn files(
    client: &impl GenericClient,
    h: &History,
) -> Result<(Vec<FileEntry>, Vec<String>)> {
    let s = checked(&h.schema)?;
    let rows = client
        .query(
            &format!(
                // A binary file has no lines to the tools.
                "SELECT v.id, v.path, v.is_binary, \
                        CASE WHEN v.is_binary THEN 0 ELSE v.line_count END, v.size \
                 FROM {s}.code_file_versions v WHERE {}",
                alive_at("v", "$1")
            ),
            &[&h.addr],
        )
        .await
        .context("cannot read the commit's files")?;
    let files = rows
        .iter()
        .map(|row| FileEntry {
            id: row.get(0),
            path: row.get(1),
            is_binary: row.get(2),
            line_count: row.get(3),
            size: row.get(4),
        })
        .collect();
    let paths: Vec<String> = client
        .query_one(&format!("SELECT paths FROM {s}.code_history"), &[])
        .await?
        .get(0);
    Ok((files, paths))
}

/// The lineages a commit has, each with its version there.
pub type LineageMap = Arc<HashMap<i32, i32>>;

type LineageCache = Mutex<Vec<((String, i64), LineageMap)>>;

static LINEAGES: LazyLock<LineageCache> = LazyLock::new(Default::default);

/// Each lineage alive at the history's commit, with its version there:
/// cached for the 16 commits read last.
pub async fn lineage_map(client: &impl GenericClient, h: &History) -> Result<LineageMap> {
    let key = (h.build.clone(), h.addr);
    if let Some(map) = LINEAGES
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, m)| m.clone())
    {
        return Ok(map);
    }
    let s = checked(&h.schema)?;
    let map: HashMap<i32, i32> = client
        .query(
            &format!(
                "SELECT v.lineage, v.id FROM {s}.code_file_versions v \
                 WHERE {} AND NOT v.is_binary",
                alive_at("v", "$1")
            ),
            &[&h.addr],
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let map = Arc::new(map);
    let mut cache = LINEAGES.lock().unwrap();
    cache.retain(|(k, _)| *k != key);
    cache.push((key, map.clone()));
    if cache.len() > 16 {
        cache.remove(0);
    }
    Ok(map)
}

/// A version's lines: their weave keys, in order.
pub type Keys = Arc<Vec<i64>>;

/// Cached keys of at most this many lines in all.
const KEY_CACHE_LINES: usize = 40_000_000;

struct KeyCache {
    entries: HashMap<(String, i32), (Keys, u64)>,
    by_use: BTreeMap<u64, (String, i32)>,
    lines: usize,
    tick: u64,
    cap: usize,
}

impl Default for KeyCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            by_use: BTreeMap::new(),
            lines: 0,
            tick: 0,
            cap: KEY_CACHE_LINES,
        }
    }
}

impl KeyCache {
    fn get(&mut self, key: &(String, i32)) -> Option<Keys> {
        self.tick += 1;
        let tick = self.tick;
        let (keys, used) = self.entries.get_mut(key)?;
        self.by_use.remove(used);
        *used = tick;
        self.by_use.insert(tick, key.clone());
        Some(keys.clone())
    }

    fn put(&mut self, key: (String, i32), keys: Keys) {
        self.tick += 1;
        self.lines += keys.len();
        if let Some((old, used)) = self.entries.insert(key.clone(), (keys, self.tick)) {
            self.lines -= old.len();
            self.by_use.remove(&used);
        }
        self.by_use.insert(self.tick, key);
        while self.lines > self.cap {
            let Some((_, oldest)) = self.by_use.pop_first() else {
                break;
            };
            if let Some((keys, _)) = self.entries.remove(&oldest) {
                self.lines -= keys.len();
            }
        }
    }
}

static KEYS: LazyLock<Mutex<KeyCache>> = LazyLock::new(Default::default);

/// The weave keys of each version's lines, from the cache or the database.
pub async fn keys(
    client: &impl GenericClient,
    h: &History,
    versions: &[i32],
) -> Result<HashMap<i32, Keys>> {
    let s = checked(&h.schema)?;
    let mut out = HashMap::new();
    let mut missing = Vec::new();
    {
        let mut cache = KEYS.lock().unwrap();
        for &v in versions {
            match cache.get(&(h.build.clone(), v)) {
                Some(keys) => {
                    out.insert(v, keys);
                }
                None => missing.push(v),
            }
        }
    }
    missing.sort_unstable();
    missing.dedup();
    if missing.is_empty() {
        return Ok(out);
    }
    let rows = client
        .query(
            &format!(
                "SELECT v.id, array_agg(l.weave ORDER BY l.weave) \
                 FROM (SELECT id, lineage, lo FROM {s}.code_file_versions \
                       WHERE id = ANY($1::int4[]) OFFSET 0) v \
                 JOIN {s}.code_line_instances l ON l.lineage = v.lineage AND {} \
                 GROUP BY v.id",
                alive_at("l", "v.lo")
            ),
            &[&missing],
        )
        .await?;
    let mut cache = KEYS.lock().unwrap();
    for row in rows {
        let v: i32 = row.get(0);
        let keys: Keys = Arc::new(row.get(1));
        cache.put((h.build.clone(), v), keys.clone());
        out.insert(v, keys);
    }
    // Versions without lines.
    for v in missing {
        out.entry(v).or_insert_with(|| {
            let empty: Keys = Arc::new(Vec::new());
            cache.put((h.build.clone(), v), empty.clone());
            empty
        });
    }
    Ok(out)
}

type Param = Box<dyn ToSql + Sync + Send>;

/// The versions to read, with their births, fenced so the planner reads
/// them once: the ids in `$1`, or every version alive at the commit `$1`.
fn versions_sql(s: &str, ids: bool) -> String {
    if ids {
        format!(
            "(SELECT id, lineage, lo FROM {s}.code_file_versions \
             WHERE id = ANY($1::int4[]) OFFSET 0)"
        )
    } else {
        // Binary files' lines (text that isn't UTF-8) are never searched.
        format!(
            "(SELECT v.id, v.lineage, v.lo FROM {s}.code_file_versions v \
             WHERE {} AND NOT v.is_binary OFFSET 0)",
            alive_at("v", "$1")
        )
    }
}

/// The lineages of versions `ids`. Trigram scans take a scope's lineages as
/// a bound array, never an `ARRAY(SELECT ...)`: PostgreSQL hashes a bound
/// array's members, but compares each row it rechecks with every member of
/// a subquery's array, and a whole tree has thousands, which took common
/// terms past 10 seconds.
pub(crate) async fn lineages_of(
    client: &impl GenericClient,
    h: &History,
    ids: &[i32],
) -> Result<Vec<i32>> {
    let s = checked(&h.schema)?;
    let rows = client
        .query(
            &format!(
                "SELECT DISTINCT lineage FROM {s}.code_file_versions \
                 WHERE id = ANY($1::int4[]) ORDER BY 1"
            ),
            &[&ids],
        )
        .await
        .context("cannot read the scope's lineages")?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

/// The lineages of a trigram scan's scope (see `lineages_of`): from the
/// commit's cached map unless the scope mounts other commits. A lineage
/// whose version at the commit is binary is left out: its lines would be
/// dropped anyway.
async fn scope_lineages(
    client: &impl GenericClient,
    q: &LineQuery<'_>,
    h: &History,
) -> Result<Vec<i32>> {
    if let (true, Some(ids)) = (q.mounted, q.file_ids) {
        return lineages_of(client, h, ids).await;
    }
    let map = lineage_map(client, h).await?;
    let mut lineages: Vec<i32> = match q.file_ids {
        Some(ids) => {
            let ids: HashSet<i32> = ids.iter().copied().collect();
            map.iter()
                .filter(|(_, v)| ids.contains(v))
                .map(|(&l, _)| l)
                .collect()
        }
        None => map.keys().copied().collect(),
    };
    lineages.sort_unstable();
    Ok(lineages)
}

/// The candidate query: rows of (version id, weave key, line).
///
/// A version's lines are its lineage's instances alive at its birth, so one
/// query reads versions of any commits: a call may mount several. Scope
/// first reads the versions' lines, then applies the text filters;
/// otherwise the trigram index finds the matching lines, once, before they
/// meet the versions, narrowed by `lineages`, the scope's (see
/// `scope_lineages`). Fences keep the planner from repeating the trigram
/// scan for each version.
pub fn line_sql(q: &LineQuery<'_>, h: &History, lineages: &[i32]) -> Result<(String, Vec<Param>)> {
    let s = checked(&h.schema)?;
    let mut params: Vec<Param> = Vec::new();
    let mut filters = String::new();
    if !q.scope_first && !q.mounted {
        // One commit: the scope's lineages and the commit's address narrow
        // the trigram scan, which PostgreSQL can AND with the scope index.
        // Rows come back by lineage; candidates() maps each to its version.
        params.push(Box::new(h.addr));
        params.push(Box::new(lineages.to_vec()));
        crate::engine::push_filters(&mut filters, &mut params, q);
        let sql = format!(
            "SELECT l.lineage, l.weave, l.line FROM {s}.code_line_instances l \
             WHERE l.lineage = ANY($2::int4[]) AND {}{filters} LIMIT {}",
            alive_at("l", "$1"),
            q.cap + 1
        );
        return Ok((sql, params));
    }
    let versions = versions_sql(s, q.file_ids.is_some());
    params.push(match q.file_ids {
        Some(ids) => Box::new(ids.to_vec()),
        None => Box::new(h.addr),
    });
    let sql = if q.scope_first {
        crate::engine::push_filters(&mut filters, &mut params, q);
        format!(
            "SELECT l.vid, l.weave, l.line FROM \
             (SELECT v.id AS vid, i.weave, i.line FROM {versions} v \
              JOIN {s}.code_line_instances i ON i.lineage = v.lineage AND i.hi > v.lo \
              WHERE i.lo <= v.lo AND (i.alive IS NULL OR i.alive @> v.lo) OFFSET 0) AS l \
             WHERE true{filters} LIMIT {}",
            q.cap + 1
        )
    } else {
        // Files of several commits: each line checked at its version's birth.
        params.push(Box::new(lineages.to_vec()));
        crate::engine::push_filters(&mut filters, &mut params, q);
        format!(
            "SELECT v.id, l.weave, l.line FROM \
             (SELECT l.lineage, l.weave, l.line, l.lo, l.hi, l.alive \
              FROM {s}.code_line_instances l \
              WHERE l.lineage = ANY($2::int4[]){filters} OFFSET 0) AS l \
             JOIN {versions} v ON v.lineage = l.lineage AND {} LIMIT {}",
            alive_at("l", "v.lo"),
            q.cap + 1
        )
    };
    Ok((sql, params))
}

/// Candidate lines with their line numbers.
pub async fn candidates(
    client: &impl GenericClient,
    q: &LineQuery<'_>,
    h: &History,
    budget: &Budget,
) -> Result<Candidates> {
    let lineages = if q.scope_first {
        Vec::new()
    } else {
        scope_lineages(client, q, h).await?
    };
    let (sql, params) = line_sql(q, h, &lineages)?;
    let mut out = Candidates::default();
    let started = std::time::Instant::now();
    let mut found =
        crate::engine::stream_rows(client, &sql, &params, q.cap, budget, &mut out, |row| {
            (
                row.get::<_, i32>(0),
                row.get::<_, i64>(1),
                row.get::<_, String>(2),
            )
        })
        .await?;
    let mut by_version = q.scope_first || q.mounted;
    if out.capped && !out.timed_out {
        found = in_tree_order(client, q, h, budget, &mut out).await?;
        by_version = true;
    }
    if !by_version {
        // The rows name lineages: each is its version at the commit.
        let map = lineage_map(client, h).await?;
        found.retain_mut(|row| match map.get(&row.0) {
            Some(&version) => {
                row.0 = version;
                true
            }
            None => false,
        });
    }
    let mut versions: Vec<i32> = found.iter().map(|r| r.0).collect();
    versions.sort_unstable();
    versions.dedup();
    if versions.is_empty() {
        return Ok(out);
    }
    if !budget.apply(client).await? {
        out.timed_out = true;
        return Ok(out);
    }
    let read_ms = started.elapsed().as_millis() as u64;
    let keys = match keys(client, h, &versions).await {
        Ok(keys) => keys,
        Err(error)
            if error
                .downcast_ref::<tokio_postgres::Error>()
                .is_some_and(is_timeout) =>
        {
            out.timed_out = true;
            crate::engine::recover(client).await;
            return Ok(out);
        }
        Err(error) => return Err(error.context("cannot number the lines")),
    };
    tracing::debug!(
        rows = found.len(),
        versions = versions.len(),
        scope_first = q.scope_first,
        read_ms,
        keys_ms = started.elapsed().as_millis() as u64 - read_ms,
        "history candidates"
    );
    for (file_id, weave, line) in found {
        let Ok(k) = keys[&file_id].binary_search(&weave) else {
            bail!("version {file_id} has no line with weave key {weave}");
        };
        out.rows.push(Row {
            file_id,
            line_no: k as i32 + 1,
            line,
        });
    }
    Ok(out)
}

/// No cap: the most rows an SQL LIMIT takes.
const UNCAPPED: usize = i64::MAX as usize - 1;

/// The first `q.cap` candidates by file in tree order, then by line, for
/// when the unordered query hit the cap and so returned an arbitrary part
/// of the matches: a capped search still gives its first matches. Reads the
/// scope's files in growing chunks, in order, until the cap.
async fn in_tree_order(
    client: &impl GenericClient,
    q: &LineQuery<'_>,
    h: &History,
    budget: &Budget,
    out: &mut Candidates,
) -> Result<Vec<(i32, i64, String)>> {
    let s = checked(&h.schema)?;
    let ordered: Vec<i32> = match q.file_ids {
        // Callers list a scope's files in tree order.
        Some(ids) => ids.to_vec(),
        None => {
            let mut all: Vec<(i32, String)> = client
                .query(
                    &format!(
                        "SELECT v.id, v.path FROM {s}.code_file_versions v \
                         WHERE {} AND NOT v.is_binary",
                        alive_at("v", "$1")
                    ),
                    &[&h.addr],
                )
                .await?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            all.sort_by(|a, b| crate::revision::tree_cmp(&a.1, &b.1));
            all.into_iter().map(|(id, _)| id).collect()
        }
    };
    let rank: HashMap<i32, usize> = ordered.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut found: Vec<(i32, i64, String)> = Vec::new();
    let mut start = 0;
    let mut chunk = 32;
    while start < ordered.len() && found.len() <= q.cap {
        let end = (start + chunk).min(ordered.len());
        let sub = LineQuery {
            history: q.history,
            mounted: q.mounted,
            file_ids: Some(&ordered[start..end]),
            require: q.require.clone(),
            reject: q.reject.clone(),
            cap: UNCAPPED,
            scope_first: true,
        };
        let (sql, params) = line_sql(&sub, h, &[])?;
        let mut sub_out = Candidates::default();
        let mut rows = crate::engine::stream_rows(
            client,
            &sql,
            &params,
            UNCAPPED,
            budget,
            &mut sub_out,
            |row| {
                (
                    row.get::<_, i32>(0),
                    row.get::<_, i64>(1),
                    row.get::<_, String>(2),
                )
            },
        )
        .await?;
        if sub_out.timed_out {
            out.timed_out = true;
            break;
        }
        rows.sort_by_key(|r| (rank[&r.0], r.1));
        found.extend(rows);
        start = end;
        chunk = (chunk * 2).min(1024);
    }
    out.capped = found.len() > q.cap;
    found.truncate(q.cap);
    Ok(found)
}

/// Lines `lo..=hi` of versions, as (version id, line number) → text.
pub async fn line_ranges(
    client: &impl GenericClient,
    h: &History,
    ranges: &[(i32, i32, i32)],
) -> Result<HashMap<(i32, i32), String>> {
    let s = checked(&h.schema)?;
    let mut out = HashMap::new();
    let versions: Vec<i32> = ranges.iter().map(|r| r.0).collect();
    let keys = keys(client, h, &versions).await?;
    let mut ids = Vec::new();
    let mut los = Vec::new();
    let mut his = Vec::new();
    for &(v, lo, hi) in ranges {
        let list = &keys[&v];
        let lo = lo.max(1) as usize;
        let hi = (hi.max(0) as usize).min(list.len());
        if lo > hi {
            continue;
        }
        ids.push(v);
        los.push(list[lo - 1]);
        his.push(list[hi - 1]);
    }
    if ids.is_empty() {
        return Ok(out);
    }
    let rows = client
        .query(
            &format!(
                "SELECT r.vid, l.weave, l.line \
                 FROM unnest($1::int4[], $2::int8[], $3::int8[]) AS r(vid, wlo, whi) \
                 JOIN {s}.code_file_versions v ON v.id = r.vid \
                 JOIN {s}.code_line_instances l ON l.lineage = v.lineage \
                     AND l.weave BETWEEN r.wlo AND r.whi AND {}",
                alive_at("l", "v.lo")
            ),
            &[&ids, &los, &his],
        )
        .await
        .context("cannot read the lines")?;
    for row in rows {
        let v: i32 = row.get(0);
        let weave: i64 = row.get(1);
        if let Ok(k) = keys[&v].binary_search(&weave) {
            out.insert((v, k as i32 + 1), row.get(2));
        }
    }
    Ok(out)
}

/// The lines that match `cond` (on `l.line`), as FROM items giving `l.id`
/// and `l.line`: the commit's (`$1`), or with `whole` false those of the
/// versions `$1`, which may be mounted from other commits, whose lineages
/// are `$3` (see `lineages_of`). The trigram scan runs once, fenced, before
/// the versions are joined.
pub fn lines_matching(h: &History, cond: &str, whole: bool) -> Result<String> {
    let s = checked(&h.schema)?;
    Ok(if whole {
        format!(
            "(SELECT l.id, l.lineage, l.line FROM {s}.code_line_instances l \
             WHERE {} AND {cond} AND l.lineage NOT IN ( \
                 SELECT b.lineage FROM {s}.code_file_versions b WHERE {} AND b.is_binary) \
             OFFSET 0) l",
            alive_at("l", "$1"),
            alive_at("b", "$1")
        )
    } else {
        // The scope's lineages narrow the scan, which may then start from
        // them rather than the whole history's trigram matches.
        format!(
            "(SELECT l.id, l.lineage, l.line, l.lo, l.hi, l.alive \
              FROM {s}.code_line_instances l \
              WHERE l.lineage = ANY($3::int4[]) AND {cond} OFFSET 0) l \
             JOIN (SELECT lineage, lo FROM {s}.code_file_versions \
                   WHERE id = ANY($1::int4[]) OFFSET 0) v \
             ON v.lineage = l.lineage AND {}",
            alive_at("l", "v.lo")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_cache_forgets_the_least_recently_used() {
        let mut cache = KeyCache {
            cap: 10,
            ..KeyCache::default()
        };
        let key = |v: i32| ("build".to_owned(), v);
        cache.put(key(1), Arc::new(vec![1, 2, 3, 4]));
        cache.put(key(2), Arc::new(vec![1, 2, 3, 4]));
        assert!(cache.get(&key(1)).is_some());
        // Version 2 is now the least recently used.
        cache.put(key(3), Arc::new(vec![1, 2, 3, 4]));
        assert!(cache.get(&key(2)).is_none());
        assert!(cache.get(&key(1)).is_some() && cache.get(&key(3)).is_some());
        assert_eq!(cache.lines, 8);
        cache.put(key(3), Arc::new(vec![1]));
        assert_eq!(cache.lines, 5);
        assert!(alive_at("l", "$1").contains("l.alive @> $1"));
        assert!(checked("pghacker_history").is_ok());
        let spans = Spans::parse("{[1,5),[9,)}").unwrap();
        assert_eq!(spans, Spans(vec![(1, 5), (9, i64::MAX)]));
        assert!(spans.contains(4) && !spans.contains(5) && spans.contains(1 << 40));
        assert!(checked("x; DROP TABLE y").is_err());
    }

    #[test]
    fn trigram_scans_bind_the_scopes_lineages() {
        let h = History {
            schema: "pghacker_history".into(),
            addr: 42,
            build: "build".into(),
        };
        let ids = [7, 8, 9];
        for (mounted, file_ids, scope_first) in [
            (false, None, false),
            (false, Some(&ids[..]), false),
            (true, Some(&ids[..]), false),
            (false, Some(&ids[..]), true),
            (true, Some(&ids[..]), true),
        ] {
            let q = LineQuery {
                history: &h,
                mounted,
                file_ids,
                require: vec![],
                reject: vec![],
                cap: 10,
                scope_first,
            };
            let (sql, params) = line_sql(&q, &h, &[3, 4]).unwrap();
            // A subquery's array is compared member by member on every row.
            assert!(!sql.contains("ARRAY(SELECT"), "{sql}");
            assert_eq!(
                sql.contains("l.lineage = ANY($2::int4[])"),
                !scope_first,
                "{sql}"
            );
            let max = (1..=9).rev().find(|n| sql.contains(&format!("${n}")));
            assert_eq!(max, Some(params.len()), "{sql}");
        }
        for whole in [true, false] {
            let sql = lines_matching(&h, "l.line ~* $2", whole).unwrap();
            assert!(!sql.contains("ARRAY(SELECT"), "{sql}");
            assert_eq!(sql.contains("ANY($3::int4[])"), !whole, "{sql}");
        }
    }
}
