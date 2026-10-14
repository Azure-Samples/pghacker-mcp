//! Back-patch groups: commits that apply one change to several lanes, as
//! PostgreSQL back-patches fixes to its supported branches. Commits group
//! when they are on different lanes and have the same patch id, or the same
//! subject and author within a week of each other and a changed file in
//! common. A group's commits share its first commit's address, master's
//! when it has one, in code_commits.backpatch.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tokio_postgres::GenericClient;

use super::addr::MASTER_STEP;

/// How far apart, by commit time, a group's commits may be when only their
/// subject and author match.
const WINDOW: i64 = 7 * 24 * 3600;

struct Row {
    addr: i64,
    lane: i16,
    at: i64,
}

/// Union-find over row indexes.
struct Sets(Vec<usize>);

impl Sets {
    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b)] = a.min(b);
        }
    }
}

/// Groups every commit afresh and stores the groups. Returns how many
/// commits are in a group.
pub async fn regroup(client: &impl GenericClient, schema: &str) -> Result<u64> {
    let s = super::store::readable_schema(schema)?;
    let rows = client
        .query(
            &format!(
                "SELECT addr, lane, extract(epoch FROM committed_at)::int8, author_name, subject, \
                        patch_id \
                 FROM {s}.code_commits ORDER BY committed_at, addr"
            ),
            &[],
        )
        .await?;
    let commits: Vec<Row> = rows
        .iter()
        .map(|r| Row {
            addr: r.get(0),
            lane: r.get(1),
            at: r.get(2),
        })
        .collect();
    let mut sets = Sets((0..commits.len()).collect());

    let mut by_patch: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_message: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(p) = r.get::<_, Option<&str>>(5) {
            by_patch.entry(p).or_default().push(i);
        }
        by_message.entry((r.get(3), r.get(4))).or_default().push(i);
    }
    for members in by_patch.values() {
        for &j in &members[1..] {
            if members.iter().any(|&i| commits[i].lane != commits[j].lane) {
                sets.union(members[0], j);
            }
        }
    }

    // Subject and author: pairs on different lanes within the window, which
    // then need a changed file in common.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for members in by_message.values() {
        for (n, &i) in members.iter().enumerate() {
            for &j in &members[n + 1..] {
                if commits[j].at - commits[i].at > WINDOW {
                    break;
                }
                if commits[i].lane != commits[j].lane {
                    pairs.push((i, j));
                }
            }
        }
    }
    if !pairs.is_empty() {
        let mut wanted: Vec<i64> = pairs
            .iter()
            .flat_map(|&(i, j)| [commits[i].addr, commits[j].addr])
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        let mut paths: HashMap<i64, HashSet<String>> = HashMap::new();
        for r in client
            .query(
                &format!(
                    "SELECT addr, path FROM {s}.code_commit_files WHERE addr = ANY($1) \
                     UNION ALL \
                     SELECT addr, old_path FROM {s}.code_commit_files \
                     WHERE addr = ANY($1) AND old_path IS NOT NULL"
                ),
                &[&wanted],
            )
            .await?
        {
            paths.entry(r.get(0)).or_default().insert(r.get(1));
        }
        let empty = HashSet::new();
        for (i, j) in pairs {
            let a = paths.get(&commits[i].addr).unwrap_or(&empty);
            let b = paths.get(&commits[j].addr).unwrap_or(&empty);
            if !a.is_disjoint(b) {
                sets.union(i, j);
            }
        }
    }

    // Each group's representative: its first master commit, else its first.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..commits.len() {
        let root = sets.find(i);
        groups.entry(root).or_default().push(i);
    }
    let mut addrs = Vec::with_capacity(commits.len());
    let mut backpatch: Vec<Option<i64>> = Vec::with_capacity(commits.len());
    let mut grouped = 0u64;
    for members in groups.values() {
        let rep = (members.len() > 1).then(|| {
            members
                .iter()
                .map(|&i| &commits[i])
                .filter(|c| c.addr % MASTER_STEP == 0)
                .map(|c| c.addr)
                .min()
                .unwrap_or(commits[members[0]].addr)
        });
        if rep.is_some() {
            grouped += members.len() as u64;
        }
        for &i in members {
            addrs.push(commits[i].addr);
            backpatch.push(rep);
        }
    }
    client
        .execute(
            &format!(
                "UPDATE {s}.code_commits c SET backpatch = u.backpatch \
                 FROM unnest($1::int8[], $2::int8[]) AS u(addr, backpatch) \
                 WHERE c.addr = u.addr AND c.backpatch IS DISTINCT FROM u.backpatch"
            ),
            &[&addrs, &backpatch],
        )
        .await?;
    Ok(grouped)
}
