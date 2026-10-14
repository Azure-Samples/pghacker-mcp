-- The code's commit history: every line of every commit on every branch,
-- stored once (crates/pghacker-code/src/history). A line instance or file
-- version is alive at the commits whose addresses lie in [lo, hi), except
-- where `alive` (set only when a branch deleted it) says otherwise:
--   lo <= X AND X < hi AND (alive IS NULL OR alive @> X)
-- `alive` is only ever a filter: GiST scans of multiranges can miss rows on
-- PostgreSQL 14 and later, so it has no index.
--
-- A build fills these tables in a schema of its own and swaps them in; the
-- code_history_sync job applies new commits. The build copies the tables'
-- columns, constraints and indexes from here.

-- The build and its syncs: one row.
CREATE TABLE code_history (
    id           int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    -- The branch the others fork from.
    master       text NOT NULL,
    -- Where syncs fetch new commits from; NULL for a local clone only.
    url          text,
    -- The path prefixes kept; empty keeps the whole tree.
    paths        text[] NOT NULL DEFAULT '{}',
    built_at     timestamptz NOT NULL DEFAULT now(),
    synced_at    timestamptz,
    -- Master's newest commit.
    head         text,
    -- Bumped when weave keys are renumbered, which invalidates cached line
    -- numbers.
    weave_epoch  int NOT NULL DEFAULT 0,
    stats        jsonb NOT NULL DEFAULT '{}'
);

-- Master and each branch. Master commit k has address k * 2^32; the n-th
-- branch forked at master commit f owns [f * 2^32 + n * 2^24 + 1,
-- f * 2^32 + (n + 1) * 2^24].
CREATE TABLE code_lanes (
    id          int2 PRIMARY KEY,
    name        text NOT NULL UNIQUE,
    -- The master commit the branch forks at; NULL for master.
    fork_addr   int8,
    lo          int8 NOT NULL,
    hi          int8 NOT NULL,
    head_addr   int8 NOT NULL
);

CREATE TABLE code_commits (
    addr            int8 PRIMARY KEY,
    sha             text NOT NULL UNIQUE,
    lane            int2 NOT NULL,
    committed_at    timestamptz NOT NULL,
    authored_at     timestamptz NOT NULL,
    -- Names only: emails aren't kept, as with mail.
    author_name     text NOT NULL,
    committer_name  text NOT NULL,
    subject         text NOT NULL,
    -- The whole message, subject first.
    message         text NOT NULL,
    -- Its trailers by key, values in order: people by name, Discussion as
    -- written (postgr.es/m URLs or message IDs), the others whole. Keys:
    -- Discussion, Author, Co-authored-by, Reviewed-by, Reported-by,
    -- Tested-by, Suggested-by, Diagnosed-by, Analyzed-by, Backpatch-through,
    -- Security, Bug.
    trailers        jsonb NOT NULL DEFAULT '{}',
    -- A hash of the changes without whitespace, line numbers or context,
    -- independent of file order (like git patch-id --stable, but not the
    -- same value); NULL when no file changed.
    patch_id        text,
    -- Commits applying one change to several lanes (the same patch_id, or
    -- the same subject and author within a week, touching a common file)
    -- share their group's first commit's address, master's when it has
    -- one; NULL for a commit alone.
    backpatch       int8
);
CREATE INDEX code_commits_backpatch ON code_commits (backpatch) WHERE backpatch IS NOT NULL;
CREATE INDEX code_commits_committed_at ON code_commits (committed_at);
CREATE INDEX code_commits_message_trgm ON code_commits USING gin (message gin_trgm_ops);

-- What each commit changed, file by file, as git show --stat counts it
-- (changes of mode alone, symlinks and submodules aside).
CREATE TABLE code_commit_files (
    addr         int8 NOT NULL,
    path         text NOT NULL,
    -- Where a rename moved the file from.
    old_path     text,
    status       text NOT NULL CHECK (status IN ('A', 'D', 'M', 'R')),
    added        int4 NOT NULL,
    removed      int4 NOT NULL,
    -- Either side is binary: no lines counted.
    is_binary    boolean NOT NULL,
    -- The version the commit made (NULL when deleted) and the one it ended
    -- (NULL when added).
    version      int4,
    old_version  int4,
    PRIMARY KEY (addr, path)
);
CREATE INDEX code_commit_files_path ON code_commit_files (path text_pattern_ops, addr);

CREATE TABLE code_refs (
    kind  text NOT NULL CHECK (kind IN ('branch', 'tag')),
    name  text NOT NULL,
    addr  int8 NOT NULL,
    PRIMARY KEY (kind, name)
);

-- A file's identity across whole-file renames (git's 50% similarity).
CREATE TABLE code_lineages (
    id          int4 PRIMARY KEY,
    first_path  text NOT NULL,
    born        int8 NOT NULL
);

-- A file's content at a path. A commit's tree is the versions alive at it.
CREATE TABLE code_file_versions (
    id             int4 PRIMARY KEY,
    lineage        int4 NOT NULL,
    path           text NOT NULL,
    blob_sha       text NOT NULL,
    lo             int8 NOT NULL,
    hi             int8 NOT NULL,
    alive          int8multirange,
    size           int4 NOT NULL,
    line_count     int4 NOT NULL,
    -- Binary to the code tools: a NUL byte or invalid UTF-8. Without a NUL
    -- its lines are kept all the same (lines that aren't UTF-8 read as
    -- Latin-1), so blame and diffs follow them as git does.
    is_binary      boolean NOT NULL,
    -- Whether the file ends with a newline, and starts with a UTF-8 BOM, so
    -- its lines rebuild the blob exactly.
    final_newline  boolean NOT NULL,
    bom            boolean NOT NULL,
    subsystem      text
);
CREATE INDEX code_file_versions_hi_lo ON code_file_versions (hi, lo);
CREATE INDEX code_file_versions_lineage ON code_file_versions (lineage, lo);
CREATE INDEX code_file_versions_path ON code_file_versions (path);

-- One row per line instance. A changed line is a new instance. Within a
-- lineage, `weave` orders every line it ever had consistently with every
-- version; a version's lines are its lineage's instances alive at the
-- version's birth, in weave order.
CREATE TABLE code_line_instances (
    weave    int8 NOT NULL,
    lo       int8 NOT NULL,
    hi       int8 NOT NULL,
    id       int4 PRIMARY KEY,
    lineage  int4 NOT NULL,
    -- The line's length in the file, for byte offsets: octet_length(line),
    -- except for a line read as Latin-1, which is longer as text.
    bytes    int4 NOT NULL,
    alive    int8multirange,
    -- The line this one replaced one-for-one (in a hunk of as many lines
    -- added as removed), which blame follows through ignored revisions.
    replaces int4,
    -- Without the line terminator; a CR before LF stays, as in code_lines.
    line     text NOT NULL
);
CREATE INDEX code_line_instances_scope ON code_line_instances (lineage, hi, lo);
CREATE INDEX code_line_instances_weave ON code_line_instances (lineage, weave);
CREATE INDEX code_line_instances_trgm ON code_line_instances USING gin (line gin_trgm_ops);
