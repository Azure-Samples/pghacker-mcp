// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The commit graph: master, the branches forked from it, and the tags.
//!
//! PostgreSQL's history is a tree: no merges, one root, and every branch
//! forks from master. The builder relies on that and refuses a branch that
//! forks from another branch; a merge is followed along its first parent.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;

use super::addr;

/// A commit and where it sits.
#[derive(Clone, Debug)]
pub struct Commit {
    pub id: ObjectId,
    pub tree: ObjectId,
    pub addr: i64,
    pub lane: u16,
    /// Seconds since the epoch.
    pub committed_at: i64,
    pub authored_at: i64,
    pub author: String,
    pub committer: String,
    pub subject: String,
    /// The whole message, subject first.
    pub message: String,
}

/// Master or a branch: a line of commits.
#[derive(Clone, Debug)]
pub struct Lane {
    pub id: u16,
    pub name: String,
    /// The master commit index the branch forks at; `None` for master.
    pub fork: Option<u32>,
    /// The lane's addresses, [lo, hi).
    pub block: (i64, i64),
    /// Oldest first: indexes into [`Graph::commits`].
    pub commits: Vec<usize>,
}

impl Lane {
    /// The address of the lane's head: its newest commit, or the fork point
    /// for a branch without commits of its own.
    pub fn head_addr(&self, graph: &Graph) -> i64 {
        match (self.commits.last(), self.fork) {
            (Some(&c), _) => graph.commits[c].addr,
            (None, Some(fork)) => addr::master(fork),
            (None, None) => 0,
        }
    }
}

/// A branch or tag and the commit it names.
#[derive(Clone, Debug)]
pub struct Ref {
    pub name: String,
    pub kind: &'static str,
    pub id: ObjectId,
}

#[derive(Debug)]
pub struct Graph {
    pub commits: Vec<Commit>,
    /// Master first, then branches by fork point and order there.
    pub lanes: Vec<Lane>,
    pub refs: Vec<Ref>,
    pub index: HashMap<ObjectId, usize>,
}

impl Graph {
    /// Reads master (`master_name`), every other branch and every tag.
    pub fn read(repo: &gix::Repository, master_name: &str) -> Result<Graph> {
        let mut heads: BTreeMap<String, ObjectId> = BTreeMap::new();
        let mut tags: BTreeMap<String, ObjectId> = BTreeMap::new();
        let references = repo.references().context("cannot read the references")?;
        for reference in references
            .local_branches()
            .context("cannot read the branches")?
        {
            let mut reference = reference.map_err(|e| anyhow!("cannot read a branch: {e}"))?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            let id = reference
                .peel_to_id()
                .with_context(|| format!("cannot resolve branch {name}"))?
                .detach();
            heads.insert(name, id);
        }
        for reference in references.tags().context("cannot read the tags")? {
            let mut reference = reference.map_err(|e| anyhow!("cannot read a tag: {e}"))?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            let id = reference
                .peel_to_id()
                .with_context(|| format!("cannot resolve tag {name}"))?
                .detach();
            if repo.find_commit(id).is_ok() {
                tags.insert(name, id);
            } else {
                tracing::warn!(%name, "the tag names no commit; skipped");
            }
        }
        let master_head = *heads
            .get(master_name)
            .with_context(|| format!("the repository has no branch {master_name}"))?;

        let mut graph = Graph {
            commits: Vec::new(),
            lanes: Vec::new(),
            refs: Vec::new(),
            index: HashMap::new(),
        };
        // Master, root first.
        let master_ids = first_parent_chain(repo, master_head, |_| false)?;
        if master_ids.len() >= (1 << 31) {
            bail!("master has too many commits");
        }
        let mut master_lane = Lane {
            id: 0,
            name: master_name.to_owned(),
            fork: None,
            block: (0, addr::OPEN),
            commits: Vec::with_capacity(master_ids.len()),
        };
        for (k, id) in master_ids.iter().rev().enumerate() {
            let c = graph.push(repo, *id, addr::master(k as u32), 0)?;
            master_lane.commits.push(c);
        }
        graph.lanes.push(master_lane);

        // Branches: their own commits back to master.
        struct Pending {
            name: String,
            fork: u32,
            own: Vec<ObjectId>,
            sort_time: i64,
        }
        let mut owner: HashMap<ObjectId, String> = HashMap::new();
        let mut pending: Vec<Pending> = Vec::new();
        for (name, head) in &heads {
            if name == master_name {
                continue;
            }
            let chain = first_parent_chain(repo, *head, |id| graph.index.contains_key(id))?;
            let fork_id = match chain.last() {
                Some(last) if graph.index.contains_key(last) => *last,
                _ => bail!("branch {name} does not fork from {master_name}"),
            };
            let own: Vec<ObjectId> = chain
                .into_iter()
                .filter(|id| !graph.index.contains_key(id))
                .rev()
                .collect();
            for id in &own {
                if let Some(other) = owner.insert(*id, name.clone()) {
                    bail!(
                        "branch {name} forks from branch {other}; only branches from {master_name} are supported"
                    );
                }
            }
            let fork = addr::fork_of(graph.commits[graph.index[&fork_id]].addr);
            let first = own.first().copied().unwrap_or(*head);
            let sort_time = repo
                .find_commit(first)?
                .time()
                .map(|t| t.seconds)
                .unwrap_or(i64::MAX);
            pending.push(Pending {
                name: name.clone(),
                fork,
                own,
                sort_time,
            });
        }
        // Branches forking at one commit are numbered in creation order.
        pending.sort_by(|a, b| (a.fork, a.sort_time, &a.name).cmp(&(b.fork, b.sort_time, &b.name)));
        let mut at_fork: HashMap<u32, u32> = HashMap::new();
        for p in pending {
            let n = at_fork.entry(p.fork).or_insert(0);
            let block = addr::branch_block(p.fork, *n);
            *n += 1;
            if p.own.len() as i64 >= addr::BRANCH_BLOCK {
                bail!("branch {} has too many commits", p.name);
            }
            let lane_id = u16::try_from(graph.lanes.len()).context("too many branches")?;
            let mut lane = Lane {
                id: lane_id,
                name: p.name,
                fork: Some(p.fork),
                block,
                commits: Vec::with_capacity(p.own.len()),
            };
            for (j, id) in p.own.iter().enumerate() {
                let c = graph.push(repo, *id, block.0 + j as i64, lane_id)?;
                lane.commits.push(c);
            }
            graph.lanes.push(lane);
        }

        for (name, id) in heads {
            graph.refs.push(Ref {
                name,
                kind: "branch",
                id,
            });
        }
        for (name, id) in tags {
            if !graph.index.contains_key(&id) {
                tracing::warn!(%name, "the tag's commit is on no branch; skipped");
                continue;
            }
            graph.refs.push(Ref {
                name,
                kind: "tag",
                id,
            });
        }
        Ok(graph)
    }

    fn push(
        &mut self,
        repo: &gix::Repository,
        id: ObjectId,
        address: i64,
        lane: u16,
    ) -> Result<usize> {
        let commit = repo
            .find_commit(id)
            .with_context(|| format!("cannot read commit {id}"))?;
        let tree = commit.tree_id()?.detach();
        let committed_at = commit.time().map(|t| t.seconds).unwrap_or(0);
        let (author, authored_at) = commit
            .author()
            .map(|a| {
                let at = a.time().map(|t| t.seconds).unwrap_or(committed_at);
                (a.name.to_str_lossy().into_owned(), at)
            })
            .unwrap_or_default();
        let committer = commit
            .committer()
            .map(|c| c.name.to_str_lossy().into_owned())
            .unwrap_or_default();
        let subject = commit
            .message()
            .map(|m| m.summary().to_str_lossy().into_owned())
            .unwrap_or_default();
        let message = commit
            .message_raw()
            .map(|m| m.to_str_lossy().trim_end().to_owned())
            .unwrap_or_default();
        let c = self.commits.len();
        self.commits.push(Commit {
            id,
            tree,
            addr: address,
            lane,
            committed_at,
            authored_at,
            author,
            committer,
            subject,
            message,
        });
        self.index.insert(id, c);
        Ok(c)
    }

    /// The commits in processing order: each master commit, then every
    /// branch forked at it, whole. Each comes with the commit whose tree it
    /// changes (`None` for the root).
    pub fn order(&self) -> Vec<(usize, Option<usize>)> {
        let mut by_fork: HashMap<u32, Vec<&Lane>> = HashMap::new();
        for lane in &self.lanes[1..] {
            by_fork.entry(lane.fork.unwrap()).or_default().push(lane);
        }
        let mut out = Vec::with_capacity(self.commits.len());
        let mut parent = None;
        for (k, &c) in self.lanes[0].commits.iter().enumerate() {
            out.push((c, parent));
            parent = Some(c);
            for lane in by_fork.get(&(k as u32)).into_iter().flatten() {
                let mut branch_parent = Some(c);
                for &b in &lane.commits {
                    out.push((b, branch_parent));
                    branch_parent = Some(b);
                }
            }
        }
        out
    }
}

/// `head` and its first parents, newest first, until `stop` holds for a
/// commit (which is included) or the root.
fn first_parent_chain(
    repo: &gix::Repository,
    head: ObjectId,
    stop: impl Fn(&ObjectId) -> bool,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut id = head;
    loop {
        out.push(id);
        if stop(&id) {
            break;
        }
        let commit = repo
            .find_commit(id)
            .with_context(|| format!("cannot read commit {id}"))?;
        let mut parents = commit.parent_ids();
        let Some(parent) = parents.next() else { break };
        if parents.next().is_some() {
            tracing::warn!(%id, "a merge commit: following its first parent");
        }
        id = parent.detach();
    }
    Ok(out)
}
