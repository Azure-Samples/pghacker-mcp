-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

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

-- Prewarming shared_buffers is optional: an existing Azure cluster whose
-- azure.extensions hasn't been updated yet skips creating the extension.
DO $$
DECLARE
    fn regprocedure;
BEGIN
    CREATE EXTENSION IF NOT EXISTS pg_prewarm;
    FOR fn IN
        SELECT p.oid::regprocedure
        FROM pg_proc p
        JOIN pg_depend d ON d.objid = p.oid AND d.classid = 'pg_proc'::regclass
        JOIN pg_extension e ON e.oid = d.refobjid
        WHERE e.extname = 'pg_prewarm'
    LOOP
        EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC, pgh_reader', fn);
    END LOOP;
EXCEPTION WHEN feature_not_supported OR insufficient_privilege OR invalid_parameter_value OR undefined_file THEN
    NULL;
END
$$;

-- Filtered vector searches keep scanning the HNSW graph until LIMIT is filled,
-- in relaxed distance order so closer matches found past the initial beam
-- aren't discarded. An administrator that isn't a superuser, as in Azure, may
-- set pgvector's parameters only once its library is loaded: the cast loads
-- it. Settings are written only when they differ, since they're shared by
-- every database.
DO $$
DECLARE
    wanted text[] := ARRAY[
        ['pgh_reader', 'hnsw.iterative_scan', 'relaxed_order'],
        ['pgh_reader', 'hnsw.ef_search', '100'],
        ['pgh_reader', 'hnsw.max_scan_tuples', '5000'],
        ['pgh_indexer', 'hnsw.iterative_scan', 'relaxed_order']];
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

-- Prewarms one relation of pghacker or pghacker_graph (a table, its TOAST
-- table, or an index of either), main, visibility-map and free-space-map
-- forks, into shared_buffers. SECURITY DEFINER so that pgh_indexer can also
-- prewarm pghacker_graph's indexes and TOAST tables, which the administrator
-- owns. Returns NULL when pg_prewarm isn't installed, or 0 if the relation
-- was dropped meanwhile.
CREATE OR REPLACE FUNCTION pghacker.prewarm_relation(target oid)
RETURNS bigint
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    ext_schema text;
    blocks bigint := 0;
    fork_blocks bigint;
BEGIN
    SELECT n.nspname INTO ext_schema
    FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
    WHERE e.extname = 'pg_prewarm';
    IF ext_schema IS NULL THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (
        SELECT FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE c.oid = target
          AND c.relkind IN ('r', 'm', 'i', 't')
          AND (
              n.nspname IN ('pghacker', 'pghacker_graph')
              OR c.oid IN (
                  SELECT b.reltoastrelid
                  FROM pg_class b JOIN pg_namespace bn ON bn.oid = b.relnamespace
                  WHERE bn.nspname IN ('pghacker', 'pghacker_graph') AND b.reltoastrelid <> 0
              )
              OR c.oid IN (
                  SELECT i.indexrelid
                  FROM pg_class b
                  JOIN pg_namespace bn ON bn.oid = b.relnamespace
                  JOIN pg_index i ON i.indrelid = b.reltoastrelid
                  WHERE bn.nspname IN ('pghacker', 'pghacker_graph') AND b.reltoastrelid <> 0
              )
          )
    ) THEN
        IF NOT EXISTS (SELECT FROM pg_class WHERE oid = target) THEN
            RETURN 0;
        END IF;
        RAISE EXCEPTION 'cannot prewarm %', target::regclass
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF pg_relation_size(target, 'main') > 0 THEN
        EXECUTE format('SELECT %I.pg_prewarm($1::regclass, ''buffer'', ''main'')', ext_schema)
            INTO fork_blocks USING target;
        blocks := blocks + coalesce(fork_blocks, 0);
    END IF;
    IF pg_relation_size(target, 'vm') > 0 THEN
        EXECUTE format('SELECT %I.pg_prewarm($1::regclass, ''buffer'', ''vm'')', ext_schema)
            INTO fork_blocks USING target;
        blocks := blocks + coalesce(fork_blocks, 0);
    END IF;
    IF pg_relation_size(target, 'fsm') > 0 THEN
        EXECUTE format('SELECT %I.pg_prewarm($1::regclass, ''buffer'', ''fsm'')', ext_schema)
            INTO fork_blocks USING target;
        blocks := blocks + coalesce(fork_blocks, 0);
    END IF;
    RETURN blocks;
EXCEPTION WHEN undefined_table OR undefined_object THEN
    RETURN 0;
END
$$;

REVOKE ALL ON FUNCTION pghacker.prewarm_relation(oid) FROM PUBLIC, pgh_reader;
GRANT EXECUTE ON FUNCTION pghacker.prewarm_relation(oid) TO pgh_indexer;
