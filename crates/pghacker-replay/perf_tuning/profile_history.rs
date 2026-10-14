//! Times one code-tool call through the snapshot and history engines in
//! pghacker_bench (see `pghacker-replay bench-prepare`): a grep command line
//! or, with --search, a search for the text, or with --json, a search with
//! those JSON arguments, whose answers go to /tmp/profile-<engine>.json.
//! Runs each three times; set
//! RUST_LOG=pghacker_codesearch=debug for the history engine's phases.

use std::time::Instant;

use anyhow::{Context, Result};
use pghacker_codesearch::grep::{Limits, run};
use pghacker_codesearch::{SearchArgs, Snapshot, resolve_snapshot, search};
use pghacker_core::config::{Env, with_database};
use pghacker_core::db;

#[tokio::main]
async fn main() -> Result<()> {
    pghacker_core::telemetry::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (version, text, mode) = match args.as_slice() {
        [v, t] => (v.clone(), t.clone(), "grep"),
        [v, flag, t] if flag == "--search" => (v.clone(), t.clone(), "search"),
        [v, flag, t] if flag == "--json" => (v.clone(), t.clone(), "json"),
        _ => anyhow::bail!("usage: profile_history VERSION [--search|--json] 'grep ...'|TEXT|JSON"),
    };
    let env = Env::local()?;
    let url = with_database(&env.reader_database_url()?, "pghacker_bench")?;
    let client = db::connect(&url, "profile-history").await?;
    let snap = resolve_snapshot(&client, "postgres", Some(&version)).await?;
    let hist =
        pghacker_codesearch::history::attach(&client, snap.clone(), "pghacker_history").await?;
    for (name, s) in [
        ("snapshot", &snap),
        ("history", &hist),
        ("snapshot", &snap),
        ("history", &hist),
    ] {
        for round in 0..3 {
            let t = Instant::now();
            let summary = if mode != "grep" {
                let search_args = if mode == "json" {
                    serde_json::from_str(&text).context("bad search arguments")?
                } else {
                    SearchArgs::new(pghacker_codesearch::search::Strings::from(text.as_str()))
                };
                let r = search(&client, s, &search_args).await?;
                if round == 0 {
                    std::fs::write(
                        format!("/tmp/profile-{name}.json"),
                        serde_json::to_string_pretty(&r)?,
                    )?;
                }
                format!("{} lines, complete {}", r.total_lines, r.complete)
            } else {
                let o = run(&client, s, &text, Limits::default())
                    .await
                    .context("grep failed")?;
                format!(
                    "{} bytes, status {}, incomplete {}",
                    o.stdout.len(),
                    o.status,
                    o.incomplete
                )
            };
            println!(
                "{name:>8} {round}: {:>8.1} ms  {summary}",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }
    let _: &Snapshot = &snap;
    Ok(())
}
