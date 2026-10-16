// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Reports how patches parse into series versions and parts, over an export
//! of patches as JSON lines, each with `mid` (the message), `tid` (the
//! thread), `sent` (RFC 3339), `subj`, `name`, and the stored `sv` and `part`.
//! It shows how many each rule gives, samples of each, what changes against
//! the stored values, and the names left without a version that look like
//! they have one.
//!
//! ```sh
//! cargo run -p pghacker-mail --example patch_series -- patches.jsonl [samples per group]
//! ```

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;

use chrono::{DateTime, Utc};
use pghacker_mail::series::{ThreadPatch, name_series, thread_series};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Row {
    mid: i64,
    tid: i64,
    sent: Option<String>,
    subj: String,
    name: String,
    sv: Option<Value>,
    part: Option<i32>,
}

/// Counts and a spread of samples, by group.
#[derive(Default)]
struct Groups(BTreeMap<String, (usize, Vec<(u64, String)>)>);

impl Groups {
    fn add(&mut self, group: &str, sample: String, samples: usize) {
        let entry = self.0.entry(group.to_owned()).or_default();
        entry.0 += 1;
        // Spread: keep the samples with the smallest hashes.
        let hash = fnv(&sample);
        if entry.1.len() < samples || entry.1.last().is_some_and(|(h, _)| hash < *h) {
            let at = entry.1.partition_point(|(h, _)| *h < hash);
            entry.1.insert(at, (hash, sample));
            entry.1.truncate(samples);
        }
    }

    fn print(&self, title: &str) {
        println!("\n## {title}\n");
        for (group, (count, samples)) in &self.0 {
            println!("{group}: {count}");
            for (_, sample) in samples {
                println!("    {sample}");
            }
        }
    }
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("give the export's path"))?;
    let samples: usize = args.next().map_or(Ok(8), |n| n.parse())?;
    let mut rows: Vec<Row> = Vec::new();
    for line in std::io::BufReader::new(std::fs::File::open(&path)?).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            rows.push(serde_json::from_str(&line)?);
        }
    }
    let sent: Vec<Option<DateTime<Utc>>> = rows
        .iter()
        .map(|r| {
            r.sent
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
        })
        .collect();
    let mut threads: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        threads.entry(row.tid).or_default().push(i);
    }
    let mut series = vec![Default::default(); rows.len()];
    for indices in threads.values() {
        let patches: Vec<ThreadPatch<'_>> = indices
            .iter()
            .map(|&i| ThreadPatch {
                message: rows[i].mid,
                sent_at: sent[i],
                subject: &rows[i].subj,
                filename: &rows[i].name,
            })
            .collect();
        for (s, &i) in thread_series(&patches).into_iter().zip(indices) {
            series[i] = s;
        }
    }

    let looks = [
        ("v-number", Regex::new(r"(?i)v[0-9]").unwrap()),
        (
            "ends in a number",
            Regex::new(r"[0-9](?:\.[a-z_]+)*$").unwrap(),
        ),
    ];
    let (mut by_rule, mut versions, mut parts, mut missing) = (
        Groups::default(),
        Groups::default(),
        Groups::default(),
        Groups::default(),
    );
    let mut totals: HashMap<&str, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let name = name_series(&row.name, sent[i]);
        let new = &series[i];
        let rule = if name.version.is_some() {
            name.rule
        } else if new.version.is_some() && name.sequence.is_some() {
            "sequence"
        } else if new.version.is_some() {
            "subject"
        } else {
            "none"
        };
        let inline = row.name.starts_with("inline-");
        *totals.entry("patches").or_default() += 1;
        *totals
            .entry(if inline { "inline" } else { "attachments" })
            .or_default() += 1;
        if new.version.is_some() {
            *totals.entry("with a version").or_default() += 1;
        }
        if new.part.is_some() {
            *totals.entry("with a part").or_default() += 1;
        }
        let shown = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
        let sample = format!(
            "{} => {} / {}{}",
            row.name,
            shown(&new.version),
            new.part.map_or("-".into(), |p| p.to_string()),
            if rule == "subject" {
                format!("   [{}]", row.subj)
            } else {
                String::new()
            }
        );
        if rule != "none" {
            by_rule.add(rule, sample.clone(), samples);
        }
        let old: Option<String> = row.sv.as_ref().map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let change = match (&old, &new.version) {
            (a, b) if a == b => None,
            (None, Some(_)) => Some("set"),
            (Some(_), None) => Some("cleared"),
            _ => Some("changed"),
        };
        if let Some(change) = change {
            versions.add(
                &format!("{change} by {rule}"),
                format!("{sample}   (was {})", shown(&old)),
                samples,
            );
        }
        let part_change = match (row.part, new.part) {
            (a, b) if a == b => None,
            (None, Some(_)) => Some("set"),
            (Some(_), None) => Some("cleared"),
            _ => Some("changed"),
        };
        if let Some(change) = part_change {
            parts.add(
                change,
                format!(
                    "{sample}   (was {})",
                    row.part.map_or("-".into(), |p| p.to_string())
                ),
                samples,
            );
        }
        if new.version.is_none()
            && !inline
            && let Some((label, _)) = looks.iter().find(|(_, re)| re.is_match(&row.name))
        {
            missing.add(label, row.name.clone(), samples);
        }
    }
    println!("# Patch series over {path}\n");
    let mut totals: Vec<_> = totals.into_iter().collect();
    totals.sort();
    for (name, count) in totals {
        println!("- {name}: {count}");
    }
    by_rule.print("Versions by rule");
    versions.print("Version changes against the stored values");
    parts.print("Part changes against the stored values");
    missing.print("Names left without a version that look like they have one");
    Ok(())
}
