-- Copyright (c) Microsoft Corporation.
-- Licensed under the MIT license.

WITH labels AS (
    SELECT l.name::text AS label, l.kind::text AS label_kind, c.oid AS table_oid,
           n.nspname::text AS schema_name, c.relname::text AS table_name,
           pg_get_userbyid(c.relowner) AS owner, c.relkind::text AS relkind,
           c.relispartition, c.reltuples::bigint AS estimated_rows
    FROM ag_catalog.ag_label l
    JOIN ag_catalog.ag_graph g ON g.graphid = l.graph
    JOIN pg_class c ON c.oid = l.relation
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE g.name = 'pghacker_graph'
      AND l.name NOT IN ('_ag_label_edge', '_ag_label_vertex')
), requirements AS (
    SELECT CASE WHEN label_kind = 'e' THEN 'edge_id' ELSE 'vertex_id' END AS kind,
           label, schema_name, table_name, table_oid, owner, relkind,
           relispartition, estimated_rows, ARRAY['id']::text[] AS columns,
           CASE WHEN label_kind = 'v' THEN label || '_pkey'
                WHEN octet_length(label || '_id_idx') <= 63 THEN label || '_id_idx'
                ELSE 'pgh_eid_' || md5(label) END AS index_name,
           label_kind = 'e' AS buildable
    FROM labels
    UNION ALL
    SELECT 'edge_endpoint', l.label, l.schema_name, l.table_name, l.table_oid,
           l.owner, l.relkind, l.relispartition, l.estimated_rows,
           e.columns, e.index_name, false
    FROM labels l
    CROSS JOIN LATERAL (VALUES
        (ARRAY['start_id']::text[], l.label || '_start_id_idx'),
        (ARRAY['end_id']::text[], l.label || '_end_id_idx')
    ) e(columns, index_name)
    WHERE l.label_kind = 'e'
    UNION ALL
    SELECT 'mapping_endpoint', NULL, 'pghacker', 'graph_edges', c.oid,
           pg_get_userbyid(c.relowner), c.relkind::text, c.relispartition,
           c.reltuples::bigint, e.columns, e.index_name, true
    FROM (VALUES
        (ARRAY['start_key', 'label', 'end_key']::text[], 'graph_edges_start_key_idx'),
        (ARRAY['end_key', 'label', 'start_key']::text[], 'graph_edges_end_key_idx')
    ) e(columns, index_name)
    LEFT JOIN pg_namespace n ON n.nspname = 'pghacker'
    LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = 'graph_edges'
), inspected AS (
    SELECT r.*, ix.indexes, ix.usable_indexes, ix.signature_oids, ix.invalid,
           named.oid AS named_oid, named.relkind AS named_kind,
           ni.indrelid AS named_table_oid,
           (SELECT count(*) FROM pg_inherits h WHERE h.inhparent = r.table_oid) AS children,
           (SELECT coalesce(jsonb_agg(jsonb_build_object(
               'table', format('%I.%I', pn.nspname, pc.relname),
               'name', pi.relname, 'definition', pg_get_indexdef(p.indexrelid),
               'valid', p.indisvalid, 'ready', p.indisready, 'live', p.indislive)
               ORDER BY pc.relname, pi.relname), '[]'::jsonb)
            FROM pg_inherits h
            JOIN pg_class pc ON pc.oid = h.inhparent
            JOIN pg_namespace pn ON pn.oid = pc.relnamespace
            JOIN pg_index p ON p.indrelid = pc.oid
            JOIN pg_class pi ON pi.oid = p.indexrelid
            WHERE h.inhrelid = r.table_oid) AS parent_indexes,
           NOT EXISTS (
               SELECT FROM unnest(r.columns) AS col(name)
               LEFT JOIN pg_attribute a ON a.attrelid = r.table_oid
                   AND a.attname = col.name AND a.attnum > 0 AND NOT a.attisdropped
               WHERE a.attnum IS NULL OR NOT a.attnotnull
                  OR a.atttypid <> CASE WHEN r.kind = 'mapping_endpoint'
                      THEN 'text'::regtype ELSE 'ag_catalog.graphid'::regtype END
           ) AS native_columns
    FROM requirements r
    LEFT JOIN pg_namespace ns ON ns.nspname = r.schema_name
    LEFT JOIN pg_class named ON named.relnamespace = ns.oid AND named.relname = r.index_name
    LEFT JOIN pg_index ni ON ni.indexrelid = named.oid
    LEFT JOIN LATERAL (
        SELECT coalesce(jsonb_agg(f.metadata ORDER BY f.name), '[]'::jsonb) AS indexes,
               coalesce(jsonb_agg(f.name ORDER BY f.name) FILTER (
                   WHERE f.matches AND f.valid AND f.ready AND f.live), '[]'::jsonb) AS usable_indexes,
               coalesce(array_agg(f.oid) FILTER (WHERE f.matches), ARRAY[]::oid[]) AS signature_oids,
               coalesce(bool_or(f.matches AND NOT (f.valid AND f.ready AND f.live)), false) AS invalid
        FROM (
            SELECT ic.oid, ic.relname::text AS name, i.indisvalid AS valid,
                   i.indisready AS ready, i.indislive AS live,
                   am.amname = 'btree' AND i.indpred IS NULL
                   AND i.indnkeyatts >= cardinality(r.columns)
                   AND ARRAY(
                       SELECT a.attname::text
                       FROM unnest(i.indkey::smallint[]) WITH ORDINALITY k(attnum, pos)
                       JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                       WHERE k.pos <= cardinality(r.columns) ORDER BY k.pos
                   ) = r.columns
                   AND NOT EXISTS (
                       SELECT FROM unnest(i.indkey::smallint[], i.indclass::oid[],
                                          i.indcollation::oid[], i.indoption::smallint[])
                           WITH ORDINALITY k(attnum, opclass_oid, collation_oid, options, pos)
                       JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                       JOIN pg_opclass o ON o.oid = k.opclass_oid
                       WHERE k.pos <= cardinality(r.columns)
                         AND (NOT o.opcdefault OR o.opcintype <> a.atttypid
                              OR k.collation_oid <> a.attcollation OR k.options <> 0)
                   ) AS matches,
                   jsonb_build_object(
                       'oid', ic.oid::bigint, 'name', ic.relname, 'definition', pg_get_indexdef(i.indexrelid),
                       'valid', i.indisvalid, 'ready', i.indisready, 'live', i.indislive,
                       'unique', i.indisunique, 'primary', i.indisprimary,
                       'check_xmin', i.indcheckxmin, 'method', am.amname,
                       'owner', pg_get_userbyid(ic.relowner),
                       'signature', md5(jsonb_build_object(
                           'method', am.oid::bigint, 'keys', i.indkey::text,
                           'classes', i.indclass::text, 'collations', i.indcollation::text,
                           'options', i.indoption::text, 'key_count', i.indnkeyatts,
                           'unique', i.indisunique, 'nulls_not_distinct', i.indnullsnotdistinct,
                           'predicate', pg_get_expr(i.indpred, i.indrelid),
                           'expressions', pg_get_expr(i.indexprs, i.indrelid))::text),
                       'columns', ARRAY(
                           SELECT a.attname::text
                           FROM unnest(i.indkey::smallint[]) WITH ORDINALITY k(attnum, pos)
                           LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
                           ORDER BY k.pos)) AS metadata
            FROM pg_index i
            JOIN pg_class ic ON ic.oid = i.indexrelid
            JOIN pg_am am ON am.oid = ic.relam
            WHERE i.indrelid = r.table_oid
        ) f
    ) ix ON true
), classified AS (
    SELECT *, CASE
        WHEN table_oid IS NULL THEN 'missing_relation'
        WHEN relkind <> 'r' OR relispartition OR children <> 0 OR NOT native_columns
          OR (kind <> 'mapping_endpoint' AND (schema_name <> 'pghacker_graph' OR table_name <> label))
          OR (kind = 'mapping_endpoint' AND owner <> 'pgh_indexer') THEN 'unsupported'
        WHEN named_oid IS NOT NULL AND
             (named_kind <> 'i' OR named_table_oid IS DISTINCT FROM table_oid
              OR NOT named_oid = ANY(signature_oids)) THEN 'name_conflict'
        WHEN invalid THEN 'invalid'
        WHEN jsonb_array_length(usable_indexes) > 0 THEN 'ready'
        ELSE 'missing' END AS state
    FROM inspected
)
SELECT coalesce(jsonb_agg(jsonb_build_object(
    'kind', kind, 'label', label, 'schema', schema_name, 'table', table_name,
    'table_oid', table_oid::bigint, 'owner', owner, 'estimated_rows', estimated_rows,
    'columns', columns, 'index_name', index_name, 'buildable', buildable,
    'state', state, 'usable_indexes', usable_indexes, 'indexes', indexes,
    'parent_indexes', parent_indexes, 'children', children,
    'named_oid', named_oid::bigint) ORDER BY schema_name, table_name, kind, index_name), '[]'::jsonb)
FROM classified
