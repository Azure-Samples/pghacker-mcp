// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Ground truth from ripgrep, computed as the study computed it: the files a
//! search covers, and ripgrep's matching lines in them.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use regex::Regex;

/// Files below the repo-relative paths (`""` is the root), as
/// `rg --files --hidden --no-ignore -g '!.git'` lists them with the globs
/// and types; None when ripgrep fails.
pub fn rg_files(
    rg: &Path,
    dir: &Path,
    rel: &[String],
    globs: &[String],
    types: &[String],
) -> Result<Option<Vec<String>>> {
    let mut args: Vec<String> = ["--files", "--hidden", "--no-ignore", "-g", "!.git"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    for g in globs {
        args.push("-g".into());
        args.push(g.clone());
    }
    for t in types {
        args.push("-t".into());
        args.push(t.clone());
    }
    args.push("--".into());
    let named: Vec<&String> = rel.iter().filter(|r| !r.is_empty()).collect();
    if named.is_empty() {
        args.push(".".into());
    } else {
        args.extend(named.into_iter().cloned());
    }
    let out = Command::new(rg)
        .args(&args)
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
        .context("cannot run ripgrep")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_prefix("./").unwrap_or(l).to_owned())
        .collect();
    if out.status.success() || !files.is_empty() {
        return Ok(Some(files));
    }
    Ok((out.status.code() == Some(1)).then(Vec::new))
}

/// A shell wildcard on a repo-relative path, as a regex that also accepts
/// paths below a matching folder (scope.ts wildcardRe).
pub fn wildcard_re(g: &str) -> Regex {
    let c: Vec<char> = g.chars().collect();
    let mut re = String::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '*' {
            while c.get(i + 1) == Some(&'*') {
                i += 1;
            }
            re.push_str("[^/]*");
            i += 1;
            continue;
        }
        if ch == '?' {
            re.push_str("[^/]");
            i += 1;
            continue;
        }
        if ch == '['
            && let Some(e) = (i + 2..c.len()).find(|&j| c[j] == ']')
        {
            let body: String = c[i + 1..e].iter().collect();
            let body = match body.strip_prefix('!') {
                Some(rest) => format!("^{rest}"),
                None => body,
            };
            re.push('[');
            re.push_str(&body.replace('\\', "\\\\"));
            re.push(']');
            i = e + 1;
            continue;
        }
        if ch == '{'
            && let Some(e) = (i..c.len()).find(|&j| c[j] == '}')
        {
            let body: String = c[i + 1..e].iter().collect();
            if body.contains(',') {
                let alts: Vec<String> = body
                    .split(',')
                    .map(|x| {
                        regex::escape(x)
                            .replace("\\*", "[^/]*")
                            .replace("\\?", "[^/]")
                    })
                    .collect();
                re.push_str(&format!("(?:{})", alts.join("|")));
                i = e + 1;
                continue;
            }
        }
        if ch == '\\' && i + 1 < c.len() {
            re.push_str(&regex::escape(&c[i + 1].to_string()));
            i += 2;
            continue;
        }
        re.push_str(&regex::escape(&ch.to_string()));
        i += 1;
    }
    Regex::new(&format!("^{re}(?:/.*)?$")).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

/// The files a search covers (scope.ts scopeOf).
pub struct Scope {
    pub files: Vec<String>,
    /// ripgrep's own paths: the named ones, or none (the whole repository)
    /// when wildcards are involved.
    pub rg_paths: Vec<String>,
}

pub fn scope_of(
    rg: &Path,
    dir: &Path,
    tracked: &HashSet<String>,
    rel: &[String],
    rel_globs: &[String],
    globs: &[String],
    types: &[String],
) -> Result<Option<Scope>> {
    let named: Vec<String> = if !rel.is_empty() || !rel_globs.is_empty() {
        rel.to_vec()
    } else {
        vec![String::new()]
    };
    let mut files: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    if !named.is_empty() {
        let Some(base) = rg_files(rg, dir, &named, globs, types)? else {
            return Ok(None);
        };
        for f in base {
            if seen.insert(f.clone()) {
                files.push(f);
            }
        }
    }
    if !rel_globs.is_empty() {
        let Some(all) = rg_files(rg, dir, &[String::new()], globs, types)? else {
            return Ok(None);
        };
        let res: Vec<Regex> = rel_globs.iter().map(|g| wildcard_re(g)).collect();
        for f in all {
            if res.iter().any(|r| r.is_match(&f)) && seen.insert(f.clone()) {
                files.push(f);
            }
        }
    }
    Ok(Some(Scope {
        files: files.into_iter().filter(|f| tracked.contains(f)).collect(),
        rg_paths: if rel_globs.is_empty() {
            named.into_iter().filter(|r| !r.is_empty()).collect()
        } else {
            Vec::new()
        },
    }))
}

/// ripgrep's options for a search (replay.ts RgOpts).
#[derive(Clone, Debug, Default)]
pub struct RgOpts {
    pub fixed: bool,
    pub ignore_case: bool,
    pub word: bool,
    pub pcre2: bool,
    pub invert: bool,
    pub paths: Vec<String>,
    pub globs: Vec<String>,
    pub types: Vec<String>,
}

/// ripgrep's matching lines.
pub enum RgLines {
    Lines(HashMap<String, Vec<u32>>),
    Error,
    TooLarge,
}

/// ripgrep's matching lines as path → line numbers (replay.ts rgLines).
pub fn rg_lines(
    rg: &Path,
    dir: &Path,
    patterns: &[String],
    o: &RgOpts,
    cap: usize,
) -> Result<RgLines> {
    let mut args: Vec<String> = [
        "--hidden",
        "--no-ignore",
        "-g",
        "!.git",
        "-n",
        "--no-heading",
        "--with-filename",
        "--null",
        "--color",
        "never",
        "--max-columns",
        "1",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    for (on, flag) in [
        (o.fixed, "-F"),
        (o.ignore_case, "-i"),
        (o.word, "-w"),
        (o.pcre2, "-P"),
        (o.invert, "-v"),
    ] {
        if on {
            args.push(flag.into());
        }
    }
    for g in &o.globs {
        args.push("-g".into());
        args.push(g.clone());
    }
    for t in &o.types {
        args.push("-t".into());
        args.push(t.clone());
    }
    for p in patterns {
        args.push("-e".into());
        args.push(p.clone());
    }
    args.push("--".into());
    if o.paths.is_empty() {
        args.push(".".into());
    } else {
        args.extend(o.paths.iter().cloned());
    }
    let mut child = Command::new(rg)
        .args(&args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot run ripgrep")?;
    let reader = BufReader::new(child.stdout.take().unwrap());
    let mut out: HashMap<String, Vec<u32>> = HashMap::new();
    let mut n = 0usize;
    for line in reader.split(b'\n') {
        let line = line?;
        let Some(z) = line.iter().position(|b| *b == 0) else {
            continue;
        };
        let file = String::from_utf8_lossy(&line[..z]);
        let file = file.strip_prefix("./").unwrap_or(&file).to_owned();
        let rest = &line[z + 1..];
        let colon = rest.iter().position(|b| *b == b':').unwrap_or(rest.len());
        let num: u32 = String::from_utf8_lossy(&rest[..colon]).parse().unwrap_or(0);
        out.entry(file).or_default().push(num);
        n += 1;
        if n > cap {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(RgLines::TooLarge);
        }
    }
    let status = child.wait()?;
    if status.code() == Some(2) && n == 0 {
        return Ok(RgLines::Error);
    }
    Ok(RgLines::Lines(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_cover_folders_below() {
        let re = wildcard_re("src/*.rs");
        assert!(re.is_match("src/a.rs") && !re.is_match("src/a/b.rs"));
        let re = wildcard_re("a/**");
        assert!(re.is_match("a/b") && re.is_match("a/b/c.rs"));
        let re = wildcard_re("x/{a,b*}.c");
        assert!(re.is_match("x/a.c") && re.is_match("x/bz.c") && !re.is_match("x/c.c"));
        let re = wildcard_re("[!a]?.txt");
        assert!(re.is_match("bb.txt") && !re.is_match("ab.txt"));
    }
}
