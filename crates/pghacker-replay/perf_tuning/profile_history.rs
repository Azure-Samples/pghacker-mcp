// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Times one code-tool call through a commit history: a grep command line
//! or, with --search, a search for the text, or with --json, a search with
//! those JSON arguments, whose answer goes to
//! .runtime/replay/profile-history.json. Reads the history in schema
//! --schema (default pghacker) of database --database (default the app's,
//! from .env) as pgh_reader, at VERSION. Runs the call three times; set
//! RUST_LOG=pghacker_codesearch=debug for its phases.
//!
//! usage: profile_history [--database NAME] [--schema NAME] VERSION
//!        [--search|--json] 'grep ...'|TEXT|JSON

use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use pghacker_codesearch::SearchArgs;
use pghacker_codesearch::grep::{Limits, run};
use pghacker_codesearch::search;
use pghacker_core::config::{Env, repo_root, with_database};
use pghacker_core::db;

#[derive(Parser)]
#[command(about = "Times one code-tool call through a commit history")]
struct Args {
    /// The database to read as pgh_reader; default the app's, from .env.
    #[arg(long)]
    database: Option<String>,
    /// The history's schema: a build's, or pghacker once installed.
    #[arg(long, default_value = "pghacker")]
    schema: String,
    /// Search for the text instead of running a grep command line.
    #[arg(long, conflicts_with = "json")]
    search: bool,
    /// Search with the JSON arguments instead.
    #[arg(long)]
    json: bool,
    /// The version to read, as the tools take it: 19, REL_19_STABLE, a SHA.
    version: String,
    /// The grep command line, the text or the JSON arguments.
    #[arg(allow_hyphen_values = true)]
    text: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    pghacker_core::telemetry::init();
    let args = Args::parse();
    let env = Env::local()?;
    let mut url = env.reader_database_url()?;
    if let Some(database) = &args.database {
        url = with_database(&url, database)?;
    }
    let client = db::connect(&url, "profile-history").await?;
    let snap =
        pghacker_codesearch::rev::resolve(&client, &args.schema, "postgres", Some(&args.version))
            .await?;
    println!(
        "{} {} ({}) in {}",
        snap.git_ref,
        &snap.sha[..12],
        snap.version.as_deref().unwrap_or("-"),
        args.schema
    );
    for round in 0..3 {
        let t = Instant::now();
        let summary = if args.search || args.json {
            let search_args = if args.json {
                serde_json::from_str(&args.text).context("bad search arguments")?
            } else {
                SearchArgs::new(search::Strings::from(args.text.as_str()))
            };
            let r = search::search(&client, &snap, &search_args).await?;
            if round == 0 {
                let root = repo_root().context("cannot find the repository root")?;
                let dir = root.join(".runtime/replay");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join("profile-history.json"),
                    serde_json::to_string_pretty(&r)?,
                )?;
            }
            format!("{} lines, complete {}", r.total_lines, r.complete)
        } else {
            let o = run(&client, &snap, &args.text, Limits::default())
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
            " history {round}: {:>8.1} ms  {summary}",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    Ok(())
}
