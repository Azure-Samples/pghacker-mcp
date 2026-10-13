-- Populated databases must prepare these concurrently before upgrading.
DO $$
DECLARE
    wanted record;
    keys smallint[];
    existing oid;
    named oid;
    definition_ok boolean;
    usable boolean;
    index_usable boolean;
BEGIN
    FOR wanted IN SELECT * FROM (VALUES
        ('graph_edges_start_key_idx', ARRAY['start_key', 'label', 'end_key']::text[]),
        ('graph_edges_end_key_idx', ARRAY['end_key', 'label', 'start_key']::text[])
    ) AS targets(name, columns)
    LOOP
        SELECT array_agg(a.attnum ORDER BY k.pos) INTO keys
        FROM unnest(wanted.columns) WITH ORDINALITY k(name, pos)
        JOIN pg_attribute a ON a.attrelid = 'pghacker.graph_edges'::regclass
            AND a.attname = k.name AND a.attnum > 0 AND NOT a.attisdropped;

        named := to_regclass(format('pghacker.%I', wanted.name));
        usable := false;
        FOR existing IN SELECT i.indexrelid FROM pg_index i
            WHERE i.indrelid = 'pghacker.graph_edges'::regclass
        LOOP
            SELECT am.amname = 'btree' AND i.indpred IS NULL
                AND i.indnkeyatts >= 3
                AND ARRAY(SELECT k.attnum FROM unnest(i.indkey::smallint[])
                    WITH ORDINALITY k(attnum, pos) WHERE k.pos <= 3 ORDER BY k.pos) = keys
                AND NOT EXISTS (
                    SELECT FROM unnest(i.indkey::smallint[], i.indclass::oid[],
                                       i.indcollation::oid[], i.indoption::smallint[])
                        WITH ORDINALITY k(attnum, opclass_oid, collation_oid, options, pos)
                    JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                    JOIN pg_opclass o ON o.oid = k.opclass_oid
                    WHERE k.pos <= 3 AND (NOT o.opcdefault OR o.opcintype <> a.atttypid
                        OR k.collation_oid <> a.attcollation OR k.options <> 0)),
                i.indisvalid AND i.indisready AND i.indislive
            INTO definition_ok, index_usable
            FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
            JOIN pg_am am ON am.oid = c.relam WHERE i.indexrelid = existing;

            IF existing = named AND NOT definition_ok THEN
                RAISE EXCEPTION 'graph index % has the wrong definition; inspect scripts/graph-indexes.mjs --plan',
                    wanted.name USING ERRCODE = '55000';
            END IF;
            IF definition_ok AND NOT index_usable THEN
                RAISE EXCEPTION 'graph index % is invalid or unready; approved operator recovery is required',
                    existing::regclass USING ERRCODE = '55000';
            END IF;
            usable := usable OR (definition_ok AND index_usable);
        END LOOP;

        IF named IS NOT NULL AND NOT EXISTS (
            SELECT FROM pg_index WHERE indexrelid = named
                AND indrelid = 'pghacker.graph_edges'::regclass
        ) THEN
            RAISE EXCEPTION 'graph index name % belongs to another object', wanted.name
                USING ERRCODE = '55000';
        END IF;
        IF usable THEN CONTINUE; END IF;
        IF EXISTS (SELECT FROM ONLY pghacker.graph_edges LIMIT 1) THEN
            RAISE EXCEPTION 'populated graph_edges requires approved concurrent preparation: scripts/graph-indexes.mjs --plan'
                USING ERRCODE = '55000';
        END IF;
        BEGIN
            LOCK TABLE ONLY pghacker.graph_edges IN SHARE MODE NOWAIT;
        EXCEPTION WHEN lock_not_available THEN
            RAISE EXCEPTION 'graph_edges cannot be locked for empty-table setup; use approved concurrent preparation'
                USING ERRCODE = '55000';
        END;
        IF EXISTS (SELECT FROM ONLY pghacker.graph_edges LIMIT 1) THEN
            RAISE EXCEPTION 'graph_edges was populated during setup; use approved concurrent preparation'
                USING ERRCODE = '55000';
        END IF;
        EXECUTE format('CREATE INDEX %I ON pghacker.graph_edges USING btree (%s)',
            wanted.name, (SELECT string_agg(format('%I', c), ', ') FROM unnest(wanted.columns) c));
    END LOOP;
END
$$;
