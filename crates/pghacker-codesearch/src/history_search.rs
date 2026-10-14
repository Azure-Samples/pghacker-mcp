//! search over the commit history: every line that some commit in a range
//! had (or any commit, for `all`), once per path and text, with the commit
//! that added it, those that removed it, and the branch heads and releases
//! that have it.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tokio_postgres::GenericClient;
use tokio_postgres::types::ToSql;

use crate::engine::{self, Budget, Candidates, LineQuery, MAX_CANDIDATES};
use crate::history::{History, alive_at, checked, keys};
use crate::range::{Range, death_at};
use crate::search::{Patterns, SearchArgs, SearchResult};
use crate::snapshot::{Snapshot, tree_cmp};

type Param = Box<dyn ToSql + Sync + Send>;

/// A commit, briefly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub date: String,
    pub branch: String,
    pub subject: String,
}

/// The releases (tags) that have a line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Releases {
    pub first: String,
    pub last: String,
    pub count: u32,
}

/// A line in history: one per path and text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HistoryHit {
    /// Where the line was added.
    pub path: String,
    pub text: String,
    /// The commit that first added it, and its line number there.
    pub added: CommitInfo,
    pub line: u32,
    /// Times it was added: more when it was removed and added again, or
    /// added on several branches.
    pub added_times: u32,
    /// The commits that removed it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<CommitInfo>,
    /// The branch heads that have it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub releases: Option<Releases>,
}

/// A line instance a search found.
struct Instance {
    lo: i64,
    alive: crate::history::Spans,
    weave: i64,
    version: i32,
}

/// The commits `spec` names: None for all of history.
pub(crate) async fn range_of(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    h: &History,
    spec: &str,
) -> Result<Option<Range>> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let at = |rev: String| {
        let repo = snapshot.repo.clone();
        async move {
            crate::rev::resolve(client, &h.schema, &repo, Some(&rev))
                .await
                .map(|s| s.history.map_or(0, |h| h.addr))
        }
    };
    Ok(Some(match spec.split_once("..") {
        Some((since, upto)) => {
            if since.trim().is_empty() || upto.trim().is_empty() {
                bail!("a range needs both ends, as REL_16_STABLE..REL_17_STABLE");
            }
            Range::between(
                Some(at(since.to_owned()).await?),
                at(upto.to_owned()).await?,
            )
        }
        None => Range::ancestors(at(spec.to_owned()).await?),
    }))
}

/// A line instance the patterns may match, alive somewhere in the range.
pub(crate) struct Matched {
    pub lineage: i32,
    pub lo: i64,
    pub alive: crate::history::Spans,
    pub weave: i64,
    pub line: String,
    /// Where it was born: the path and version.
    pub path: String,
    pub version: i32,
}

/// Instances whose text passes the SQL filters (callers check exactly),
/// in `scope`'s lineages, alive somewhere in `range`.
pub(crate) async fn matching(
    client: &impl GenericClient,
    h: &History,
    require: Vec<&crate::pattern::Filter>,
    reject: Vec<&crate::pattern::Filter>,
    scope: Option<Vec<i32>>,
    range: Option<&Range>,
    budget: &Budget,
) -> Result<(Vec<Matched>, Candidates)> {
    let s = checked(&h.schema)?;
    let q = LineQuery {
        snapshot_id: 0,
        history: Some(h),
        file_ids: None,
        require,
        reject,
        cap: MAX_CANDIDATES,
        scope_first: false,
    };
    let mut params: Vec<Param> = Vec::new();
    let mut conds = String::new();
    engine::push_filters(&mut conds, &mut params, &q);
    if let Some(scope) = scope {
        params.push(Box::new(scope));
        conds.push_str(&format!(" AND l.lineage = ANY(${}::int4[])", params.len()));
    }
    if let Some(r) = range {
        let (lo, hi) = r.hull();
        params.push(Box::new(hi));
        conds.push_str(&format!(" AND l.lo < ${}", params.len()));
        params.push(Box::new(lo));
        conds.push_str(&format!(" AND l.hi > ${}", params.len()));
    }
    let sql = format!(
        "SELECT l.lo, l.hi, l.alive::text, l.weave, l.line, v.path, v.id, l.lineage FROM \
         (SELECT l.lineage, l.lo, l.hi, l.alive, l.weave, l.line \
          FROM {s}.code_line_instances l WHERE true{conds} OFFSET 0) AS l \
         JOIN {s}.code_file_versions v ON v.lineage = l.lineage AND {} \
         LIMIT {}",
        alive_at("v", "l.lo"),
        MAX_CANDIDATES + 1
    );
    let mut found = Candidates::default();
    let rows = engine::stream_rows(
        client,
        &sql,
        &params,
        MAX_CANDIDATES,
        budget,
        &mut found,
        |r| {
            (
                r.get::<_, i64>(0),
                r.get::<_, i64>(1),
                r.get::<_, Option<String>>(2),
                r.get::<_, i64>(3),
                r.get::<_, String>(4),
                r.get::<_, String>(5),
                r.get::<_, i32>(6),
                r.get::<_, i32>(7),
            )
        },
    )
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (lo, hi, alive, weave, line, path, version, lineage) in rows {
        let alive = match alive {
            Some(text) => crate::history::Spans::parse(&text)?,
            None => crate::history::Spans(vec![(lo, hi)]),
        };
        if let Some(r) = range
            && !alive.0.iter().any(|&(a, b)| r.meets(a, b))
        {
            continue;
        }
        out.push(Matched {
            lineage,
            lo,
            alive,
            weave,
            line,
            path,
            version,
        });
    }
    Ok((out, found))
}

/// The lineages that ever had a file at or below `paths`.
pub(crate) async fn lineages(
    client: &impl GenericClient,
    s: &str,
    paths: &[String],
) -> Result<Vec<i32>> {
    let exact: Vec<String> = paths.to_vec();
    let below: Vec<String> = paths
        .iter()
        .map(|p| {
            format!(
                "{}/%",
                p.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        })
        .collect();
    Ok(client
        .query(
            &format!(
                "SELECT DISTINCT lineage FROM {s}.code_file_versions \
                 WHERE path = ANY($1) OR path LIKE ANY($2)"
            ),
            &[&exact, &below],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// Searches the history of `snapshot`'s repository, in `args.history`.
pub(crate) async fn search(
    client: &impl GenericClient,
    snapshot: &Snapshot,
    args: &SearchArgs,
    patterns: &Patterns<'_>,
    budget: &Budget,
) -> Result<SearchResult> {
    let Some(h) = &snapshot.history else {
        bail!("history search reads the commit history, which this deployment doesn't serve yet");
    };
    let spec = args.history.as_deref().unwrap_or("all");
    let s = checked(&h.schema)?;
    let range = range_of(client, snapshot, h, spec).await?;
    let filter = crate::glob::RgFilter::new(&args.include.to_vec(), &args.exclude.to_vec())
        .map_err(|e| anyhow!(e))?;
    let raw_paths: Vec<String> = args
        .paths
        .to_vec()
        .into_iter()
        .map(|p| {
            p.trim()
                .trim_start_matches("./")
                .trim_matches('/')
                .to_owned()
        })
        .filter(|p| !p.is_empty() && p != ".")
        .collect();
    if raw_paths.iter().any(|p| p.contains(['*', '?', '[', '{'])) {
        bail!("history search takes folders and files, not globs: use include and exclude");
    }

    let mut require = vec![&patterns.query.filter];
    require.extend(patterns.also.iter().map(|c| &c.filter));
    let reject: Vec<&crate::pattern::Filter> =
        patterns.not.map(|n| vec![&n.filter]).unwrap_or_default();
    let scope = if raw_paths.is_empty() {
        None
    } else {
        Some(lineages(client, s, &raw_paths).await?)
    };
    let (rows, found) = matching(client, h, require, reject, scope, range.as_ref(), budget).await?;
    // Lines alive somewhere in the range, by path and text.
    let mut groups: HashMap<(String, String), Vec<Instance>> = HashMap::new();
    for m in rows {
        if !filter.allows("", &m.path) || !patterns.is_match(&m.line) {
            continue;
        }
        groups.entry((m.path, m.line)).or_default().push(Instance {
            lo: m.lo,
            alive: m.alive,
            weave: m.weave,
            version: m.version,
        });
    }
    let mut ordered: Vec<((String, String), Vec<Instance>)> = groups.into_iter().collect();
    for (_, instances) in &mut ordered {
        instances.sort_by_key(|i| i.lo);
    }
    ordered.sort_by(|a, b| {
        tree_cmp(&a.0.0, &b.0.0)
            .then_with(|| a.1[0].lo.cmp(&b.1[0].lo))
            .then_with(|| a.0.1.cmp(&b.0.1))
    });
    let total_files = {
        let mut paths: Vec<&str> = ordered.iter().map(|g| g.0.0.as_str()).collect();
        paths.dedup();
        paths.len() as u64
    };
    let limit = args
        .limit
        .unwrap_or(crate::search::DEFAULT_LIMIT)
        .clamp(1, crate::search::MAX_LIMIT) as usize;
    let offset = args.offset as usize;
    let page: Vec<&((String, String), Vec<Instance>)> =
        ordered.iter().skip(offset).take(limit).collect();

    // The page's commits, line numbers and refs.
    let mut addrs: Vec<i64> = Vec::new();
    for (_, instances) in &page {
        addrs.push(instances[0].lo);
        for i in instances.iter() {
            addrs.extend(i.alive.0.iter().filter_map(|&(_, b)| death_at(b)));
        }
    }
    addrs.sort_unstable();
    addrs.dedup();
    let commits: HashMap<i64, CommitInfo> = client
        .query(
            &format!(
                "SELECT c.addr, c.sha, to_char(c.committed_at, 'YYYY-MM-DD'), l.name, c.subject \
                 FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane \
                 WHERE c.addr = ANY($1)"
            ),
            &[&addrs],
        )
        .await?
        .iter()
        .map(|r| {
            (
                r.get(0),
                CommitInfo {
                    sha: r.get(1),
                    date: r.get(2),
                    branch: r.get(3),
                    subject: r.get(4),
                },
            )
        })
        .collect();
    let born_versions: Vec<i32> = page.iter().map(|g| g.1[0].version).collect();
    let lines = keys(client, h, &born_versions).await?;
    let refs = client
        .query(
            &format!(
                "SELECT r.kind, r.name, r.addr FROM {s}.code_refs r \
                 JOIN {s}.code_commits c ON c.addr = r.addr ORDER BY c.committed_at, r.name"
            ),
            &[],
        )
        .await?;

    let mut hits = Vec::new();
    for ((path, text), instances) in page {
        let first = &instances[0];
        let Some(added) = commits.get(&first.lo).cloned() else {
            bail!("the history has no commit at {}", first.lo);
        };
        let line = lines
            .get(&first.version)
            .and_then(|k| k.binary_search(&first.weave).ok())
            .map_or(0, |k| k as u32 + 1);
        let mut deaths: Vec<i64> = instances
            .iter()
            .flat_map(|i| i.alive.0.iter().filter_map(|&(_, b)| death_at(b)))
            .collect();
        deaths.sort_unstable();
        deaths.dedup();
        let has = |addr: i64| instances.iter().any(|i| i.alive.contains(addr));
        let mut branches = Vec::new();
        let mut tags: Vec<String> = Vec::new();
        for r in &refs {
            let (kind, name, addr): (&str, String, i64) = (r.get(0), r.get(1), r.get(2));
            if has(addr) {
                if kind == "branch" {
                    branches.push(name);
                } else {
                    tags.push(name);
                }
            }
        }
        branches.sort();
        hits.push(HistoryHit {
            path: path.clone(),
            text: text.clone(),
            added,
            line,
            added_times: instances.len() as u32,
            removed: deaths
                .iter()
                .filter_map(|d| commits.get(d).cloned())
                .collect(),
            branches,
            releases: (!tags.is_empty()).then(|| Releases {
                first: tags[0].clone(),
                last: tags[tags.len() - 1].clone(),
                count: tags.len() as u32,
            }),
        });
    }
    let total = ordered.len() as u64;
    let mut notes = Vec::new();
    if found.capped {
        notes.push(format!(
            "more than {MAX_CANDIDATES} candidate lines: narrow the search; totals are lower bounds"
        ));
    }
    Ok(SearchResult {
        repo: snapshot.repo.clone(),
        version: snapshot.version.clone(),
        git_ref: snapshot.git_ref.clone(),
        sha: snapshot.sha.clone(),
        total_lines: total,
        total_files,
        total_matches: None,
        offset: args.offset,
        returned: hits.len() as u32,
        cut: offset + hits.len() < ordered.len(),
        budget_exceeded: found.timed_out,
        complete: !found.timed_out && !found.capped,
        lines: Vec::new(),
        files: Vec::new(),
        matches: Vec::new(),
        similar: None,
        notes,
        history: hits,
        range: Some(spec.to_owned()),
        elapsed_ms: budget.elapsed_ms(),
    })
}
