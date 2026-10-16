// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! pghacker-mcp's mail and graph tools: threads, authors, extracts and
//! author perspectives, searched by full text (BM25), semantic search
//! (pgvector) or both (hybrid, the default). Read-only; results cite Message-IDs and
//! verbatim quotes, email addresses as written.

mod common;
pub mod extracts;
pub mod ontology;
pub mod people;
pub mod threads;

pub use common::{AuthorRef, Mode, Searcher};

/// Tool descriptions, for agents.
pub mod descriptions {
    pub const SEARCH_THREADS: &str = "Searches pgsql-hackers threads by full text (BM25), semantic search (embeddings) or both \
        (mode hybrid, the default). Optional author (id or name) and since/until (ISO dates) filters. Returns threads with \
        subject, dates, participants, linked features and the best-matching passages with their Message-IDs.";
    pub const GET_THREAD: &str = "Returns a thread by thread_id or by any of its Message-IDs: subject, dates, \
        participants, extracts (features, decisions, positions) and its messages in order with author, date, \
        Message-ID, reply parent, text without quoted replies, patches (files changed) and mentions of files, \
        commits, CommitFest entries and releases. Pages with offset/limit.";
    pub const SEARCH_EXTRACTS: &str = "Searches the extracts: features (capabilities or changes to PostgreSQL \
        that threads propose, review, commit, revert or reject, each with what it does, its status, the release it \
        targets or shipped in, its threads and the files that implement it), decisions (resolutions a thread \
        reached: outcome, alternatives, rationale and the feature each decides) and positions (an author's stance \
        on a feature or decision), by full text, semantic search or both (mode hybrid, the default). A feature's \
        status is the most advanced any of its threads showed: proposed, in_progress, committed, reverted, \
        rejected, returned (with feedback, in a CommitFest), withdrawn or unknown. Optional kind and author \
        filters. Each result cites messages with verbatim quotes.";
    pub const AUTHOR_PERSPECTIVE: &str = "Summarizes an author's perspective on a topic: their extracted positions \
        (stance and summary on features and decisions) that match the topic through their own text or the feature or \
        decision they are about, best first, in mode fulltext, semantic or hybrid (default; semantic and hybrid keep \
        only positions close to the topic), each with cited quotes; plus their most relevant messages on the topic as \
        further evidence. paths (files or folders, such as a patch's) adds their positions on features implemented \
        there, and each position says whether it matched the topic, the paths or both. until (a date) keeps only \
        evidence from before it: what they had said by then.";
    pub const SEARCH_AUTHORS: &str =
        "Finds authors by name, with how many messages they wrote and when.";
    pub const GET_AUTHOR: &str = "Returns an author's profile: names used, message counts and dates, most active \
        threads, positions by stance, and the features they took positions on.";
    pub const GET_ONTOLOGY: &str = "Describes the data: graph vertex labels and edge types with counts and \
        properties, tables and columns for the query tool (the schema's comments describe each), example SQL and \
        Cypher, search modes, and what's loaded (the mailing lists and the range of their months, the commit \
        history, extraction coverage).";
}
