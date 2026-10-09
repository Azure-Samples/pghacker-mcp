-- Source code snapshots, one row per line for pg_trgm code search.

CREATE TABLE code_snapshots (
    id          serial PRIMARY KEY,
    source_id   bigint REFERENCES sources (id) ON DELETE CASCADE,
    -- 'postgres', or a replay repository's name.
    repo        text NOT NULL,
    -- The branch or tag the SHA was pinned from, such as REL_19_STABLE.
    ref         text NOT NULL,
    sha         text NOT NULL,
    -- The PostgreSQL major version, such as '19'.
    version     text,
    -- The path prefixes loaded; empty means the whole tree.
    paths       text[] NOT NULL DEFAULT '{}',
    state       text NOT NULL DEFAULT 'loading' CHECK (state IN ('loading', 'ready', 'failed')),
    file_count  int NOT NULL DEFAULT 0,
    line_count  bigint NOT NULL DEFAULT 0,
    bytes       bigint NOT NULL DEFAULT 0,
    loaded_at   timestamptz,
    UNIQUE (repo, sha)
);

CREATE TABLE code_files (
    id            serial PRIMARY KEY,
    snapshot_id   int NOT NULL REFERENCES code_snapshots (id) ON DELETE CASCADE,
    path          text NOT NULL,
    subsystem     text,
    blob_sha      text NOT NULL,
    size          int NOT NULL,
    line_count    int NOT NULL,
    is_binary     boolean NOT NULL DEFAULT false,
    -- Whether the file ends with a newline, so lines rebuild it exactly.
    final_newline boolean NOT NULL DEFAULT true,
    UNIQUE (snapshot_id, path)
);
CREATE INDEX code_files_path_trgm ON code_files USING gin (path gin_trgm_ops);

CREATE TABLE code_lines (
    file_id  int NOT NULL REFERENCES code_files (id) ON DELETE CASCADE,
    -- 1-based, as grep numbers lines.
    line_no  int NOT NULL,
    -- Without the line terminator; a CR before LF stays, as ripgrep sees it.
    line     text NOT NULL,
    PRIMARY KEY (file_id, line_no)
);
CREATE INDEX code_lines_trgm ON code_lines USING gin (line gin_trgm_ops);
