// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Shared by pghacker-indexer and pghacker-mcp: configuration from the
//! environment, PostgreSQL pools, tool calls that can be abandoned, the
//! bootstrap and migrations of schema `pghacker` and graph `pghacker_graph`,
//! the embeddings client, search modes, citation checks, the commit history's
//! schema and release ordering, and the MCP server plumbing.

pub mod calls;
pub mod code;
pub mod config;
pub mod db;
pub mod embed;
pub mod graph_indexes;
pub mod mcp;
pub mod migrate;
pub mod search;
pub mod telemetry;
pub mod testing;
pub mod text;

/// The AGE graph that the indexer projects the tables into.
pub const GRAPH: &str = "pghacker_graph";

/// The graph's vertex labels, created by bootstrap.
pub const VERTEX_LABELS: &[&str] = &[
    "Author",
    "Thread",
    "Message",
    "Feature",
    "Decision",
    "Position",
    "Release",
    "Subsystem",
    "File",
    "Commit",
    "CommitFestEntry",
];

/// The graph's edge labels, created by bootstrap: a new one empty, with its
/// indexes. MODIFIES, DISCUSSED_IN, BACKPATCH_OF and COMMITTED connect the
/// commit history.
pub const EDGE_LABELS: &[&str] = &[
    "AUTHORED",
    "REPLIES_TO",
    "IN_THREAD",
    "MENTIONS",
    "PART_OF",
    "DISCUSSES",
    "DECIDES",
    "TARGETS",
    "IMPLEMENTS",
    "HOLDS",
    "ABOUT",
    "EVIDENCED_BY",
    "MODIFIES",
    "DISCUSSED_IN",
    "BACKPATCH_OF",
    "COMMITTED",
];
