//! Times one code-tool call through the snapshot and history engines in
//! pghacker_bench (see `pghacker-replay bench-prepare`): a grep command line
//! or, with --search, a search for the text. Runs each three times; set
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
    let (version, text, as_search) = match args.as_slice() {
        [v, t] => (v.clone(), t.clone(), false),
        [v, flag, t] if flag == "--search" => (v.clone(), t.clone(), true),
        _ => anyhow::bail!("usage: profile_history VERSION [--search] 'grep ...'|TEXT"),
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
            let summary = if as_search {
                let r = search(
                    &client,
                    s,
                    &SearchArgs::new(pghacker_codesearch::search::Strings::from(text.as_str())),
                )
                .await?;
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
