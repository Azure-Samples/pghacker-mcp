-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Mailing-list messages, threads, authors, patches, anchors and search chunks.

CREATE TABLE authors (
    id            bigserial PRIMARY KEY,
    display_name  text NOT NULL,
    -- Set when merge_authors folds this author into another.
    merged_into   bigint REFERENCES authors (id),
    message_count int NOT NULL DEFAULT 0,
    first_seen    timestamptz,
    last_seen     timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX authors_name_trgm ON authors USING gin (display_name gin_trgm_ops);

CREATE TABLE author_names (
    author_id bigint NOT NULL REFERENCES authors (id) ON DELETE CASCADE,
    name      text NOT NULL,
    count     int NOT NULL DEFAULT 0,
    PRIMARY KEY (author_id, name)
);
CREATE INDEX author_names_trgm ON author_names USING gin (name gin_trgm_ops);

-- Identity matching only. Never granted to the reader, never projected into
-- the graph and never returned by a tool.
CREATE TABLE author_emails (
    email      text PRIMARY KEY,
    author_id  bigint NOT NULL REFERENCES authors (id) ON DELETE CASCADE
);
REVOKE ALL ON author_emails FROM pgh_reader;

CREATE TABLE threads (
    id                bigserial PRIMARY KEY,
    list              text NOT NULL,
    -- The thread's identity: its root Message-ID, even if the root itself
    -- hasn't been retrieved (the first References entry).
    root_message_id   text NOT NULL UNIQUE,
    subject           text NOT NULL DEFAULT '',
    started_at        timestamptz,
    last_at           timestamptz,
    message_count     int NOT NULL DEFAULT 0,
    participant_count int NOT NULL DEFAULT 0,
    -- Bumped whenever messages are added; extraction records the version it saw.
    content_version   int NOT NULL DEFAULT 0,
    extraction_state  text NOT NULL DEFAULT 'pending'
                      CHECK (extraction_state IN ('pending', 'extracted', 'skipped')),
    extracted_version int,
    extracted_at      timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX threads_subject_trgm ON threads USING gin (subject gin_trgm_ops);
CREATE INDEX threads_last_at ON threads (last_at DESC);

CREATE TABLE messages (
    id                bigserial PRIMARY KEY,
    -- Without angle brackets.
    message_id        text NOT NULL UNIQUE,
    list              text NOT NULL,
    thread_id         bigint NOT NULL REFERENCES threads (id),
    -- In-Reply-To, else the last References entry.
    parent_message_id text,
    references_ids    text[] NOT NULL DEFAULT '{}',
    author_id         bigint REFERENCES authors (id),
    -- The From display name, as the public archive shows it.
    from_name         text NOT NULL DEFAULT '',
    subject           text NOT NULL DEFAULT '',
    sent_at           timestamptz,
    -- The text/plain body, decoded.
    body              text NOT NULL DEFAULT '',
    -- The body without quoted replies, signatures and inline patches: what search indexes.
    body_clean        text NOT NULL DEFAULT '',
    has_patch         boolean NOT NULL DEFAULT false,
    -- YYYY-MM of the monthly mbox it came from, if any.
    archive_month     text,
    source_id         bigint REFERENCES sources (id) ON DELETE SET NULL,
    raw_sha256        bytea,
    raw_size          int,
    created_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX messages_thread ON messages (thread_id, sent_at);
CREATE INDEX messages_author ON messages (author_id, sent_at);
CREATE INDEX messages_parent ON messages (parent_message_id);
CREATE INDEX messages_sent ON messages (sent_at);

CREATE TABLE patches (
    id             bigserial PRIMARY KEY,
    message_id     bigint NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    -- The attachment's name, or inline-<n> for a diff in the body.
    filename       text NOT NULL,
    -- From names like v3-0001-....patch.
    series_version int,
    part           int,
    paths          text[] NOT NULL DEFAULT '{}',
    added          int NOT NULL DEFAULT 0,
    removed        int NOT NULL DEFAULT 0,
    content        text NOT NULL,
    UNIQUE (message_id, filename)
);
CREATE INDEX patches_paths ON patches USING gin (paths);

CREATE TABLE anchors (
    id          bigserial PRIMARY KEY,
    message_id  bigint NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('file', 'commit', 'commitfest', 'release')),
    -- A path, an abbreviated commit SHA, a CommitFest entry ID or a major version.
    value       text NOT NULL,
    -- For files in a loaded snapshot: REL_19_STABLE@<sha>:<path>.
    resolved    text,
    origin      text NOT NULL DEFAULT 'body' CHECK (origin IN ('body', 'patch', 'subject')),
    UNIQUE (message_id, kind, value)
);
CREATE INDEX anchors_kind_value ON anchors (kind, value);

CREATE TABLE message_chunks (
    id         bigserial PRIMARY KEY,
    message_id bigint NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    thread_id  bigint NOT NULL REFERENCES threads (id),
    ord        int NOT NULL,
    content    text NOT NULL,
    embedding  halfvec(1536),
    UNIQUE (message_id, ord)
);
CREATE INDEX message_chunks_bm25 ON message_chunks USING bm25 (content) WITH (text_config = 'english');
CREATE INDEX message_chunks_embedding ON message_chunks USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX message_chunks_unembedded ON message_chunks (id) WHERE embedding IS NULL;
CREATE INDEX message_chunks_thread ON message_chunks (thread_id);
