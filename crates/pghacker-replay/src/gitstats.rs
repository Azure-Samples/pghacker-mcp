//! A commit's statistics, as the study's data records them, to find which
//! commit the study replayed: every tracked file counted; symlinks; blobs
//! with a NUL byte are binary; blobs that are not UTF-8 are skipped; text
//! lines split on `\n` with a final empty element dropped (an empty file
//! counts one line).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Per-repository statistics, as data/trgm/load.json records them.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StudyStats {
    pub files: u64,
    pub text: u64,
    pub binary: u64,
    pub non_utf8: u64,
    pub symlinks: u64,
    pub lines: u64,
    pub bytes: u64,
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("cannot run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// (mode, type, sha, path) of every tree entry.
fn ls_tree(dir: &Path, rev: &str) -> Result<Vec<(String, String, String, String)>> {
    let out = git(dir, &["ls-tree", "-r", "-z", rev])?;
    let mut entries = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split(' ');
        let (mode, kind, sha) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        entries.push((mode.into(), kind.into(), sha.into(), path.into()));
    }
    Ok(entries)
}

/// The number of tracked files (not submodules) at `rev`.
pub fn file_count(dir: &Path, rev: &str) -> Result<u64> {
    Ok(ls_tree(dir, rev)?
        .iter()
        .filter(|e| e.1 != "commit")
        .count() as u64)
}

/// The statistics of `rev`, counted as the study's data counts them.
pub fn study_stats(dir: &Path, rev: &str) -> Result<StudyStats> {
    let entries = ls_tree(dir, rev)?;
    let mut st = StudyStats::default();
    let mut blobs = Vec::new();
    for (mode, kind, sha, _) in &entries {
        if kind == "commit" {
            continue;
        }
        st.files += 1;
        if mode == "120000" {
            st.symlinks += 1;
        } else if kind == "blob" {
            blobs.push(sha.clone());
        }
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot run git cat-file")?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for sha in &blobs {
            writeln!(stdin, "{sha}")?;
        }
        Ok(())
    });
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("unexpected cat-file header {header:?}"))?;
        let mut data = vec![0; size + 1];
        reader.read_exact(&mut data)?;
        data.pop();
        if data.contains(&0) {
            st.binary += 1;
            continue;
        }
        let Ok(text) = std::str::from_utf8(&data) else {
            st.non_utf8 += 1;
            continue;
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        st.text += 1;
        st.bytes += data.len() as u64;
        let mut n = text.split('\n').count() as u64;
        if n > 1 && text.ends_with('\n') {
            n -= 1;
        }
        st.lines += n;
    }
    writer.join().expect("the writer thread panicked")?;
    child.wait()?;
    Ok(st)
}

/// The commit of `dir` whose statistics equal `want`: HEAD first, then the
/// first-parent commits of the remotes' main branches nearest `around`.
pub fn find_commit(
    dir: &Path,
    want: &StudyStats,
    around: chrono::DateTime<chrono::Utc>,
) -> Result<String> {
    let mut candidates: Vec<(i64, String)> = Vec::new();
    let head = String::from_utf8(git(dir, &["rev-parse", "HEAD"])?)?
        .trim()
        .to_owned();
    candidates.push((i64::MIN, head));
    let refs = String::from_utf8(git(
        dir,
        &["for-each-ref", "--format=%(refname)", "refs/remotes"],
    )?)?;
    let since = (around - chrono::Duration::days(45)).to_rfc3339();
    let until = (around + chrono::Duration::days(2)).to_rfc3339();
    for r in refs
        .lines()
        .filter(|r| r.ends_with("/main") || r.ends_with("/master"))
    {
        let log = git(
            dir,
            &[
                "log",
                "--first-parent",
                "--format=%H %ct",
                &format!("--since={since}"),
                &format!("--until={until}"),
                r,
            ],
        )?;
        for line in String::from_utf8_lossy(&log).lines() {
            if let Some((sha, ts)) = line.split_once(' ') {
                let distance = (ts.parse::<i64>().unwrap_or(0) - around.timestamp()).abs();
                candidates.push((distance, sha.to_owned()));
            }
        }
    }
    candidates.sort();
    let mut seen = std::collections::HashSet::new();
    for (_, sha) in candidates {
        if !seen.insert(sha.clone()) {
            continue;
        }
        if file_count(dir, &sha)? != want.files {
            continue;
        }
        if study_stats(dir, &sha)? == *want {
            return Ok(sha);
        }
    }
    bail!("no commit of {} has the study's statistics", dir.display())
}
