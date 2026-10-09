//! pgsql-hackers archive retrieval, parsing, threading, authors, patches,
//! anchors and chunks.
//!
//! Deterministic code handles the verbatim facts of the archive: Message-IDs,
//! authors, dates, threading, patches and anchors. The indexer's job workers
//! call [`ingest`]; models later make only judgments.
//!
//! - [`ArchiveClient`] retrieves monthly mbox archives, flat thread pages and
//!   raw messages from the public archive: rate-limited, retried and cached.
//! - [`mbox`] splits an mbox, [`parse`] decodes one message, [`clean`] keeps
//!   the author's own words, [`patch`] finds patches, [`anchors`] finds files,
//!   commits, CommitFest entries and releases, and [`chunk`] cuts search chunks.
//! - [`store`] threads messages, matches authors and writes the rows;
//!   [`ingest`] runs the whole pipeline for a month or a thread.

pub mod anchors;
pub mod chunk;
pub mod clean;
mod client;
pub mod flat;
pub mod ingest;
pub mod mbox;
pub mod parse;
pub mod patch;
pub mod store;
pub mod subject;

pub use client::{ArchiveClient, ArchiveConfig, DEFAULT_USER_AGENT, NotFound, encode_path_segment};
