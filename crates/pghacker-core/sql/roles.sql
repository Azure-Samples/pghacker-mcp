-- The cluster's two roles, run by the administrator before database.sql and
-- on every start: idempotent. The passwords arrive as the settings
-- pghacker.indexer_password and pghacker.reader_password.

DO $$
BEGIN
    BEGIN
        CREATE ROLE pgh_indexer LOGIN;
    EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
    END;
    BEGIN
        CREATE ROLE pgh_reader LOGIN;
    EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
    END;
    EXECUTE format('ALTER ROLE pgh_indexer PASSWORD %L', current_setting('pghacker.indexer_password'));
    EXECUTE format('ALTER ROLE pgh_reader PASSWORD %L', current_setting('pghacker.reader_password'));
END
$$;

ALTER ROLE pgh_indexer SET search_path = pghacker, ag_catalog, public;
ALTER ROLE pgh_reader SET search_path = pghacker, ag_catalog, public;
-- The reader serves the public MCP server: read-only, with a 5-second limit.
ALTER ROLE pgh_reader SET default_transaction_read_only = on;
ALTER ROLE pgh_reader SET statement_timeout = '5s';
ALTER ROLE pgh_reader SET idle_in_transaction_session_timeout = '30s';
ALTER ROLE pgh_reader CONNECTION LIMIT 64;
-- Filtered vector searches keep scanning the HNSW graph until LIMIT is filled.
ALTER ROLE pgh_reader SET hnsw.iterative_scan = 'strict_order';
ALTER ROLE pgh_reader SET hnsw.ef_search = 100;
ALTER ROLE pgh_indexer SET hnsw.iterative_scan = 'strict_order';
