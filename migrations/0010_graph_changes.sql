-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

-- Change capture for the incremental graph sync. Triggers on the tables that
-- the graph projects append, in the writing transaction, the keys of the
-- vertices and the edges that a change may affect, from the row before and
-- after it: a vertex by its key, an edge by its label and end keys. The graph
-- sync recomputes just those from the tables. Writers only append here, so
-- they never wait on the graph. A full sync recomputes the whole graph: after
-- this migration, after a TRUNCATE (as a restore does), and on request.

-- The triggers lock each table against writes until the migration commits. A
-- long writer, such as a snapshot load, would stall the other tables' writers
-- meanwhile: fail instead, and bootstrap tries again.
SET LOCAL lock_timeout = '10s';

CREATE TABLE graph_changes (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- The edge's label, or NULL for a vertex.
    label   text,
    -- The vertex's key, or the edge's start key.
    key     text NOT NULL,
    -- The edge's end key, or NULL for a vertex.
    end_key text,
    at      timestamptz NOT NULL DEFAULT now(),
    CHECK ((label IS NULL) = (end_key IS NULL))
);

-- One row. Each request for a full sync bumps full_requested; a full sync
-- records in full_done the requests it saw before it read the tables.
CREATE TABLE graph_sync_state (
    id                boolean PRIMARY KEY DEFAULT true CHECK (id),
    full_requested    bigint NOT NULL DEFAULT 1,
    full_done         bigint NOT NULL DEFAULT 0,
    full_reason       text,
    full_requested_at timestamptz,
    last_full_at      timestamptz
);
INSERT INTO graph_sync_state (full_reason, full_requested_at) VALUES ('change capture installed', now());

-- Each label's vertices or edges in the graph, kept by the sync in the
-- transaction that changes them, for ingest_status and get_ontology.
CREATE TABLE graph_label_counts (
    label text PRIMARY KEY,
    kind  text NOT NULL CHECK (kind IN ('vertex', 'edge')),
    count bigint NOT NULL
);

REVOKE ALL ON graph_changes, graph_sync_state FROM pgh_reader;

-- Asks for a full sync.
CREATE FUNCTION graph_request_full(reason text) RETURNS void LANGUAGE sql AS $$
    INSERT INTO pghacker.graph_sync_state AS s (full_reason, full_requested_at) VALUES (reason, now())
    ON CONFLICT (id) DO UPDATE
        SET full_requested = s.full_requested + 1, full_reason = EXCLUDED.full_reason,
            full_requested_at = EXCLUDED.full_requested_at
$$;

-- A TRUNCATE records no rows, so it asks for a full sync.
CREATE FUNCTION graph_capture_truncate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pghacker.graph_request_full('TRUNCATE ' || TG_TABLE_NAME);
    RETURN NULL;
END
$$;

-- What each table's row affects: (NULL, vertex key, NULL), or (edge label,
-- start key, end key). Keys and expressions match graph.rs.

CREATE FUNCTION graph_message_entries(p_message_id text, p_thread bigint, p_author bigint, p_parent text)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT e.label, e.key, e.end_key
    FROM (VALUES
        (NULL, 'message:' || p_message_id, NULL),
        ('AUTHORED', 'author:' || p_author, 'message:' || p_message_id),
        ('REPLIES_TO', 'message:' || p_message_id, 'message:' || p_parent),
        ('IN_THREAD', 'message:' || p_message_id, 'thread:' || p_thread)
    ) e (label, key, end_key)
    WHERE e.key IS NOT NULL AND (e.label IS NULL) = (e.end_key IS NULL)
$$;

CREATE FUNCTION graph_anchor_entries(p_message bigint, p_kind text, p_value text)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT e.label, e.key, e.end_key
    FROM (SELECT CASE p_kind WHEN 'file' THEN 'file:' WHEN 'commit' THEN 'commit:'
                             WHEN 'commitfest' THEN 'cf:' ELSE 'release:' END || p_value AS target) t
    CROSS JOIN LATERAL (
        SELECT NULL::text, t.target, NULL::text
        UNION ALL
        SELECT 'MENTIONS', 'message:' || m.message_id, t.target FROM pghacker.messages m WHERE m.id = p_message
    ) e (label, key, end_key)
$$;

CREATE FUNCTION graph_feature_entries(p_slug text, p_release text)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT e.label, e.key, e.end_key
    FROM (VALUES
        (NULL, 'feature:' || p_slug, NULL),
        (NULL, 'release:' || p_release, NULL),
        ('TARGETS', 'feature:' || p_slug, 'release:' || p_release)
    ) e (label, key, end_key)
    WHERE e.key IS NOT NULL AND (e.label IS NULL) = (e.end_key IS NULL)
$$;

CREATE FUNCTION graph_decision_entries(p_id bigint, p_feature bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT NULL::text, 'decision:' || p_id, NULL::text
    UNION ALL
    SELECT 'DECIDES', 'decision:' || p_id, 'feature:' || f.slug FROM pghacker.features f WHERE f.id = p_feature
$$;

CREATE FUNCTION graph_position_entries(p_id bigint, p_author bigint, p_feature bigint, p_decision bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT NULL::text, 'position:' || p_id, NULL::text
    UNION ALL
    SELECT 'HOLDS', 'author:' || p_author, 'position:' || p_id WHERE p_author IS NOT NULL
    UNION ALL
    SELECT 'ABOUT', 'position:' || p_id, 'feature:' || f.slug FROM pghacker.features f WHERE f.id = p_feature
    UNION ALL
    SELECT 'ABOUT', 'position:' || p_id, 'decision:' || p_decision WHERE p_decision IS NOT NULL
$$;

CREATE FUNCTION graph_citation_entries(p_kind text, p_extract bigint, p_message bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT 'EVIDENCED_BY', s.key, 'message:' || m.message_id
    FROM pghacker.messages m
    CROSS JOIN LATERAL (
        SELECT CASE p_kind WHEN 'feature' THEN (SELECT 'feature:' || f.slug FROM pghacker.features f WHERE f.id = p_extract)
                           ELSE p_kind || ':' || p_extract END AS key
    ) s
    WHERE m.id = p_message AND s.key IS NOT NULL
$$;

CREATE FUNCTION graph_feature_thread_entries(p_feature bigint, p_thread bigint)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT 'DISCUSSES', 'thread:' || p_thread, 'feature:' || f.slug FROM pghacker.features f WHERE f.id = p_feature
$$;

CREATE FUNCTION graph_feature_file_entries(p_feature bigint, p_path text)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT NULL::text, 'file:' || p_path, NULL::text
    UNION ALL
    SELECT 'IMPLEMENTS', 'file:' || p_path, 'feature:' || f.slug FROM pghacker.features f WHERE f.id = p_feature
$$;

CREATE FUNCTION graph_code_file_entries(p_path text, p_subsystem text)
RETURNS TABLE (label text, key text, end_key text) LANGUAGE sql STABLE AS $$
    SELECT e.label, e.key, e.end_key
    FROM (VALUES
        (NULL, 'file:' || p_path, NULL),
        (NULL, 'subsystem:' || p_subsystem, NULL),
        ('PART_OF', 'file:' || p_path, 'subsystem:' || p_subsystem)
    ) e (label, key, end_key)
    WHERE e.key IS NOT NULL AND (e.label IS NULL) = (e.end_key IS NULL)
$$;

-- The capture functions. Inserts and deletes are captured per statement, from
-- the transition tables; updates per row, and only when a column that the
-- graph reads changed, so that updates of embeddings, anchor resolutions or
-- workflow state cost nothing.

CREATE FUNCTION graph_capture_authors() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (key) SELECT DISTINCT 'author:' || r.id FROM new_rows r;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (key) SELECT DISTINCT 'author:' || r.id FROM old_rows r;
    ELSE
        INSERT INTO pghacker.graph_changes (key)
        SELECT DISTINCT k FROM (VALUES ('author:' || OLD.id), ('author:' || NEW.id)) v (k);
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_threads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (key) SELECT DISTINCT 'thread:' || r.id FROM new_rows r;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (key) SELECT DISTINCT 'thread:' || r.id FROM old_rows r;
    ELSE
        INSERT INTO pghacker.graph_changes (key)
        SELECT DISTINCT k FROM (VALUES ('thread:' || OLD.id), ('thread:' || NEW.id)) v (k);
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_messages() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r
        CROSS JOIN LATERAL pghacker.graph_message_entries(r.message_id, r.thread_id, r.author_id, r.parent_message_id) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r
        CROSS JOIN LATERAL pghacker.graph_message_entries(r.message_id, r.thread_id, r.author_id, r.parent_message_id) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.message_id, OLD.thread_id, OLD.author_id, OLD.parent_message_id),
                     (NEW.message_id, NEW.thread_id, NEW.author_id, NEW.parent_message_id)) r (message_id, thread_id, author_id, parent)
        CROSS JOIN LATERAL pghacker.graph_message_entries(r.message_id, r.thread_id, r.author_id, r.parent) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_anchors() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r
        CROSS JOIN LATERAL pghacker.graph_anchor_entries(r.message_id, r.kind, r.value) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r
        CROSS JOIN LATERAL pghacker.graph_anchor_entries(r.message_id, r.kind, r.value) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.message_id, OLD.kind, OLD.value), (NEW.message_id, NEW.kind, NEW.value)) r (message_id, kind, value)
        CROSS JOIN LATERAL pghacker.graph_anchor_entries(r.message_id, r.kind, r.value) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_features() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r CROSS JOIN LATERAL pghacker.graph_feature_entries(r.slug, r.release) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r CROSS JOIN LATERAL pghacker.graph_feature_entries(r.slug, r.release) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.slug, OLD.release), (NEW.slug, NEW.release)) r (slug, release)
        CROSS JOIN LATERAL pghacker.graph_feature_entries(r.slug, r.release) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_decisions() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r CROSS JOIN LATERAL pghacker.graph_decision_entries(r.id, r.feature_id) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r CROSS JOIN LATERAL pghacker.graph_decision_entries(r.id, r.feature_id) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.id, OLD.feature_id), (NEW.id, NEW.feature_id)) r (id, feature_id)
        CROSS JOIN LATERAL pghacker.graph_decision_entries(r.id, r.feature_id) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_positions() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r
        CROSS JOIN LATERAL pghacker.graph_position_entries(r.id, r.author_id, r.feature_id, r.decision_id) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r
        CROSS JOIN LATERAL pghacker.graph_position_entries(r.id, r.author_id, r.feature_id, r.decision_id) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.id, OLD.author_id, OLD.feature_id, OLD.decision_id),
                     (NEW.id, NEW.author_id, NEW.feature_id, NEW.decision_id)) r (id, author_id, feature_id, decision_id)
        CROSS JOIN LATERAL pghacker.graph_position_entries(r.id, r.author_id, r.feature_id, r.decision_id) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_citations() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r
        CROSS JOIN LATERAL pghacker.graph_citation_entries(r.extract_kind, r.extract_id, r.message_id) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r
        CROSS JOIN LATERAL pghacker.graph_citation_entries(r.extract_kind, r.extract_id, r.message_id) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.extract_kind, OLD.extract_id, OLD.message_id),
                     (NEW.extract_kind, NEW.extract_id, NEW.message_id)) r (extract_kind, extract_id, message_id)
        CROSS JOIN LATERAL pghacker.graph_citation_entries(r.extract_kind, r.extract_id, r.message_id) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_feature_threads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r CROSS JOIN LATERAL pghacker.graph_feature_thread_entries(r.feature_id, r.thread_id) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r CROSS JOIN LATERAL pghacker.graph_feature_thread_entries(r.feature_id, r.thread_id) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.feature_id, OLD.thread_id), (NEW.feature_id, NEW.thread_id)) r (feature_id, thread_id)
        CROSS JOIN LATERAL pghacker.graph_feature_thread_entries(r.feature_id, r.thread_id) e;
    END IF;
    RETURN NULL;
END
$$;

CREATE FUNCTION graph_capture_feature_files() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r CROSS JOIN LATERAL pghacker.graph_feature_file_entries(r.feature_id, r.path) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r CROSS JOIN LATERAL pghacker.graph_feature_file_entries(r.feature_id, r.path) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.feature_id, OLD.path), (NEW.feature_id, NEW.path)) r (feature_id, path)
        CROSS JOIN LATERAL pghacker.graph_feature_file_entries(r.feature_id, r.path) e;
    END IF;
    RETURN NULL;
END
$$;

-- Files count only in ready snapshots of postgres: a snapshot's files are
-- captured when it becomes ready (graph_capture_code_snapshots), or when
-- they're added to a ready one; deleted files, always.
CREATE FUNCTION graph_capture_code_files() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM new_rows r
        JOIN pghacker.code_snapshots s ON s.id = r.snapshot_id AND s.state = 'ready' AND s.repo = 'postgres'
        CROSS JOIN LATERAL pghacker.graph_code_file_entries(r.path, r.subsystem) e;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.* FROM old_rows r CROSS JOIN LATERAL pghacker.graph_code_file_entries(r.path, r.subsystem) e;
    ELSE
        INSERT INTO pghacker.graph_changes (label, key, end_key)
        SELECT DISTINCT e.*
        FROM (VALUES (OLD.path, OLD.subsystem), (NEW.path, NEW.subsystem)) r (path, subsystem)
        CROSS JOIN LATERAL pghacker.graph_code_file_entries(r.path, r.subsystem) e;
    END IF;
    RETURN NULL;
END
$$;

-- A snapshot that becomes, or stops being, a ready snapshot of postgres
-- changes its files' standing. Deleting one deletes its files, captured above.
CREATE FUNCTION graph_capture_code_snapshots() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO pghacker.graph_changes (label, key, end_key)
    SELECT DISTINCT e.* FROM pghacker.code_files f
    CROSS JOIN LATERAL pghacker.graph_code_file_entries(f.path, f.subsystem) e
    WHERE f.snapshot_id IN (OLD.id, NEW.id);
    RETURN NULL;
END
$$;

CREATE TRIGGER graph_capture_insert AFTER INSERT ON authors REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_authors();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON authors REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_authors();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON authors FOR EACH ROW
    WHEN ((OLD.id, OLD.display_name, OLD.merged_into, OLD.message_count, OLD.first_seen, OLD.last_seen)
          IS DISTINCT FROM (NEW.id, NEW.display_name, NEW.merged_into, NEW.message_count, NEW.first_seen, NEW.last_seen))
    EXECUTE FUNCTION graph_capture_authors();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON threads REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_threads();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON threads REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_threads();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON threads FOR EACH ROW
    WHEN ((OLD.id, OLD.subject, OLD.root_message_id, OLD.message_count, OLD.started_at, OLD.last_at)
          IS DISTINCT FROM (NEW.id, NEW.subject, NEW.root_message_id, NEW.message_count, NEW.started_at, NEW.last_at))
    EXECUTE FUNCTION graph_capture_threads();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON messages REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_messages();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON messages REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_messages();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON messages FOR EACH ROW
    WHEN ((OLD.message_id, OLD.thread_id, OLD.author_id, OLD.parent_message_id, OLD.subject, OLD.sent_at, OLD.has_patch)
          IS DISTINCT FROM (NEW.message_id, NEW.thread_id, NEW.author_id, NEW.parent_message_id, NEW.subject, NEW.sent_at, NEW.has_patch))
    EXECUTE FUNCTION graph_capture_messages();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON anchors REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_anchors();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON anchors REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_anchors();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON anchors FOR EACH ROW
    WHEN ((OLD.message_id, OLD.kind, OLD.value) IS DISTINCT FROM (NEW.message_id, NEW.kind, NEW.value))
    EXECUTE FUNCTION graph_capture_anchors();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON features REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_features();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON features REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_features();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON features FOR EACH ROW
    WHEN ((OLD.slug, OLD.name, OLD.status, OLD.release, OLD.merged_into)
          IS DISTINCT FROM (NEW.slug, NEW.name, NEW.status, NEW.release, NEW.merged_into))
    EXECUTE FUNCTION graph_capture_features();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON decisions REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_decisions();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON decisions REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_decisions();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON decisions FOR EACH ROW
    WHEN ((OLD.id, OLD.feature_id, OLD.thread_id, OLD.title, OLD.outcome, OLD.decided_at, OLD.merged_into)
          IS DISTINCT FROM (NEW.id, NEW.feature_id, NEW.thread_id, NEW.title, NEW.outcome, NEW.decided_at, NEW.merged_into))
    EXECUTE FUNCTION graph_capture_decisions();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON positions REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_positions();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON positions REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_positions();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON positions FOR EACH ROW
    WHEN ((OLD.id, OLD.author_id, OLD.thread_id, OLD.feature_id, OLD.decision_id, OLD.stance, OLD.summary)
          IS DISTINCT FROM (NEW.id, NEW.author_id, NEW.thread_id, NEW.feature_id, NEW.decision_id, NEW.stance, NEW.summary))
    EXECUTE FUNCTION graph_capture_positions();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON citations REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_citations();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON citations REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_citations();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON citations FOR EACH ROW
    WHEN ((OLD.extract_kind, OLD.extract_id, OLD.message_id) IS DISTINCT FROM (NEW.extract_kind, NEW.extract_id, NEW.message_id))
    EXECUTE FUNCTION graph_capture_citations();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON feature_threads REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_feature_threads();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON feature_threads REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_feature_threads();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON feature_threads FOR EACH ROW
    WHEN ((OLD.feature_id, OLD.thread_id) IS DISTINCT FROM (NEW.feature_id, NEW.thread_id))
    EXECUTE FUNCTION graph_capture_feature_threads();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON feature_files REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_feature_files();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON feature_files REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_feature_files();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON feature_files FOR EACH ROW
    WHEN ((OLD.feature_id, OLD.path) IS DISTINCT FROM (NEW.feature_id, NEW.path))
    EXECUTE FUNCTION graph_capture_feature_files();

CREATE TRIGGER graph_capture_insert AFTER INSERT ON code_files REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_code_files();
CREATE TRIGGER graph_capture_delete AFTER DELETE ON code_files REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_code_files();
CREATE TRIGGER graph_capture_update AFTER UPDATE ON code_files FOR EACH ROW
    WHEN ((OLD.snapshot_id, OLD.path, OLD.subsystem) IS DISTINCT FROM (NEW.snapshot_id, NEW.path, NEW.subsystem))
    EXECUTE FUNCTION graph_capture_code_files();

CREATE TRIGGER graph_capture_update AFTER UPDATE ON code_snapshots FOR EACH ROW
    WHEN (OLD.id IS DISTINCT FROM NEW.id
          OR (OLD.state = 'ready' AND OLD.repo = 'postgres') IS DISTINCT FROM (NEW.state = 'ready' AND NEW.repo = 'postgres'))
    EXECUTE FUNCTION graph_capture_code_snapshots();

DO $$
DECLARE
    tab text;
BEGIN
    FOREACH tab IN ARRAY ARRAY[
        'authors', 'threads', 'messages', 'anchors', 'features', 'decisions', 'positions', 'citations',
        'feature_threads', 'feature_files', 'code_files', 'code_snapshots']
    LOOP
        EXECUTE format('CREATE TRIGGER graph_capture_truncate AFTER TRUNCATE ON %I
                        FOR EACH STATEMENT EXECUTE FUNCTION graph_capture_truncate()', tab);
    END LOOP;
END
$$;

-- What the sync looks up by: paths and subsystems of files, files and
-- releases of features, positions by what they're about. Small tables.
CREATE INDEX code_files_path ON code_files (path);
CREATE INDEX code_files_subsystem ON code_files (subsystem);
CREATE INDEX feature_files_path ON feature_files (path);
CREATE INDEX features_release ON features (release);
CREATE INDEX positions_feature ON positions (feature_id);
CREATE INDEX positions_decision ON positions (decision_id);
