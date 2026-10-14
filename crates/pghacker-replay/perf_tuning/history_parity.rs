//! Checks pghacker_bench's history build (see `pghacker-replay
//! bench-prepare`) against git, on random commits and files: get_commit's
//! diff against git show, file by file; its changed files against git show
//! --numstat; and blame against git blame, plain and with the head's
//! .git-blame-ignore-revs, for random files at the production heads.
//! Changes the history leaves out by design (symlinks, submodules, modes
//! alone) are counted apart. Prints agreement and the first differences.
//!
//! usage: history_parity GIT_DIR [COMMITS] [FILES] [SEED]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use pghacker_codesearch::{BlameArgs, GetCommitArgs, blame, get_commit};
use pghacker_core::config::{Env, with_database};
use pghacker_core::db;

const SCHEMA: &str = "pghacker_history";
const HEADS: [&str; 15] = [
    "master",
    "REL_19_STABLE",
    "REL_18_STABLE",
    "REL_17_STABLE",
    "REL_16_STABLE",
    "REL_15_STABLE",
    "REL_14_STABLE",
    "REL_13_STABLE",
    "REL_12_STABLE",
    "REL_11_STABLE",
    "REL_10_STABLE",
    "REL9_6_STABLE",
    "REL9_2_STABLE",
    "REL8_4_STABLE",
    "REL7_4_STABLE",
];

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", "/nonexistent")
        .output()
        .context("git runs")?;
    if !out.status.success() {
        bail!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

/// A diff's per-file blocks without what the history doesn't keep (modes,
/// blob ids, similarity), by header line.
fn blocks(diff: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            current = Some(line.to_owned());
            out.entry(line.to_owned()).or_default();
            continue;
        }
        let skip = [
            "index ",
            "new file",
            "deleted file",
            "similarity index",
            "old mode",
            "new mode",
        ];
        if skip.iter().any(|p| line.starts_with(p)) {
            continue;
        }
        if let Some(c) = &current {
            out.get_mut(c).unwrap().push(line.to_owned());
        }
    }
    out
}

/// Why git shows a file the history leaves out, if it does.
fn left_out(git_block: &[String], raw_block: &str) -> Option<&'static str> {
    if raw_block.contains("120000") {
        Some("symlink")
    } else if raw_block.contains("160000")
        || git_block.iter().any(|l| l.contains("Subproject commit"))
    {
        Some("submodule")
    } else if !git_block
        .iter()
        .any(|l| l.starts_with("@@") || l.starts_with("Binary") || l.starts_with("rename "))
    {
        Some("mode or empty")
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let git_dir = Path::new(
        args.first()
            .context("usage: history_parity GIT_DIR [COMMITS] [FILES] [SEED]")?,
    );
    let n_commits: i64 = args.get(1).map_or(Ok(200), |a| a.parse())?;
    let n_files: usize = args.get(2).map_or(Ok(40), |a| a.parse())?;
    let seed: &str = args.get(3).map_or("1", String::as_str);
    let env = Env::local()?;
    let url = with_database(&env.reader_database_url()?, "pghacker_bench")?;
    let client = db::connect(&url, "history-parity").await?;

    // get_commit and numstat against git show.
    let shas: Vec<String> = client
        .query(
            &format!("SELECT sha FROM {SCHEMA}.code_commits ORDER BY md5(sha || $1) LIMIT $2"),
            &[&seed, &n_commits],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let (mut same, mut differ, mut ours_only) = (0usize, 0usize, 0usize);
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut examples: Vec<String> = Vec::new();
    let (mut stat_same, mut stat_differ) = (0usize, 0usize);
    for sha in &shas {
        let ours = get_commit(
            &client,
            SCHEMA,
            "postgres",
            &GetCommitArgs {
                commit: sha.clone(),
                ..Default::default()
            },
        )
        .await?;
        let shown = String::from_utf8_lossy(&git(git_dir, &["show", "--format=", "-M50%", sha])?)
            .into_owned();
        let ours_blocks = blocks(&ours.diff);
        let git_blocks = blocks(&shown);
        // The raw text of git's blocks, for modes.
        let mut raw: BTreeMap<String, String> = BTreeMap::new();
        let mut cur = String::new();
        for line in shown.lines() {
            if line.starts_with("diff --git ") {
                cur = line.to_owned();
            }
            raw.entry(cur.clone())
                .or_default()
                .push_str(&format!("{line}\n"));
        }
        let last = ours_blocks.keys().last().cloned();
        for (header, theirs) in &git_blocks {
            match ours_blocks.get(header) {
                Some(mine) if mine == theirs => same += 1,
                // The last block of a cut diff may stop early.
                Some(_) if ours.cut && Some(header) == last.as_ref() => {}
                Some(mine) => {
                    differ += 1;
                    if examples.len() < 6 {
                        let at = mine
                            .iter()
                            .zip(theirs)
                            .position(|(a, b)| a != b)
                            .unwrap_or(mine.len().min(theirs.len()));
                        examples.push(format!(
                            "{sha} {header}: line {at}: ours {:?} git {:?}",
                            mine.get(at),
                            theirs.get(at)
                        ));
                    }
                }
                None if ours.cut => {}
                None => match left_out(theirs, raw.get(header).map_or("", String::as_str)) {
                    Some(why) => *skipped.entry(why).or_default() += 1,
                    None => {
                        differ += 1;
                        if examples.len() < 6 {
                            examples.push(format!("{sha} {header}: missing from ours"));
                        }
                    }
                },
            }
        }
        ours_only += ours_blocks
            .keys()
            .filter(|h| !git_blocks.contains_key(*h))
            .count();

        // Line counts: git show --numstat, binary as "- -".
        let numstat = String::from_utf8_lossy(&git(
            git_dir,
            &["show", "--format=", "-M50%", "--numstat", sha],
        )?)
        .into_owned();
        let theirs: BTreeSet<String> = numstat
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let parts: Vec<&str> = l.splitn(3, '\t').collect();
                format!("{} {} {}", parts[2], parts[0], parts[1])
            })
            .collect();
        for f in &ours.files {
            let path = match &f.old_path {
                Some(old) => rename_display(old, &f.path),
                None => f.path.clone(),
            };
            let counts = if f.binary {
                "- -".to_owned()
            } else {
                format!("{} {}", f.added, f.removed)
            };
            if theirs.contains(&format!("{path} {counts}")) {
                stat_same += 1;
            } else {
                stat_differ += 1;
                if examples.len() < 12 {
                    examples.push(format!(
                        "{sha} numstat {path} {counts}: git has {:?}",
                        theirs.iter().find(|t| t.starts_with(&path))
                    ));
                }
            }
        }
    }
    println!(
        "get_commit: {} commits, {same} files alike, {differ} differ, {ours_only} only ours, left out by design {skipped:?}",
        shas.len()
    );
    println!("numstat: {stat_same} files alike, {stat_differ} differ");

    // blame against git blame.
    let mut lines = [0usize; 2];
    let mut agree = [0usize; 2];
    for k in 0..n_files {
        let head = HEADS[k % HEADS.len()];
        let snapshot =
            pghacker_codesearch::rev::resolve(&client, SCHEMA, "postgres", Some(head)).await?;
        let addr = snapshot.history.as_ref().context("no history")?.addr;
        let path: String = client
            .query_one(
                &format!(
                    "SELECT v.path FROM {SCHEMA}.code_file_versions v \
                     WHERE v.lo <= $1 AND v.hi > $1 AND (v.alive IS NULL OR v.alive @> $1) \
                       AND NOT v.is_binary AND v.line_count > 0 \
                     ORDER BY md5(v.path || $2) LIMIT 1"
                ),
                &[&addr, &format!("{seed}{k}")],
            )
            .await?
            .get(0);
        let ignore_list = git(
            git_dir,
            &["show", &format!("{}:.git-blame-ignore-revs", snapshot.sha)],
        )
        .ok();
        for (mode, ignore) in [(0usize, false), (1, true)] {
            if ignore && ignore_list.is_none() {
                continue;
            }
            let ours = blame(
                &client,
                &snapshot,
                &BlameArgs {
                    path: path.clone(),
                    start_line: Some(1),
                    end_line: Some(2000),
                    ignore_revs: Some(ignore),
                    ..Default::default()
                },
            )
            .await?;
            let end = ours.end_line.to_string();
            let range = format!("1,{end}");
            let mut git_args = vec!["blame", "--porcelain", "-L", range.as_str()];
            let list_file = std::env::temp_dir().join("history-parity-ignore-revs");
            if ignore {
                std::fs::write(&list_file, ignore_list.as_ref().unwrap())?;
                git_args.push("--ignore-revs-file");
                git_args.push(list_file.to_str().unwrap());
            }
            git_args.push(&snapshot.sha);
            git_args.push("--");
            git_args.push(&path);
            let porcelain = String::from_utf8_lossy(&git(git_dir, &git_args)?).into_owned();
            let theirs: Vec<&str> = porcelain
                .lines()
                .filter(|l| {
                    l.len() > 41
                        && l.as_bytes()[40] == b' '
                        && l[..40].bytes().all(|b| b.is_ascii_hexdigit())
                })
                .map(|l| &l[..12])
                .collect();
            let mut mismatched = 0;
            for (l, t) in ours.lines.iter().zip(&theirs) {
                lines[mode] += 1;
                if l.commit == *t {
                    agree[mode] += 1;
                } else {
                    mismatched += 1;
                }
            }
            if ours.lines.len() != theirs.len() {
                println!(
                    "blame {head} {path}: {} lines vs git's {}",
                    ours.lines.len(),
                    theirs.len()
                );
            }
            if mismatched > 0 {
                println!(
                    "blame{} {head} {path}: {mismatched} of {} lines differ",
                    if ignore { " (ignore-revs)" } else { "" },
                    ours.lines.len()
                );
            }
            if mismatched > 0 && examples.len() < 20 {
                examples.push(format!(
                    "blame{} {head} {path}: {mismatched} of {} lines differ",
                    if ignore { " (ignore-revs)" } else { "" },
                    ours.lines.len()
                ));
            }
        }
    }
    for (mode, name) in ["blame", "blame with ignore-revs"].iter().enumerate() {
        println!(
            "{name}: {} of {} lines agree ({:.2}%)",
            agree[mode],
            lines[mode],
            100.0 * agree[mode] as f64 / lines[mode].max(1) as f64
        );
    }
    for e in examples {
        println!("  {e}");
    }
    Ok(())
}

/// How git --numstat names a rename: `a/{b => c}/d` with the common parts
/// outside braces.
fn rename_display(old: &str, new: &str) -> String {
    let (o, n) = (old.as_bytes(), new.as_bytes());
    let mut pre = o.iter().zip(n).take_while(|(a, b)| a == b).count();
    while pre > 0 && o[pre - 1] != b'/' {
        pre -= 1;
    }
    let mut suf = o
        .iter()
        .rev()
        .zip(n.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    suf = suf.min(o.len() - pre).min(n.len() - pre);
    while suf > 0 && o[o.len() - suf] != b'/' {
        suf -= 1;
    }
    if pre == 0 && suf == 0 {
        return format!("{old} => {new}");
    }
    format!(
        "{}{{{} => {}}}{}",
        &old[..pre],
        &old[pre..old.len() - suf],
        &new[pre..new.len() - suf],
        &old[old.len() - suf..]
    )
}
