// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! GNU grep 3.11 in Docker (debian:trixie-slim, LC_ALL=C.UTF-8) on
//! materialized trees: the ground truth for the grep tool's output.
//!
//! The trees are copied into a tmpfs, each folder's entries created in
//! reverse byte order: tmpfs lists the newest first, so `grep -r` walks
//! names in byte order, the order the grep tool uses. Commands run in bash,
//! as agents' commands do.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const IMAGE: &str = "debian:trixie-slim";

/// A command to run in a tree, from the tree's root.
#[derive(Clone, Debug)]
pub struct Job {
    pub tree: String,
    pub cwd: String,
    pub command: String,
}

/// What GNU grep printed.
#[derive(Clone, Debug, Default)]
pub struct Ran {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub status: i32,
}

const RUNNER: &str = r#"#!/bin/sh
# Copies each /src/<tree> into /t/<tree> so that readdir lists names in byte
# order, then runs the jobs listed in /work/jobs.tsv (tree, folder; the
# command in /work/cmd/N.sh) in shards, writing N.out, N.err and N.rc.
copy() {
  ls -A "$1" | LC_ALL=C sort -r | while IFS= read -r n; do
    if [ -d "$1/$n" ]; then mkdir "$2/$n"; copy "$1/$n" "$2/$n"; else cp "$1/$n" "$2/$n"; fi
  done
}
for d in /src/*; do
  name=$(basename "$d")
  mkdir -p "/t/$name"
  copy "$d" "/t/$name"
done
if [ -f /work/prelude.sh ]; then sh /work/prelude.sh; fi
shards=$1
k=0
while [ "$k" -lt "$shards" ]; do
  awk -F '\t' -v k="$k" -v n="$shards" '(NR - 1) % n == k { print NR "\t" $0 }' /work/jobs.tsv |
  while IFS="$(printf '\t')" read -r i tree cwd; do
    ( cd "/t/$tree/$cwd" && timeout 120 bash "/work/cmd/$i.sh" ) < /dev/null > "/work/out/$i.out" 2> "/work/out/$i.err"
    echo $? > "/work/out/$i.rc"
  done &
  k=$((k + 1))
done
wait
"#;

/// Runs the jobs; `trees` maps tree names to host folders, and `prelude`
/// runs (as root) after the trees are copied, before the jobs.
pub fn run(
    jobs: &[Job],
    trees: &[(String, PathBuf)],
    work: &Path,
    shards: usize,
    prelude: &str,
) -> Result<Vec<Ran>> {
    if work.exists() {
        std::fs::remove_dir_all(work)?;
    }
    std::fs::create_dir_all(work.join("out"))?;
    std::fs::create_dir_all(work.join("cmd"))?;
    let mut tsv = String::new();
    for (k, job) in jobs.iter().enumerate() {
        if job.cwd.contains(['\n', '\t']) || job.tree.contains(['\n', '\t']) {
            bail!("a GNU job's folder has a tab or newline: {:?}", job.cwd);
        }
        let cwd = if job.cwd.is_empty() { "." } else { &job.cwd };
        tsv.push_str(&format!("{}\t{}\n", job.tree, cwd));
        std::fs::write(work.join("cmd").join(format!("{}.sh", k + 1)), &job.command)?;
    }
    std::fs::write(work.join("jobs.tsv"), tsv)?;
    std::fs::write(work.join("run.sh"), RUNNER)?;
    if !prelude.is_empty() {
        std::fs::write(work.join("prelude.sh"), prelude)?;
    }
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--tmpfs", "/t:size=3g,exec"])
        .args(["-e", "LC_ALL=C.UTF-8"])
        .arg("-v")
        .arg(format!("{}:/work", work.display()));
    for (name, dir) in trees {
        cmd.arg("-v")
            .arg(format!("{}:/src/{name}:ro", dir.display()));
    }
    cmd.args([IMAGE, "sh", "/work/run.sh", &shards.to_string()]);
    let status = cmd.status().context("cannot run docker")?;
    if !status.success() {
        bail!("the GNU grep container failed: {status}");
    }
    let mut out = Vec::with_capacity(jobs.len());
    for i in 1..=jobs.len() {
        let dir = work.join("out");
        out.push(Ran {
            stdout: std::fs::read(dir.join(format!("{i}.out"))).unwrap_or_default(),
            stderr: std::fs::read_to_string(dir.join(format!("{i}.err"))).unwrap_or_default(),
            status: std::fs::read_to_string(dir.join(format!("{i}.rc")))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(-1),
        });
    }
    Ok(out)
}
