-- Run by the administrator in the target database after roles.sql, before
-- the migrations and on every start: idempotent. Creates the extensions,
-- schema pghacker (owned by pgh_indexer), the AGE graph with all its labels
-- (AGE requires these privileges), and the grants.

DO $$
BEGIN
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO pgh_indexer, pgh_reader', current_database());
END
$$;

CREATE EXTENSION IF NOT EXISTS age;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS pg_textsearch;

CREATE SCHEMA IF NOT EXISTS pghacker AUTHORIZATION pgh_indexer;
GRANT USAGE ON SCHEMA pghacker TO pgh_reader;
-- Tables the indexer creates are readable unless a migration revokes them.
ALTER DEFAULT PRIVILEGES FOR ROLE pgh_indexer IN SCHEMA pghacker GRANT SELECT ON TABLES TO pgh_reader;

SET LOCAL search_path = ag_catalog, public;
GRANT USAGE ON SCHEMA ag_catalog TO pgh_indexer, pgh_reader;
GRANT SELECT ON ag_catalog.ag_graph, ag_catalog.ag_label TO pgh_indexer, pgh_reader;

DO $$
DECLARE
    label text;
    graph_oid oid;
BEGIN
    IF NOT EXISTS (SELECT FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph') THEN
        PERFORM ag_catalog.create_graph('pghacker_graph');
    END IF;
    SELECT graphid INTO graph_oid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph';
    FOREACH label IN ARRAY ARRAY[
        'Author', 'Thread', 'Message', 'Feature', 'Decision', 'Position', 'Release',
        'Subsystem', 'File', 'Commit', 'CommitFestEntry']
    LOOP
        IF NOT EXISTS (SELECT FROM ag_catalog.ag_label WHERE graph = graph_oid AND name = label) THEN
            PERFORM ag_catalog.create_vlabel('pghacker_graph'::cstring, label::cstring);
        END IF;
    END LOOP;
    FOREACH label IN ARRAY ARRAY[
        'AUTHORED', 'REPLIES_TO', 'IN_THREAD', 'MENTIONS', 'PART_OF', 'DISCUSSES', 'DECIDES',
        'TARGETS', 'IMPLEMENTS', 'HOLDS', 'ABOUT', 'EVIDENCED_BY', 'MODIFIES', 'DISCUSSED_IN']
    LOOP
        IF NOT EXISTS (SELECT FROM ag_catalog.ag_label WHERE graph = graph_oid AND name = label) THEN
            PERFORM ag_catalog.create_elabel('pghacker_graph'::cstring, label::cstring);
        END IF;
    END LOOP;
END
$$;

GRANT USAGE ON SCHEMA pghacker_graph TO pgh_indexer, pgh_reader;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA pghacker_graph TO pgh_indexer;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA pghacker_graph TO pgh_indexer;
GRANT SELECT ON ALL TABLES IN SCHEMA pghacker_graph TO pgh_reader;
