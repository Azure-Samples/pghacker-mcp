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
use sha2::{Digest, Sha256};

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

/// One file a commit changed, as git show --stat counts it. Changes of
/// mode alone, symlinks and submodules aren't recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// Where a rename moved the file from.
    pub old_path: Option<String>,
    /// A(dded), D(eleted), M(odified), or R(enamed, perhaps modified too).
    pub status: char,
    pub added: u32,
    pub removed: u32,
    /// Either side is binary: no lines counted.
    pub binary: bool,
    /// The version the commit made (none when deleted) and the one it
    /// ended (none when added).
    pub version: Option<u32>,
    pub old_version: Option<u32>,
}

/// What one commit changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitChanges {
    pub files: Vec<FileChange>,
    /// A hash of the changes without whitespace, line numbers or context,
    /// independent of file order: equal for a commit and its exact
    /// back-patches. Like git patch-id --stable, but not the same value.
    pub patch_id: Option<String>,
}

/// One file's change as the walk sees it: line counts, and a hash of its
/// paths and changed lines without whitespace.
struct Delta {
    added: u32,
    removed: u32,
    hash: Sha256,
}

impl Delta {
    fn new(header: &[&str]) -> Self {
        let mut hash = Sha256::new();
        for part in header {
            hash.update(part.as_bytes());
            hash.update([0u8]);
        }
        Self {
            added: 0,
            removed: 0,
            hash,
        }
    }

    fn line(&mut self, sign: u8, token: &[u8]) {
        if sign == b'+' {
            self.added += 1;
        } else {
            self.removed += 1;
        }
        self.hash.update([sign]);
        for part in token.split(u8::is_ascii_whitespace) {
            self.hash.update(part);
        }
        self.hash.update(b"\n");
    }

    /// Binary content: its blobs stand for its lines.
    fn binary(&mut self, old: Option<ObjectId>, new: Option<ObjectId>) {
        (self.added, self.removed) = (0, 0);
        for id in [old, new].into_iter().flatten() {
            self.hash.update(id.as_bytes());
        }
    }
}

/// The sum of a commit's file hashes, so their order doesn't matter.
#[derive(Default)]
struct PatchId {
    sum: [u8; 32],
    files: usize,
}

impl PatchId {
    fn add(&mut self, hash: Sha256) {
        let digest = hash.finalize();
        let mut carry = 0u16;
        for (s, d) in self.sum.iter_mut().zip(digest.iter()) {
            carry += u16::from(*s) + u16::from(*d);
            *s = carry as u8;
            carry >>= 8;
        }
        self.files += 1;
    }

    fn hex(&self) -> Option<String> {
        (self.files > 0).then(|| self.sum[..20].iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// Every line instance, lineage and file version the walk made.
#[derive(Default)]
pub struct Arena {
    /// Per instance: its lineage, birth address and weave successor.
    pub lineage: Vec<u32>,
    pub born: Vec<i64>,
    pub next: Vec<u32>,
    /// The instance each replaced one-for-one, or [`NONE`].
    pub replaces: Vec<u32>,
    death: Vec<i64>,
    more_deaths: HashMap<u32, Vec<i64>>,
    /// Per lineage: its first instance in the weave.
    pub heads: Vec<u32>,
    pub lineages: Vec<Lineage>,
    pub versions: Vec<Version>,
    /// Per commit that changed files, by address.
    pub changes: Vec<(i64, CommitChanges)>,
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

    pub(crate) fn kill(&mut self, id: u32, at: i64) {
        let slot = &mut self.death[id as usize];
        if *slot == NO_DEATH {
            *slot = at;
        } else {
            self.more_deaths.entry(id).or_default().push(at);
        }
    }

    pub(crate) fn new_lineage(&mut self, path: &str, born: i64) -> u32 {
        let id = self.lineages.len() as u32;
        self.lineages.push(Lineage {
            path: path.to_owned(),
            born,
        });
        self.heads.push(NONE);
        id
    }

    /// A new instance in `lineage`'s weave, right after `after` (or first).
    pub(crate) fn new_instance(
        &mut self,
        lineage: u32,
        born: i64,
        after: u32,
        replaces: u32,
    ) -> Result<u32> {
        let id = u32::try_from(self.lineage.len())
            .ok()
            .filter(|&id| id != NONE)
            .context("too many line instances")?;
        self.lineage.push(lineage);
        self.born.push(born);
        self.replaces.push(replaces);
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
pub(crate) struct File {
    pub lineage: u32,
    pub version: u32,
    pub blob: ObjectId,
    /// Its line instances, in order; empty for binary content.
    pub lines: Arc<[u32]>,
    pub binary: bool,
}

pub(crate) type Files = HashMap<String, File>;

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

/// Where the walk records what each commit did: the arena of a bulk
/// build, or the tables a sync updates.
pub trait Ledger {
    fn new_lineage(&mut self, path: &str, born: i64) -> Result<u32>;
    /// A new line instance right after `after` in its lineage's weave, or
    /// first when `after` is [`NONE`], replacing `replaces` one-for-one
    /// (or [`NONE`]).
    fn new_instance(
        &mut self,
        lineage: u32,
        born: i64,
        after: u32,
        replaces: u32,
        text: &str,
    ) -> Result<u32>;
    fn kill(&mut self, id: u32, at: i64);
    fn new_version(&mut self, version: Version) -> Result<u32>;
    fn kill_version(&mut self, id: u32, at: i64);
    /// What a commit changed, once it is applied.
    fn changed(&mut self, at: i64, changes: CommitChanges) -> Result<()>;
}

/// A bulk build's ledger: the arena, with new lines' text sent to a sink.
pub struct ArenaLedger<S: LineSink> {
    pub arena: Arena,
    pub sink: S,
}

impl<S: LineSink> Ledger for ArenaLedger<S> {
    fn new_lineage(&mut self, path: &str, born: i64) -> Result<u32> {
        Ok(self.arena.new_lineage(path, born))
    }

    fn new_instance(
        &mut self,
        lineage: u32,
        born: i64,
        after: u32,
        replaces: u32,
        text: &str,
    ) -> Result<u32> {
        let id = self.arena.new_instance(lineage, born, after, replaces)?;
        self.sink.line(id, text)?;
        Ok(id)
    }

    fn kill(&mut self, id: u32, at: i64) {
        self.arena.kill(id, at);
    }

    fn new_version(&mut self, version: Version) -> Result<u32> {
        let id = u32::try_from(self.arena.versions.len()).context("too many file versions")?;
        self.arena.versions.push(version);
        Ok(id)
    }

    fn kill_version(&mut self, id: u32, at: i64) {
        self.arena.versions[id as usize].deaths.push(at);
    }

    fn changed(&mut self, at: i64, changes: CommitChanges) -> Result<()> {
        self.arena.changes.push((at, changes));
        Ok(())
    }
}

/// Applies commits to lanes' trees.
pub struct Walker<'r, L: Ledger> {
    repo: &'r gix::Repository,
    paths: &'r [String],
    pub ledger: L,
    pub stats: WalkStats,
    cache: gix::diff::blob::Platform,
    tree_state: gix::diff::tree::State,
    /// The current commit's changes.
    changed: Vec<FileChange>,
    patch: PatchId,
}

impl<'r, S: LineSink> Walker<'r, ArenaLedger<S>> {
    /// A bulk build's walker: `sink` gets each new instance's id and text.
    pub fn new(repo: &'r gix::Repository, paths: &'r [String], sink: S) -> Result<Self> {
        Self::with_ledger(
            repo,
            paths,
            ArenaLedger {
                arena: Arena::default(),
                sink,
            },
        )
    }
}

impl<'r, L: Ledger> Walker<'r, L> {
    pub fn with_ledger(repo: &'r gix::Repository, paths: &'r [String], ledger: L) -> Result<Self> {
        let cache = repo
            .diff_resource_cache_for_tree_diff()
            .map_err(|e| anyhow!("cannot prepare the diff cache: {e}"))?;
        Ok(Self {
            repo,
            paths,
            ledger,
            stats: WalkStats::default(),
            cache,
            tree_state: Default::default(),
            changed: Vec::new(),
            patch: PatchId::default(),
        })
    }

    /// The ledger and the stats, once the walk is done.
    pub fn finish(self) -> (L, WalkStats) {
        (self.ledger, self.stats)
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
                    commits_per_sec = (n + 1) as u64 / started.elapsed().as_secs().max(1),
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

    pub(crate) fn tree_changes(
        &mut self,
        parent: Option<ObjectId>,
        tree: ObjectId,
    ) -> Result<Vec<Change>> {
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
    pub(crate) fn apply(
        &mut self,
        files: &mut Files,
        parent: Option<ObjectId>,
        tree: ObjectId,
        at: i64,
    ) -> Result<()> {
        let changes = self.tree_changes(parent, tree)?;
        self.apply_changes(files, changes, at)
    }

    /// The paths a commit's changes touch, as sources or destinations.
    pub(crate) fn touched(changes: &[Change]) -> Vec<String> {
        let mut out = Vec::new();
        for change in changes {
            match change {
                Change::Addition { location, .. }
                | Change::Deletion { location, .. }
                | Change::Modification { location, .. } => out.push(location.to_string()),
                Change::Rewrite {
                    source_location,
                    location,
                    ..
                } => {
                    out.push(source_location.to_string());
                    out.push(location.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Applies `changes`, one commit's, to `files`.
    pub(crate) fn apply_changes(
        &mut self,
        files: &mut Files,
        changes: Vec<Change>,
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
        for change in changes {
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
                self.delete(file, &p, at);
            }
        }
        // Every source leaves before any destination arrives.
        let mut moving = Vec::new();
        for (src, dst, id) in renames {
            match files.remove(&src) {
                Some(file) if self.tracked(&dst) => moving.push((file, src, dst, id)),
                Some(file) => self.delete(file, &src, at),
                None if self.tracked(&dst) => additions.push((dst, id)),
                None => {}
            }
        }
        for (file, src, dst, id) in moving {
            self.stats.renames += 1;
            let file = self.change(file, Some(&src), &dst, id, at)?;
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
                    let file = self.change(file, None, &p, id, at)?;
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
                let file = self.change(file, None, &p, id, at)?;
                files.insert(p, file);
                continue;
            }
            let file = self.add(&p, id, at)?;
            files.insert(p, file);
        }
        let changes = CommitChanges {
            files: std::mem::take(&mut self.changed),
            patch_id: std::mem::take(&mut self.patch).hex(),
        };
        if !changes.files.is_empty() {
            self.ledger.changed(at, changes)?;
        }
        Ok(())
    }

    fn record(&mut self, change: FileChange, delta: Delta) {
        self.patch.add(delta.hash);
        self.changed.push(change);
    }

    fn delete(&mut self, file: File, path: &str, at: i64) {
        self.stats.files_deleted += 1;
        self.ledger.kill_version(file.version, at);
        for &id in file.lines.iter() {
            self.ledger.kill(id, at);
        }
        self.stats.lines_deleted += file.lines.len() as u64;
        // The blob stands for the lines it had.
        let mut delta = Delta::new(&["D", path]);
        delta.binary(Some(file.blob), None);
        let removed = if file.binary {
            0
        } else {
            file.lines.len() as u32
        };
        self.record(
            FileChange {
                path: path.to_owned(),
                old_path: None,
                status: 'D',
                added: 0,
                removed,
                binary: file.binary,
                version: None,
                old_version: Some(file.version),
            },
            delta,
        );
    }

    fn add(&mut self, path: &str, id: ObjectId, at: i64) -> Result<File> {
        self.stats.files_added += 1;
        let blob = self.read(id)?;
        let lineage = self.ledger.new_lineage(path, at)?;
        let mut delta = Delta::new(&["A", path]);
        let lines = self.insert(lineage, &blob, NONE, at, &mut delta)?;
        if !blob.text {
            delta.binary(None, Some(id));
        }
        let version = self.new_version(lineage, path, id, &blob, at, lines.len())?;
        self.record(
            FileChange {
                path: path.to_owned(),
                old_path: None,
                status: 'A',
                added: delta.added,
                removed: 0,
                binary: !blob.text,
                version: Some(version),
                old_version: None,
            },
            delta,
        );
        Ok(File {
            lineage,
            version,
            blob: id,
            lines: lines.into(),
            binary: !blob.text,
        })
    }

    /// `file`'s next version: at `path`, moved from `from` by a rename, with
    /// blob `id`.
    fn change(
        &mut self,
        file: File,
        from: Option<&str>,
        path: &str,
        id: ObjectId,
        at: i64,
    ) -> Result<File> {
        self.ledger.kill_version(file.version, at);
        let new = self.read(id)?;
        let mut delta = match from {
            Some(src) => Delta::new(&["R", src, path]),
            None => Delta::new(&["M", path]),
        };
        let lines: Arc<[u32]> = if file.blob == id {
            file.lines.clone()
        } else {
            let old = self.read(file.blob)?;
            self.diff(&file, &old, &new, at, &mut delta)?.into()
        };
        let binary = file.binary || !new.text;
        if binary && file.blob != id {
            delta.binary(Some(file.blob), Some(id));
        }
        let version = self.new_version(file.lineage, path, id, &new, at, lines.len())?;
        self.record(
            FileChange {
                path: path.to_owned(),
                old_path: from.map(str::to_owned),
                status: if from.is_some() { 'R' } else { 'M' },
                added: delta.added,
                removed: delta.removed,
                binary,
                version: Some(version),
                old_version: Some(file.version),
            },
            delta,
        );
        Ok(File {
            lineage: file.lineage,
            version,
            blob: id,
            lines,
            binary: !new.text,
        })
    }

    /// Every line of `blob` as new instances after `after`.
    fn insert(
        &mut self,
        lineage: u32,
        blob: &Blob,
        mut after: u32,
        at: i64,
        delta: &mut Delta,
    ) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        if !blob.text {
            return Ok(out);
        }
        for token in byte_lines(blob.body()) {
            delta.line(b'+', token);
            after = self.born(lineage, after, NONE, at, line_text(token))?;
            out.push(after);
        }
        Ok(out)
    }

    fn born(
        &mut self,
        lineage: u32,
        after: u32,
        replaces: u32,
        at: i64,
        text: &str,
    ) -> Result<u32> {
        let id = self
            .ledger
            .new_instance(lineage, at, after, replaces, text)?;
        self.stats.lines_added += 1;
        self.stats.line_bytes += text.len() as u64;
        Ok(id)
    }

    /// The new version's instances: the old ones it keeps and new ones for
    /// the lines Myers's diff (with git's indent heuristic) adds.
    fn diff(
        &mut self,
        file: &File,
        old: &Blob,
        new: &Blob,
        at: i64,
        delta: &mut Delta,
    ) -> Result<Vec<u32>> {
        if !new.text {
            for &id in file.lines.iter() {
                self.ledger.kill(id, at);
            }
            self.stats.lines_deleted += file.lines.len() as u64;
            return Ok(Vec::new());
        }
        if !old.text {
            return self.insert(file.lineage, new, NONE, at, delta);
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
        // Lines exclude a BOM, but git sees a first line that gained or lost
        // one as changed: count it when the diff kept that line.
        let first_kept = diff.hunks().next().is_none_or(|h| {
            let side = if old.bom { &h.before } else { &h.after };
            side.start > 0 || side.is_empty()
        });
        let lines = if old.bom { &input.before } else { &input.after };
        if old.bom != new.bom && first_kept && !lines.is_empty() {
            delta.line(b'-', b"\xef\xbb\xbf");
            delta.line(b'+', b"");
        }
        let old_ids = &file.lines;
        let mut out = Vec::with_capacity(input.after.len());
        let mut pos = 0usize;
        for hunk in diff.hunks() {
            let (b0, b1) = (hunk.before.start as usize, hunk.before.end as usize);
            out.extend_from_slice(&old_ids[pos..b0]);
            for &id in &old_ids[b0..b1] {
                self.ledger.kill(id, at);
            }
            for &token in &input.before[b0..b1] {
                delta.line(b'-', input.interner[token]);
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
            // As many lines added as removed: each replaces its counterpart.
            let one_for_one = hunk.after.len() == b1 - b0;
            for (i, j) in hunk.after.clone().enumerate() {
                let token = input.interner[input.after[j as usize]];
                delta.line(b'+', token);
                let replaces = if one_for_one { old_ids[b0 + i] } else { NONE };
                after = self.born(file.lineage, after, replaces, at, line_text(token))?;
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
        self.ledger.new_version(Version {
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
        })
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
