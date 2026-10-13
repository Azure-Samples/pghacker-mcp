-- Run by the administrator in the target database after roles.sql, before
-- the migrations and on every start: idempotent. Creates the extensions,
-- the roles' settings of extension parameters, schema pghacker (owned by
-- pgh_indexer), the AGE graph with all its labels (AGE requires these
-- privileges), and the grants.

DO $$
BEGIN
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO pgh_indexer, pgh_reader', current_database());
END
$$;

CREATE EXTENSION IF NOT EXISTS age;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS pg_textsearch;

-- Filtered vector searches keep scanning the HNSW graph until LIMIT is filled.
-- An administrator that isn't a superuser, as in Azure, may set pgvector's
-- parameters only once its library is loaded: the cast loads it. Settings are
-- written only when they differ, since they're shared by every database.
DO $$
DECLARE
    wanted text[] := ARRAY[
        ['pgh_reader', 'hnsw.iterative_scan', 'strict_order'],
        ['pgh_reader', 'hnsw.ef_search', '100'],
        ['pgh_indexer', 'hnsw.iterative_scan', 'strict_order']];
BEGIN
    EXECUTE format('SELECT %L::%s.vector', '[0]',
        (SELECT extnamespace::regnamespace::text FROM pg_extension WHERE extname = 'vector'));
    FOR i IN 1 .. array_length(wanted, 1) LOOP
        IF NOT EXISTS (
            SELECT FROM pg_db_role_setting
            WHERE setrole = wanted[i][1]::regrole AND setdatabase = 0
              AND format('%s=%s', wanted[i][2], wanted[i][3]) = ANY (setconfig)
        ) THEN
            EXECUTE format('ALTER ROLE %I SET %s = %L', wanted[i][1], wanted[i][2], wanted[i][3]);
        END IF;
    END LOOP;
END
$$;

-- PilotSwarm's own role, when it has one, creates its schemas in this
-- database and nothing else: it can't read schema pghacker.
DO $$
BEGIN
    IF EXISTS (SELECT FROM pg_roles WHERE rolname = 'pgh_swarm') THEN
        EXECUTE format('GRANT CONNECT, CREATE, TEMPORARY ON DATABASE %I TO pgh_swarm', current_database());
    END IF;
END
$$;

CREATE SCHEMA IF NOT EXISTS pghacker AUTHORIZATION pgh_indexer;
GRANT USAGE ON SCHEMA pghacker TO pgh_reader;
-- Tables the indexer creates are readable unless a migration revokes them.
ALTER DEFAULT PRIVILEGES FOR ROLE pgh_indexer IN SCHEMA pghacker GRANT SELECT ON TABLES TO pgh_reader;

SET LOCAL search_path = ag_catalog, public;
-- An administrator that doesn't own AGE's catalog, as in Azure, may be unable
-- to grant this; it's enough that the roles have it.
DO $$
DECLARE
    member text;
BEGIN
    BEGIN
        GRANT USAGE ON SCHEMA ag_catalog TO pgh_indexer, pgh_reader;
        GRANT SELECT ON ag_catalog.ag_graph, ag_catalog.ag_label TO pgh_indexer, pgh_reader;
    EXCEPTION WHEN insufficient_privilege THEN
        FOREACH member IN ARRAY ARRAY['pgh_indexer', 'pgh_reader'] LOOP
            IF NOT has_schema_privilege(member, 'ag_catalog', 'USAGE')
               OR NOT has_table_privilege(member, 'ag_catalog.ag_graph', 'SELECT')
               OR NOT has_table_privilege(member, 'ag_catalog.ag_label', 'SELECT') THEN
                RAISE;
            END IF;
        END LOOP;
    END;
END
$$;

DO $$
DECLARE
    label text;
    graph_oid oid;
    new_edges jsonb := '[]'::jsonb;
BEGIN
    IF NOT EXISTS (SELECT FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph') THEN
        PERFORM ag_catalog.create_graph('pghacker_graph');
    END IF;
    SELECT graphid INTO graph_oid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph';
    FOR label IN SELECT jsonb_array_elements_text(current_setting('pghacker.vertex_labels')::jsonb)
    LOOP
        IF NOT EXISTS (SELECT FROM ag_catalog.ag_label WHERE graph = graph_oid AND name = label) THEN
            PERFORM ag_catalog.create_vlabel('pghacker_graph'::cstring, label::cstring);
        END IF;
    END LOOP;
    FOR label IN SELECT jsonb_array_elements_text(current_setting('pghacker.edge_labels')::jsonb)
    LOOP
        IF NOT EXISTS (SELECT FROM ag_catalog.ag_label WHERE graph = graph_oid AND name = label) THEN
            PERFORM ag_catalog.create_elabel('pghacker_graph'::cstring, label::cstring);
            new_edges := new_edges || jsonb_build_array(label);
        END IF;
    END LOOP;
    PERFORM set_config('pghacker.new_edge_labels', new_edges::text, true);
END
$$;

GRANT USAGE ON SCHEMA pghacker_graph TO pgh_indexer, pgh_reader;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA pghacker_graph TO pgh_indexer;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA pghacker_graph TO pgh_indexer;
GRANT SELECT ON ALL TABLES IN SCHEMA pghacker_graph TO pgh_reader;
