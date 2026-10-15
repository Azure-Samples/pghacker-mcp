-- The commit history in the graph (graph.rs): a Commit vertex for every
-- commit, keyed by its full SHA, which MODIFIES its files, is DISCUSSED_IN
-- the threads that discussed it, is a BACKPATCH_OF its back-patch group's
-- first commit, and which the one mail author with its author name
-- COMMITTED; and a File vertex, PART_OF its subsystem, for every path the
-- history has. With no history installed its tables are empty, and nothing
-- new is projected.
--
-- The history's tables have no triggers: an install swaps them in whole,
-- which would drop them, a build writes some 600,000 rows at once, and a
-- regroup rewrites the back-patch groups. Their writers record instead, in
-- the writing transaction, what they change, with the functions below
-- (crates/pghacker-code/src/history); an install asks for a full sync.

-- The indexes lock their tables against writes until the migration commits:
-- fail rather than wait behind a long writer, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

-- The projection that the last full sync computed (graph.rs PROJECTION). A
-- newer one needs a full sync, which an indexer with an older one doesn't
-- give, as while an old revision still runs the jobs beside a new one.
-- First: a running full sync holds graph_sync_state until it ends, so this
-- times out before any index is built rather than after.
ALTER TABLE graph_sync_state ADD COLUMN projection int NOT NULL DEFAULT 1;
SELECT graph_request_full('the commit history joins the graph');

-- The one author, not merged, that has a name as its display name or as a
-- name it sent mail under, ignoring case; none when several do. A commit's
-- author name is its committer's in PostgreSQL's history.
CREATE FUNCTION graph_committer(p_name text) RETURNS TABLE (author_id bigint) LANGUAGE sql STABLE AS $$
    SELECT min(x.id) FROM (
        SELECT a.id FROM pghacker.authors a
        WHERE lower(a.display_name) = lower(p_name) AND a.merged_into IS NULL
        UNION
        SELECT a.id FROM pghacker.author_names n JOIN pghacker.authors a ON a.id = n.author_id
        WHERE lower(n.name) = lower(p_name) AND a.merged_into IS NULL
    ) x HAVING count(*) = 1
$$;

-- What a commit that a history sync adds affects: its vertex, its COMMITTED
-- and MODIFIES edges, and its files with their subsystems.
CREATE FUNCTION graph_commit_entries(p_addr bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT NULL::text, 'commit:' || c.sha, NULL::text FROM pghacker.code_commits c WHERE c.addr = p_addr
    UNION ALL
    SELECT 'COMMITTED', 'author:' || m.author_id, 'commit:' || c.sha
    FROM pghacker.code_commits c CROSS JOIN LATERAL pghacker.graph_committer(c.author_name) m
    WHERE c.addr = p_addr
    UNION ALL
    SELECT 'MODIFIES', 'commit:' || c.sha, 'file:' || f.path
    FROM pghacker.code_commits c JOIN pghacker.code_commit_files f ON f.addr = c.addr
    WHERE c.addr = p_addr
    UNION ALL
    SELECT e.* FROM pghacker.code_commit_files f
    LEFT JOIN pghacker.code_file_versions v ON v.id = coalesce(f.version, f.old_version)
    CROSS JOIN LATERAL pghacker.graph_code_file_entries(f.path, v.subsystem) e
    WHERE f.addr = p_addr
$$;

-- What a commit's back-patch group affects, before and after a regroup: the
-- commit's vertex, and its BACKPATCH_OF edge to the group's first commit.
CREATE FUNCTION graph_backpatch_entries(p_addr bigint, p_backpatch bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT NULL::text, 'commit:' || c.sha, NULL::text FROM pghacker.code_commits c WHERE c.addr = p_addr
    UNION ALL
    SELECT 'BACKPATCH_OF', 'commit:' || c.sha, 'commit:' || r.sha
    FROM pghacker.code_commits c JOIN pghacker.code_commits r ON r.addr = p_backpatch
    WHERE c.addr = p_addr AND r.addr <> c.addr
$$;

-- What a link from a commit to a thread that discussed it affects.
CREATE FUNCTION graph_commit_thread_entries(p_sha text, p_thread bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql IMMUTABLE AS $$
    SELECT 'DISCUSSED_IN'::text, 'commit:' || p_sha, 'thread:' || p_thread
$$;

-- A commit anchor names its commit by the full SHA once resolve_anchors has
-- found it in the history, else as written: resolving it moves its MENTIONS
-- to another vertex.
CREATE OR REPLACE FUNCTION graph_capture_anchors() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r
        CROSS JOIN LATERAL pghacker.graph_anchor_entries(r.message_id, r.kind,
            CASE r.kind WHEN 'commit' THEN coalesce(r.resolved, r.value) ELSE r.value END) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r
        CROSS JOIN LATERAL pghacker.graph_anchor_entries(r.message_id, r.kind,
            CASE r.kind WHEN 'commit' THEN coalesce(r.resolved, r.value) ELSE r.value END) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.message_id, OLD.kind, OLD.value, OLD.resolved),
                     (NEW.message_id, NEW.kind, NEW.value, NEW.resolved)) r (message_id, kind, value, resolved)
        CROSS JOIN LATERAL pghacker.graph_anchor_entries(r.message_id, r.kind,
            CASE r.kind WHEN 'commit' THEN coalesce(r.resolved, r.value) ELSE r.value END) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE OR REPLACE TRIGGER graph_capture_update AFTER UPDATE ON anchors FOR EACH ROW
    WHEN ((OLD.message_id, OLD.kind, OLD.value) IS DISTINCT FROM (NEW.message_id, NEW.kind, NEW.value)
          OR (NEW.kind = 'commit' AND OLD.resolved IS DISTINCT FROM NEW.resolved))
    EXECUTE FUNCTION graph_capture_anchors();

-- What the sync looks up by: the commits that anchors name, the authors with
-- a committer's name and that name's commits, and a subsystem's files.
-- History builds copy the history tables' indexes from here.
CREATE INDEX anchors_commit ON anchors ((coalesce(resolved, value))) WHERE kind = 'commit';
CREATE INDEX authors_name_lower ON authors (lower(display_name));
CREATE INDEX author_names_name_lower ON author_names (lower(name));
CREATE INDEX code_commits_author_name ON code_commits (lower(author_name));
CREATE INDEX code_file_versions_subsystem ON code_file_versions (subsystem);

-- The committer that the graph has for each author name in the history,
-- lower-cased: the author whose COMMITTED edges reach the name's commits. No
-- row for a name that no author, or several, have. The sync keeps it, to
-- find the names whose author changed.
CREATE TABLE graph_committers (
    name       text PRIMARY KEY,
    author_key text NOT NULL
);
REVOKE ALL ON graph_committers FROM pgh_reader;
