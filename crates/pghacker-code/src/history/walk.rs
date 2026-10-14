//! The weave builder: applies every commit, in address order, to the files
//! of its lane, recording each line instance's birth, deaths and place in
//! its lineage's weave.
//!
//! A lineage's weave is every line it ever had, in one order that agrees
//! with every version: each new line goes right after the lines it replaces,
//! or after the line before it. Lines between those neighbours in the weave
//! never coexist with it, so any place between them is right. The weave is a
//! linked list while building; its keys are numbered once at the end, so a
//! bulk build never runs out of room between two keys.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::diff::blob::sources::byte_lines;
use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::diff::tree_with_rewrites::Change;

use super::graph::Graph;

/// No instance: the end of a weave, or the place before its first line.
pub const NONE: u32 = u32::MAX;
const NO_DEATH: i64 = i64::MIN;
const BOM: &[u8] = "\u{feff}".as_bytes();

/// A file's identity across whole-file renames.
#[derive(Clone, Debug)]
pub struct Lineage {
    pub path: String,
    pub born: i64,
}

/// A file's content at one path, from the commit that made it.
#[derive(Clone, Debug)]
pub struct Version {
    pub lineage: u32,
    pub path: String,
    pub blob: ObjectId,
    pub born: i64,
    pub deaths: Vec<i64>,
    pub size: i32,
    pub line_count: i32,
    pub binary: bool,
    pub final_newline: bool,
    pub bom: bool,
}

/// Every line instance, lineage and file version the walk made.
#[derive(Default)]
pub struct Arena {
    /// Per instance: its lineage, birth address and weave successor.
    pub lineage: Vec<u32>,
    pub born: Vec<i64>,
    pub next: Vec<u32>,
    death: Vec<i64>,
    more_deaths: HashMap<u32, Vec<i64>>,
    /// Per lineage: its first instance in the weave.
    pub heads: Vec<u32>,
    pub lineages: Vec<Lineage>,
    pub versions: Vec<Version>,
}

impl Arena {
    pub fn instances(&self) -> usize {
        self.lineage.len()
    }

    /// Where an instance died: one address per lane that deleted it.
    pub fn deaths(&self, id: u32) -> impl Iterator<Item = i64> + '_ {
        let first = self.death[id as usize];
        (first != NO_DEATH)
            .then_some(first)
            .into_iter()
            .chain(self.more_deaths.get(&id).into_iter().flatten().copied())
    }

    /// A lineage's instances in weave order.
    pub fn weave(&self, lineage: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut id = self.heads[lineage as usize];
        while id != NONE {
            out.push(id);
            id = self.next[id as usize];
        }
        out
    }

    fn kill(&mut self, id: u32, at: i64) {
        let slot = &mut self.death[id as usize];
        if *slot == NO_DEATH {
            *slot = at;
        } else {
            self.more_deaths.entry(id).or_default().push(at);
        }
    }

    fn new_lineage(&mut self, path: &str, born: i64) -> u32 {
        let id = self.lineages.len() as u32;
        self.lineages.push(Lineage {
            path: path.to_owned(),
            born,
        });
        self.heads.push(NONE);
        id
    }

    /// A new instance in `lineage`'s weave, right after `after` (or first).
    fn new_instance(&mut self, lineage: u32, born: i64, after: u32) -> Result<u32> {
        let id = u32::try_from(self.lineage.len())
            .ok()
            .filter(|&id| id != NONE)
            .context("too many line instances")?;
        self.lineage.push(lineage);
        self.born.push(born);
        self.death.push(NO_DEATH);
        if after == NONE {
            let head = &mut self.heads[lineage as usize];
            self.next.push(*head);
            *head = id;
        } else {
            self.next.push(self.next[after as usize]);
            self.next[after as usize] = id;
        }
        Ok(id)
    }
}

/// What the walk did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct WalkStats {
    pub commits: u64,
    pub files_added: u64,
    pub files_deleted: u64,
    pub files_modified: u64,
    pub renames: u64,
    pub lines_added: u64,
    pub lines_deleted: u64,
    pub line_bytes: u64,
}

/// A file in a lane's current tree.
#[derive(Clone)]
struct File {
    lineage: u32,
    version: u32,
    blob: ObjectId,
    /// Its line instances, in order; empty for binary content.
    lines: Arc<[u32]>,
}

type Files = HashMap<String, File>;

/// A blob read by the loader's rules: binary with a NUL byte or invalid
/// UTF-8; a leading BOM set aside; lines split after each `\n`.
struct Blob {
    data: Vec<u8>,
    text: bool,
    bom: bool,
}

impl Blob {
    fn new(data: Vec<u8>) -> Self {
        let text = !data.contains(&0) && std::str::from_utf8(&data).is_ok();
        let bom = text && data.starts_with(BOM);
        Self { data, text, bom }
    }

    fn body(&self) -> &[u8] {
        if self.bom {
            &self.data[BOM.len()..]
        } else {
            &self.data
        }
    }

    fn final_newline(&self) -> bool {
        self.text && self.body().ends_with(b"\n")
    }

    fn line_count(&self) -> usize {
        if self.text {
            byte_lines(self.body()).count()
        } else {
            0
        }
    }
}

/// A token's text: the line without its `\n`.
fn line_text(token: &[u8]) -> &str {
    let line = token.strip_suffix(b"\n").unwrap_or(token);
    // The whole blob is UTF-8 and '\n' is a character boundary.
    std::str::from_utf8(line).expect("a text blob's line is UTF-8")
}

/// Where new lines' text goes.
pub trait LineSink {
    fn line(&mut self, id: u32, text: &str) -> Result<()>;
}

impl<F: FnMut(u32, &str) -> Result<()>> LineSink for F {
    fn line(&mut self, id: u32, text: &str) -> Result<()> {
        self(id, text)
    }
}

/// Applies commits to lanes' trees.
pub struct Walker<'r, S: LineSink> {
    repo: &'r gix::Repository,
    paths: &'r [String],
    pub arena: Arena,
    pub stats: WalkStats,
    sink: S,
    cache: gix::diff::blob::Platform,
    tree_state: gix::diff::tree::State,
}

impl<'r, S: LineSink> Walker<'r, S> {
    /// `sink` gets each new instance's id and text.
    pub fn new(repo: &'r gix::Repository, paths: &'r [String], sink: S) -> Result<Self> {
        let cache = repo
            .diff_resource_cache_for_tree_diff()
            .map_err(|e| anyhow!("cannot prepare the diff cache: {e}"))?;
        Ok(Self {
            repo,
            paths,
            arena: Arena::default(),
            stats: WalkStats::default(),
            sink,
            cache,
            tree_state: Default::default(),
        })
    }

    /// The arena, the stats and the sink, once the walk is done.
    pub fn finish(self) -> (Arena, WalkStats, S) {
        (self.arena, self.stats, self.sink)
    }

    /// Walks every commit of `graph` in address order.
    pub fn walk(&mut self, graph: &Graph) -> Result<()> {
        let started = std::time::Instant::now();
        let order = graph.order();
        let total = order.len();
        let mut master = Files::new();
        let mut branch: Option<(u16, Files)> = None;
        for (n, (c, parent)) in order.into_iter().enumerate() {
            let commit = &graph.commits[c];
            let files = if commit.lane == 0 {
                branch = None;
                &mut master
            } else {
                if branch.as_ref().map(|(lane, _)| *lane) != Some(commit.lane) {
                    // A branch starts from master's tree at its fork point.
                    branch = Some((commit.lane, master.clone()));
                }
                &mut branch.as_mut().unwrap().1
            };
            let parent_tree = parent.map(|p| graph.commits[p].tree);
            self.apply(files, parent_tree, commit.tree, commit.addr)
                .with_context(|| format!("cannot apply commit {}", commit.id))?;
            self.stats.commits += 1;
            if (n + 1) % 5_000 == 0 {
                tracing::info!(
                    commits = n + 1,
                    of = total,
                    instances = self.arena.instances(),
                    secs = started.elapsed().as_secs(),
                    "walking the history"
                );
            }
        }
        Ok(())
    }

    fn tracked(&self, path: &str) -> bool {
        crate::in_paths(path, self.paths)
    }

    fn read(&self, id: ObjectId) -> Result<Blob> {
        let blob = self
            .repo
            .find_blob(id)
            .with_context(|| format!("cannot read blob {id}"))?;
        Ok(Blob::new(blob.detach().data))
    }

    fn tree_changes(&mut self, parent: Option<ObjectId>, tree: ObjectId) -> Result<Vec<Change>> {
        let kind = tree.kind();
        let old = match parent {
            Some(id) => self.repo.find_tree(id)?.detach().data,
            None => Vec::new(),
        };
        let new = self.repo.find_tree(tree)?.detach().data;
        let mut out = Vec::new();
        gix::diff::tree_with_rewrites(
            gix::objs::TreeRefIter::from_bytes(&old, kind),
            gix::objs::TreeRefIter::from_bytes(&new, kind),
            &mut self.cache,
            &mut self.tree_state,
            &self.repo.objects,
            |change| {
                out.push(change.into_owned());
                Ok::<_, gix::Error>(std::ops::ControlFlow::Continue(()))
            },
            gix::diff::tree_with_rewrites::Options {
                location: Some(gix::diff::tree::recorder::Location::Path),
                rewrites: Some(gix::diff::Rewrites {
                    copies: None,
                    percentage: Some(0.5),
                    limit: 1000,
                    track_empty: false,
                }),
            },
        )
        .map_err(|e| anyhow!("cannot diff the trees: {e}"))?;
        self.cache.clear_resource_cache_keep_allocation();
        Ok(out)
    }

    /// Applies one commit's changes to `files`.
    fn apply(
        &mut self,
        files: &mut Files,
        parent: Option<ObjectId>,
        tree: ObjectId,
        at: i64,
    ) -> Result<()> {
        let path = |location: &gix::bstr::BString| -> Result<String> {
            location
                .to_str()
                .map(str::to_owned)
                .map_err(|_| anyhow!("a path isn't UTF-8: {location:?}"))
        };
        let mut deletions = Vec::new();
        let mut renames = Vec::new();
        let mut modifications = Vec::new();
        let mut additions = Vec::new();
        for change in self.tree_changes(parent, tree)? {
            match change {
                Change::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    if entry_mode.is_blob() {
                        additions.push((path(&location)?, id));
                    }
                }
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => {
                    if entry_mode.is_blob() {
                        deletions.push(path(&location)?);
                    }
                }
                Change::Modification {
                    location,
                    previous_entry_mode,
                    entry_mode,
                    id,
                    ..
                } => match (previous_entry_mode.is_blob(), entry_mode.is_blob()) {
                    (true, true) => modifications.push((path(&location)?, id)),
                    (true, false) => deletions.push(path(&location)?),
                    (false, true) => additions.push((path(&location)?, id)),
                    (false, false) => {}
                },
                Change::Rewrite {
                    source_location,
                    source_entry_mode,
                    location,
                    entry_mode,
                    id,
                    copy,
                    ..
                } => match (source_entry_mode.is_blob() && !copy, entry_mode.is_blob()) {
                    (true, true) => renames.push((path(&source_location)?, path(&location)?, id)),
                    (true, false) => deletions.push(path(&source_location)?),
                    (false, true) => additions.push((path(&location)?, id)),
                    (false, false) => {}
                },
            }
        }

        for p in deletions {
            if let Some(file) = files.remove(&p) {
                self.delete(file, at);
            }
        }
        // Every source leaves before any destination arrives.
        let mut moving = Vec::new();
        for (src, dst, id) in renames {
            match files.remove(&src) {
                Some(file) if self.tracked(&dst) => moving.push((file, dst, id)),
                Some(file) => self.delete(file, at),
                None if self.tracked(&dst) => additions.push((dst, id)),
                None => {}
            }
        }
        for (file, dst, id) in moving {
            self.stats.renames += 1;
            let file = self.change(file, &dst, id, at)?;
            files.insert(dst, file);
        }
        for (p, id) in modifications {
            if !self.tracked(&p) {
                continue;
            }
            match files.remove(&p) {
                // Only the mode changed, which versions don't record.
                Some(file) if file.blob == id => {
                    files.insert(p, file);
                }
                Some(file) => {
                    self.stats.files_modified += 1;
                    let file = self.change(file, &p, id, at)?;
                    files.insert(p, file);
                }
                None => additions.push((p, id)),
            }
        }
        for (p, id) in additions {
            if !self.tracked(&p) {
                continue;
            }
            if let Some(file) = files.remove(&p) {
                let file = self.change(file, &p, id, at)?;
                files.insert(p, file);
                continue;
            }
            let file = self.add(&p, id, at)?;
            files.insert(p, file);
        }
        Ok(())
    }

    fn delete(&mut self, file: File, at: i64) {
        self.stats.files_deleted += 1;
        self.arena.versions[file.version as usize].deaths.push(at);
        for &id in file.lines.iter() {
            self.arena.kill(id, at);
        }
        self.stats.lines_deleted += file.lines.len() as u64;
    }

    fn add(&mut self, path: &str, id: ObjectId, at: i64) -> Result<File> {
        self.stats.files_added += 1;
        let blob = self.read(id)?;
        let lineage = self.arena.new_lineage(path, at);
        let lines = self.insert(lineage, &blob, NONE, at)?;
        let version = self.new_version(lineage, path, id, &blob, at, lines.len())?;
        Ok(File {
            lineage,
            version,
            blob: id,
            lines: lines.into(),
        })
    }

    /// `file`'s next version: at `path` (a rename when it differs), with
    /// blob `id`.
    fn change(&mut self, file: File, path: &str, id: ObjectId, at: i64) -> Result<File> {
        self.arena.versions[file.version as usize].deaths.push(at);
        let new = self.read(id)?;
        let lines: Arc<[u32]> = if file.blob == id {
            file.lines.clone()
        } else {
            let old = self.read(file.blob)?;
            self.diff(&file, &old, &new, at)?.into()
        };
        let version = self.new_version(file.lineage, path, id, &new, at, lines.len())?;
        Ok(File {
            lineage: file.lineage,
            version,
            blob: id,
            lines,
        })
    }

    /// Every line of `blob` as new instances after `after`.
    fn insert(&mut self, lineage: u32, blob: &Blob, mut after: u32, at: i64) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        if !blob.text {
            return Ok(out);
        }
        for token in byte_lines(blob.body()) {
            after = self.born(lineage, after, at, line_text(token))?;
            out.push(after);
        }
        Ok(out)
    }

    fn born(&mut self, lineage: u32, after: u32, at: i64, text: &str) -> Result<u32> {
        let id = self.arena.new_instance(lineage, at, after)?;
        self.stats.lines_added += 1;
        self.stats.line_bytes += text.len() as u64;
        self.sink.line(id, text)?;
        Ok(id)
    }

    /// The new version's instances: the old ones it keeps and new ones for
    /// the lines Myers's diff (with git's indent heuristic) adds.
    fn diff(&mut self, file: &File, old: &Blob, new: &Blob, at: i64) -> Result<Vec<u32>> {
        if !new.text {
            for &id in file.lines.iter() {
                self.arena.kill(id, at);
            }
            self.stats.lines_deleted += file.lines.len() as u64;
            return Ok(Vec::new());
        }
        if !old.text {
            return self.insert(file.lineage, new, NONE, at);
        }
        let input = InternedInput::new(byte_lines(old.body()), byte_lines(new.body()));
        if input.before.len() != file.lines.len() {
            bail!(
                "blob {} has {} lines but its version has {}",
                file.blob,
                input.before.len(),
                file.lines.len()
            );
        }
        let mut diff = Diff::compute(Algorithm::Myers, &input);
        diff.postprocess_lines(&input);
        let old_ids = &file.lines;
        let mut out = Vec::with_capacity(input.after.len());
        let mut pos = 0usize;
        for hunk in diff.hunks() {
            let (b0, b1) = (hunk.before.start as usize, hunk.before.end as usize);
            out.extend_from_slice(&old_ids[pos..b0]);
            for &id in &old_ids[b0..b1] {
                self.arena.kill(id, at);
            }
            self.stats.lines_deleted += (b1 - b0) as u64;
            // New lines follow the lines they replace, else the line before.
            let mut after = if b1 > b0 {
                old_ids[b1 - 1]
            } else if b0 > 0 {
                old_ids[b0 - 1]
            } else {
                NONE
            };
            for j in hunk.after.clone() {
                let token = input.interner[input.after[j as usize]];
                after = self.born(file.lineage, after, at, line_text(token))?;
                out.push(after);
            }
            pos = b1;
        }
        out.extend_from_slice(&old_ids[pos..]);
        Ok(out)
    }

    fn new_version(
        &mut self,
        lineage: u32,
        path: &str,
        blob_id: ObjectId,
        blob: &Blob,
        at: i64,
        lines: usize,
    ) -> Result<u32> {
        debug_assert_eq!(lines, blob.line_count());
        let id = u32::try_from(self.arena.versions.len()).context("too many file versions")?;
        self.arena.versions.push(Version {
            lineage,
            path: path.to_owned(),
            blob: blob_id,
            born: at,
            deaths: Vec::new(),
            size: i32::try_from(blob.data.len()).map_err(|_| anyhow!("{path} is too large"))?,
            line_count: lines as i32,
            binary: !blob.text,
            final_newline: blob.final_newline(),
            bom: blob.bom,
        });
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_blobs_like_the_loader() {
        for data in [
            &b""[..],
            b"a\nb\n",
            b"a\nb",
            b"\n",
            b"a\r\nb\r\n",
            b"a\n\n",
            "\u{feff}x\n".as_bytes(),
            b"a\0b",
            &[0xff, 0xfe, b'a'],
        ] {
            let blob = Blob::new(data.to_vec());
            let lines: Vec<&str> = if blob.text {
                byte_lines(blob.body()).map(line_text).collect()
            } else {
                vec![]
            };
            match crate::split_lines(data) {
                Some((expected, final_newline)) => {
                    assert!(blob.text, "{data:?}");
                    assert_eq!(lines, expected, "{data:?}");
                    assert_eq!(blob.final_newline(), final_newline, "{data:?}");
                }
                None => assert!(!blob.text, "{data:?}"),
            }
        }
    }
}
