-- The cluster's roles, run by the administrator before database.sql and
-- on every start: idempotent. The passwords arrive as the settings
-- pghacker.indexer_password, pghacker.reader_password and, when PilotSwarm
-- gets a role of its own (in Azure), pghacker.swarm_password. Settings of
-- extensions' parameters are in database.sql, after the extensions.

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
    IF coalesce(current_setting('pghacker.swarm_password', true), '') <> '' THEN
        BEGIN
            CREATE ROLE pgh_swarm LOGIN;
        EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
        END;
        EXECUTE format('ALTER ROLE pgh_swarm PASSWORD %L', current_setting('pghacker.swarm_password'));
    END IF;
END
$$;

-- An administrator that isn't a superuser, as in Azure, must act as
-- pgh_indexer to give it schema pghacker and set its default privileges.
DO $$
BEGIN
    IF NOT pg_has_role(current_user, 'pgh_indexer', 'USAGE') THEN
        EXECUTE format('GRANT pgh_indexer TO %I WITH INHERIT TRUE, SET TRUE', current_user);
    END IF;
END
$$;

ALTER ROLE pgh_indexer SET search_path = pghacker, ag_catalog, public;
ALTER ROLE pgh_reader SET search_path = pghacker, ag_catalog, public;
-- The reader serves the public MCP server: read-only, with a 10-second default
-- statement timeout. Code searches temporarily set their own time budgets.
ALTER ROLE pgh_reader SET default_transaction_read_only = on;
ALTER ROLE pgh_reader SET statement_timeout = '10s';
ALTER ROLE pgh_reader SET idle_in_transaction_session_timeout = '30s';
ALTER ROLE pgh_reader CONNECTION LIMIT 64;
