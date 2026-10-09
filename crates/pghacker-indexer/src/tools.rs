//! The indexer's MCP tools, for crawler sessions: sources, progress,
//! extraction and repair.

use rmcp::ErrorData;
use serde::Deserialize;
use serde_json::Value;

use pghacker_core::mcp::{Effect, Output, ToolSpec, Tools, failed, parse, schema};

use crate::Indexer;
use crate::extraction::{
    FindDuplicatesArgs, MatchArgs, MergeAuthorsArgs, MergeExtractsArgs, NextBatchArgs,
    ReadMessagesArgs, SubmitArgs, UndoMergeArgs,
};
use crate::sources::{AddSourceArgs, RemoveSourceArgs};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

const READ: Effect = Effect::Read;
const WRITE: Effect = Effect::Write {
    destructive: false,
    idempotent: true,
};
const CREATE: Effect = Effect::Write {
    destructive: false,
    idempotent: false,
};
const DESTRUCTIVE: Effect = Effect::Write {
    destructive: true,
    idempotent: false,
};

pub const INSTRUCTIONS: &str = "pghacker-indexer: tools for crawler sessions that build the pghacker corpus. \
Sources (add_source, list_sources, remove_source) choose mailing lists and PostgreSQL versions; workers then \
retrieve, parse, embed and graph them; ingest_status shows progress. Extraction: next_extraction_batch leases \
threads, read_messages pages through long ones, match_extracts compares your draft features and decisions with \
existing ones, and submit_extracts saves a thread's features, decisions and positions with verbatim citations. \
find_duplicates, merge_authors, merge_extracts and undo_merge repair the corpus.";

pub static TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "add_source",
        description: "Adds a source and queues its retrieval: a mailing list (kind mailing_list, name pgsql-hackers) \
            or a PostgreSQL version's code (kind postgres_version, name 19; branch, sha and paths default to the \
            configured test slice). Idempotent. Wave 1 retrieves only the configured test slices.",
        effect: WRITE,
        schema: schema::<AddSourceArgs>,
    },
    ToolSpec {
        name: "remove_source",
        description: "Disables a source and cancels its queued jobs; purge also deletes a version's code snapshot.",
        effect: DESTRUCTIVE,
        schema: schema::<RemoveSourceArgs>,
    },
    ToolSpec {
        name: "list_sources",
        description: "Lists sources with their jobs, and the lists and versions that can be added.",
        effect: READ,
        schema: schema::<NoArgs>,
    },
    ToolSpec {
        name: "ingest_status",
        description: "Reports progress: jobs by kind with recent errors; months and seed threads retrieved; \
            messages, threads, authors, patches and anchors parsed; chunks embedded; threads extracted, pending \
            and skipped; features, decisions, positions and citations; code snapshots; graph vertices and edges.",
        effect: READ,
        schema: schema::<NoArgs>,
    },
    ToolSpec {
        name: "next_extraction_batch",
        description: "Leases 1-3 threads that need extraction (or the thread_ids given) and returns each with its \
            participants (author_id, name), extracts already linked to it, and the first page of messages: \
            Message-ID, author, date, text without quoted replies, patches (files changed) and anchors. Long \
            threads have next_offset: page on with read_messages. Returns done=true when nothing is pending.",
        effect: CREATE,
        schema: schema::<NextBatchArgs>,
    },
    ToolSpec {
        name: "read_messages",
        description: "Returns the next page of a leased thread's messages from offset (the previous page's next_offset).",
        effect: READ,
        schema: schema::<ReadMessagesArgs>,
    },
    ToolSpec {
        name: "match_extracts",
        description: "Deduplication, required before submitting: send draft features (name, what it does) and \
            decisions (title, outcome and rationale). Returns a draft_id for each and the most similar existing \
            features or decisions with semantic and name similarity and a near_duplicate flag. Then decide per \
            draft: reuse a candidate that is the same thing, or create a new one.",
        effect: CREATE,
        schema: schema::<MatchArgs>,
    },
    ToolSpec {
        name: "submit_extracts",
        description: "Saves one leased thread's extracts, all or nothing: features (new with a slug, or reuse an \
            existing id; status, release, files from the thread's patches), decisions (outcome, alternatives, \
            rationale, the feature decided), and positions (an author's stance on a feature or decision, citing \
            the author's own messages). Every extract cites messages of the thread with verbatim quotes. A new \
            extract that resembles an existing one needs distinct_because. Send skip_reason instead for a thread \
            without technical discussion. On errors nothing is saved: fix all and resubmit.",
        effect: CREATE,
        schema: schema::<SubmitArgs>,
    },
    ToolSpec {
        name: "find_duplicates",
        description: "Lists pairs of features or decisions that look like the same thing, by meaning or name.",
        effect: READ,
        schema: schema::<FindDuplicatesArgs>,
    },
    ToolSpec {
        name: "merge_authors",
        description: "Merges two author identities (the same person with two addresses): messages, positions and \
            names move to into_id. Logged and reversible with undo_merge.",
        effect: DESTRUCTIVE,
        schema: schema::<MergeAuthorsArgs>,
    },
    ToolSpec {
        name: "merge_extracts",
        description: "Merges a duplicate feature or decision into another: threads, files, citations, decisions and \
            positions move to into_id. Logged and reversible with undo_merge.",
        effect: DESTRUCTIVE,
        schema: schema::<MergeExtractsArgs>,
    },
    ToolSpec {
        name: "undo_merge",
        description: "Undoes a merge by its merge_id.",
        effect: DESTRUCTIVE,
        schema: schema::<UndoMergeArgs>,
    },
];

/// Turns a tool function's result into the tool's output: errors are the
/// caller's to read.
fn output(result: anyhow::Result<Value>) -> Result<Output, ErrorData> {
    Ok(match result {
        Ok(value) if value.get("accepted") == Some(&Value::Bool(false)) => {
            Output::Failed(serde_json::to_string_pretty(&value).unwrap_or_default())
        }
        Ok(value) => Output::Json(value),
        Err(error) => failed(error),
    })
}

impl Tools for Indexer {
    fn specs(&self) -> &'static [ToolSpec] {
        TOOLS
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Output, ErrorData> {
        use crate::{extraction, sources, status};
        match name {
            "add_source" => output(sources::add_source(self, parse(name, arguments)?).await),
            "remove_source" => output(sources::remove_source(self, parse(name, arguments)?).await),
            "list_sources" => output(sources::list_sources(self).await),
            "ingest_status" => output(status::ingest_status(self).await),
            "next_extraction_batch" => {
                output(extraction::next_batch(self, parse(name, arguments)?).await)
            }
            "read_messages" => {
                output(extraction::read_messages(self, parse(name, arguments)?).await)
            }
            "match_extracts" => {
                output(extraction::match_extracts(self, parse(name, arguments)?).await)
            }
            "submit_extracts" => output(extraction::submit(self, parse(name, arguments)?).await),
            "find_duplicates" => {
                output(extraction::find_duplicates(self, parse(name, arguments)?).await)
            }
            "merge_authors" => {
                output(extraction::merge_authors(self, parse(name, arguments)?).await)
            }
            "merge_extracts" => {
                output(extraction::merge_extracts(self, parse(name, arguments)?).await)
            }
            "undo_merge" => output(extraction::undo_merge(self, parse(name, arguments)?).await),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }
}
