//! Code search over the commit history, behind a flag while the prototype
//! is measured against the snapshots.
//!
//! The same tools read a commit's file versions and line instances instead
//! of a snapshot's files and lines. A file's id is its version's id. A
//! version's lines are its lineage's instances alive at the version's birth,
//! in weave order; their weave keys are cached per version, which gives line
//! numbers, and every commit where the version is alive shares the list.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

use crate::engine::{Budget, Candidates, LineQuery, Row, is_timeout};
use crate::snapshot::{FileEntry, Snapshot};

/// Names the schema of a history build to read code from instead of the
/// snapshots, such as `pghacker_history`.
pub const HISTORY_ENV: &str = "PGHACKER_CODE_HISTORY";

/// Where a snapshot's commit is in a history build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub schema: String,
    pub addr: i64,
    /// The database and the build's finish time: cached lists are only
    /// reused for the same build.
    pub build: String,
}

/// The history schema the flag names, if any.
pub fn schema_from_env() -> Option<&'static str> {
    static SCHEMA: LazyLock<Option<String>> = LazyLock::new(|| {
        std::env::var(HISTORY_ENV)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    });
    SCHEMA.as_deref()
}

fn checked(schema: &str) -> Result<&str> {
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

/// `snapshot`, read from the history build in `schema` at the same commit.
pub async fn attach(
    client: &impl GenericClient,
    mut snapshot: Snapshot,
    schema: &str,
) -> Result<Snapshot> {
    let s = checked(schema)?;
    let row = client
        .query_opt(
            &format!(
                "SELECT c.addr, current_database()::text || ':' || b.finished_at::text \
                 FROM {s}.code_commits c, {s}.history_build b \
                 WHERE c.sha = $1 AND b.finished_at IS NOT NULL"
            ),
            &[&snapshot.sha],
        )
        .await
        .with_context(|| format!("cannot read the history in schema {s}"))?
        .with_context(|| format!("the history in schema {s} has no commit {}", snapshot.sha))?;
    snapshot.history = Some(History {
        schema: s.to_owned(),
        addr: row.get(0),
        build: row.get(1),
    });
    Ok(snapshot)
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
                "SELECT v.id, v.path, v.is_binary, v.line_count, v.size \
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
        .query_one(&format!("SELECT paths FROM {s}.history_build"), &[])
        .await?
        .get(0);
    Ok((files, paths))
}

/// A version's lines: their weave keys, in order.
pub type Keys = Arc<Vec<i64>>;

/// Cached keys of at most this many lines in all.
const KEY_CACHE_LINES: usize = 40_000_000;

#[derive(Default)]
struct KeyCache {
    entries: HashMap<(String, i32), (Keys, u64)>,
    by_use: BTreeMap<u64, (String, i32)>,
    lines: usize,
    tick: u64,
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
        while self.lines > KEY_CACHE_LINES {
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
                 FROM {s}.code_file_versions v \
                 JOIN {s}.code_line_instances l ON l.lineage = v.lineage AND {} \
                 WHERE v.id = ANY($1::int4[]) GROUP BY v.id",
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

/// The candidate query: rows of (version id, weave key, line).
pub fn line_sql(q: &LineQuery<'_>, h: &History) -> Result<(String, Vec<Param>)> {
    let s = checked(&h.schema)?;
    let mut params: Vec<Param> = vec![Box::new(h.addr)];
    let scope = match q.file_ids {
        Some(ids) => {
            params.push(Box::new(ids.to_vec()));
            "v.id = ANY($2::int4[])"
        }
        None => "true",
    };
    let mut sql = if q.scope_first {
        // The scope's lines first, fenced from the text predicates.
        format!(
            "SELECT l.vid, l.weave, l.line FROM \
             (SELECT v.id AS vid, i.weave, i.line FROM {s}.code_file_versions v \
              JOIN {s}.code_line_instances i ON i.lineage = v.lineage AND i.hi > $1 \
              WHERE {} AND {scope} AND i.lo <= $1 AND (i.alive IS NULL OR i.alive @> $1) \
              OFFSET 0) AS l WHERE true",
            alive_at("v", "$1")
        )
    } else {
        format!(
            "SELECT v.id, l.weave, l.line FROM {s}.code_line_instances l \
             JOIN {s}.code_file_versions v ON v.lineage = l.lineage AND {} \
             WHERE {} AND {scope}",
            alive_at("v", "$1"),
            alive_at("l", "$1")
        )
    };
    crate::engine::push_filters(&mut sql, &mut params, q);
    sql.push_str(&format!(" LIMIT {}", q.cap + 1));
    Ok((sql, params))
}

/// Candidate lines with their line numbers.
pub async fn candidates(
    client: &impl GenericClient,
    q: &LineQuery<'_>,
    h: &History,
    budget: &Budget,
) -> Result<Candidates> {
    let (sql, params) = line_sql(q, h)?;
    let mut out = Candidates::default();
    let found = crate::engine::stream_rows(client, &sql, &params, q.cap, budget, &mut out, |row| {
        (
            row.get::<_, i32>(0),
            row.get::<_, i64>(1),
            row.get::<_, String>(2),
        )
    })
    .await?;
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
    let keys = match keys(client, h, &versions).await {
        Ok(keys) => keys,
        Err(error)
            if error
                .downcast_ref::<tokio_postgres::Error>()
                .is_some_and(is_timeout) =>
        {
            out.timed_out = true;
            return Ok(out);
        }
        Err(error) => return Err(error.context("cannot number the lines")),
    };
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

/// The FROM and WHERE of the spelling queries: the commit's lines as `l`
/// (with `l.id` and `l.line`), in the versions `$3` or all of them.
pub fn spelling_scope(h: &History, whole: bool) -> Result<String> {
    let s = checked(&h.schema)?;
    let scope = if whole {
        ""
    } else {
        " AND v.id = ANY($3::int4[])"
    };
    Ok(format!(
        "{s}.code_line_instances l JOIN {s}.code_file_versions v \
         ON v.lineage = l.lineage AND {}{scope} WHERE {}",
        alive_at("v", "$1"),
        alive_at("l", "$1")
    ))
}
