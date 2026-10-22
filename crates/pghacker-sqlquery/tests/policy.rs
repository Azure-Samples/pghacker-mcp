// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Query policy without a database, including the PostgreSQL/AGE fallback.

use pghacker_sqlquery::{Checked, Fallback, check};

fn refused(sql: &str, expected: &str) {
    let reason = check(sql).expect_err(sql);
    assert!(reason.contains(expected), "{sql}: {reason}");
}

/// Checks that `sql` passes, and says how.
fn allowed(sql: &str) -> Checked {
    check(sql).unwrap_or_else(|reason| panic!("{sql}: {reason}"))
}

fn graph(text: &str) -> String {
    format!("SELECT * FROM cypher('pghacker_graph', $query${text}$query$) AS (result agtype)")
}

#[test]
fn settings_and_identity_changes_have_specific_errors() {
    for sql in [
        "SET work_mem = '1MB'",
        "SET LOCAL statement_timeout = 0",
        "SET SESSION search_path = public",
        "SET TIME ZONE 'UTC'",
        "SET ROLE pgh_indexer",
        "SET SESSION AUTHORIZATION DEFAULT",
        "RESET ALL",
        "RESET work_mem",
        "RESET ROLE",
        "RESET SESSION AUTHORIZATION",
        "ALTER SYSTEM SET statement_timeout = 0",
        "ALTER SYSTEM RESET ALL",
        "EXPLAIN (ANALYZE, BUFFERS) SET search_path = public",
    ] {
        refused(sql, "settings");
        refused(sql, "SHOW");
    }
    for name in ["set_config", "setseed", "set_limit"] {
        refused(&format!("SELECT {name}()"), "changes session settings");
    }
}

#[test]
fn dangerous_function_families_are_refused() {
    for (effect, names) in [
        (
            "advisory",
            vec![
                "pg_advisory_lock",
                "pg_advisory_lock_shared",
                "pg_advisory_xact_lock",
                "pg_advisory_xact_lock_shared",
                "pg_try_advisory_lock",
                "pg_try_advisory_lock_shared",
                "pg_try_advisory_xact_lock",
                "pg_try_advisory_xact_lock_shared",
                "pg_advisory_unlock",
                "pg_advisory_unlock_shared",
                "pg_advisory_unlock_all",
            ],
        ),
        ("delays", vec!["pg_sleep", "pg_sleep_for", "pg_sleep_until"]),
        ("notification", vec!["pg_notify"]),
        ("maintenance", vec!["graph_request_full", "rethread_mail"]),
        ("coordination", vec!["pg_export_snapshot"]),
        ("sequence", vec!["nextval", "setval"]),
        (
            "server",
            vec![
                "pg_cancel_backend",
                "pg_terminate_backend",
                "pg_reload_conf",
                "pg_rotate_logfile",
                "pg_log_backend_memory_contexts",
                "pg_log_standby_snapshot",
                "pg_import_system_collations",
                "pg_extension_config_dump",
                "pg_nextoid",
                "pg_stop_making_pinned_objects",
                "binary_upgrade_set_next_pg_type_oid",
            ],
        ),
        (
            "index state",
            vec![
                "brin_desummarize_range",
                "brin_summarize_new_values",
                "brin_summarize_range",
                "gin_clean_pending_list",
                "bm25_compact",
                "bm25_compact_step",
                "bm25_compact_step_if_current",
                "bm25_cache_apply_to_tail",
                "bm25_cache_bump_spill_generation",
                "bm25_cache_cold_build",
                "bm25_cache_evict_largest",
                "bm25_force_merge",
                "bm25_spill_index",
                "bm25_test_corrupt_tombstone_head",
                "bm25_test_memtable_append",
                "bm25_test_recycle_tombstone_head",
            ],
        ),
        (
            "replication",
            vec![
                "pg_switch_wal",
                "pg_backup_start",
                "pg_backup_stop",
                "pg_create_restore_point",
                "pg_promote",
                "pg_wal_replay_pause",
                "pg_wal_replay_resume",
                "pg_replication_slot_advance",
                "pg_create_physical_replication_slot",
                "pg_create_logical_replication_slot",
                "pg_drop_replication_slot",
                "pg_copy_physical_replication_slot",
                "pg_copy_logical_replication_slot",
                "pg_sync_replication_slots",
                "pg_logical_slot_get_changes",
                "pg_logical_slot_get_binary_changes",
                "pg_logical_emit_message",
                "pg_replication_origin_create",
                "pg_replication_origin_drop",
                "pg_replication_origin_advance",
                "pg_replication_origin_session_setup",
                "pg_replication_origin_session_reset",
                "pg_replication_origin_xact_setup",
                "pg_replication_origin_xact_reset",
            ],
        ),
        (
            "statistics",
            vec![
                "pg_stat_reset",
                "pg_stat_reset_shared",
                "pg_stat_reset_single_table_counters",
                "pg_stat_reset_single_function_counters",
                "pg_stat_reset_slru",
                "pg_stat_reset_replication_slot",
                "pg_stat_reset_subscription_stats",
                "pg_stat_clear_snapshot",
                "pg_stat_force_next_flush",
            ],
        ),
        (
            "files",
            vec![
                "pg_read_file",
                "pg_read_binary_file",
                "pg_stat_file",
                "pg_ls_dir",
                "pg_ls_logdir",
                "pg_ls_waldir",
                "pg_ls_tmpdir",
                "pg_file_write",
                "pg_file_rename",
                "pg_file_unlink",
                "pg_file_sync",
                "lo_create",
                "lo_import",
                "lo_export",
                "lo_unlink",
                "lo_get",
                "lo_put",
                "lo_open",
                "lo_close",
                "lo_lseek",
                "lo_lseek64",
                "lo_tell",
                "lo_tell64",
                "lo_truncate",
                "lo_truncate64",
                "loread",
                "lowrite",
                "loopen",
                "loclose",
            ],
        ),
        (
            "external",
            vec![
                "dblink",
                "dblink_exec",
                "dblink_connect",
                "dblink_connect_u",
                "dblink_disconnect",
                "dblink_send_query",
                "postgres_fdw_get_connections",
                "postgres_fdw_disconnect",
                "postgres_fdw_disconnect_all",
            ],
        ),
        (
            "SQL supplied as text",
            vec![
                "query_to_xml",
                "query_to_xmlschema",
                "query_to_xml_and_xmlschema",
                "ts_stat",
                "ts_rewrite",
            ],
        ),
        (
            "relations named in text",
            vec![
                "table_to_xml",
                "table_to_xmlschema",
                "table_to_xml_and_xmlschema",
                "schema_to_xml",
                "schema_to_xml_and_xmlschema",
                "database_to_xml",
                "database_to_xml_and_xmlschema",
            ],
        ),
        (
            "query text that query can't check",
            vec!["age_prepare_cypher"],
        ),
        (
            "AGE",
            vec![
                "create_graph",
                "alter_graph",
                "drop_graph",
                "create_vlabel",
                "create_elabel",
                "drop_label",
                "load_labels_from_file",
                "load_edges_from_file",
                "age_create_barbell_graph",
                "create_complete_graph",
                "_cypher_create_clause",
                "_cypher_delete_clause",
                "_cypher_merge_clause",
                "_cypher_set_clause",
            ],
        ),
    ] {
        for name in names {
            refused(&format!("SELECT {name}()"), effect);
            refused(&format!("SELECT pg_catalog.\"{name}\"()"), effect);
            refused(
                &format!("SELECT {name}() FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)"),
                effect,
            );
        }
    }
}

#[test]
fn calls_are_found_in_every_query_position() {
    for sql in [
        "SELECT PG_CATALOG.PG_ADVISORY_LOCK(1)",
        "SELECT \"pg_catalog\".\"pg_advisory_lock\"(1, 2)",
        "SELECT (((pg_advisory_lock(1::bigint))))::text",
        "SELECT coalesce((SELECT pg_advisory_lock(1)::text), '')",
        "VALUES (pg_advisory_lock(1))",
        "WITH r AS (SELECT pg_advisory_lock(1)) SELECT * FROM r",
        "SELECT CASE WHEN false THEN pg_advisory_lock(1)::text ELSE '' END",
        "SELECT 1 WHERE pg_advisory_lock(1) IS NULL",
        "SELECT count(*) FILTER (WHERE pg_advisory_lock(1) IS NULL) FROM threads",
        "SELECT 1 FROM threads ORDER BY pg_advisory_lock(1)::text",
        "SELECT count(*) OVER (ORDER BY pg_advisory_lock(1)::text) FROM threads",
        "SELECT * FROM pg_advisory_lock(1)",
        "SELECT * FROM LATERAL pg_advisory_lock(1) AS held",
        "SELECT * FROM pg_advisory_lock(1) AS (held integer)",
        "SELECT * FROM ROWS FROM (pg_advisory_lock(1), generate_series(1, 2))",
        "SELECT * FROM threads t JOIN authors a ON pg_advisory_lock(1) IS NULL",
        "EXPLAIN SELECT pg_advisory_lock(1)",
        "EXPLAIN (ANALYZE false, FORMAT JSON) SELECT pg_advisory_lock(1)",
        "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM ROWS FROM (pg_advisory_lock(1))",
    ] {
        refused(sql, "advisory");
    }
}

#[test]
fn unicode_quoted_names_cannot_hide_calls() {
    for sql in [
        r#"SELECT U&"pg_\0073leep"(1)"#,
        r#"SELECT pg_catalog.U&"set\005fconfig"('work_mem', '1MB', true)"#,
        r#"SELECT U&"pg_!0073leep" UESCAPE '!'(1)"#,
        r#"SELECT U&"pg_!0073leep" UESCAPE E'!'(1)"#,
        r#"SELECT U&"pg_!0073leep" UESCAPE $$!$$(1)"#,
        r#"SELECT U&"pg_\+000073leep"(1) FROM ROWS FROM (generate_series(1, 2))"#,
    ] {
        refused(sql, "query only reads");
    }
    for sql in [
        r#"SELECT U&"clock\005ftimestamp"()"#,
        r#"SELECT U&"current!005fsetting" UESCAPE '!'('work_mem')"#,
        r#"SELECT U&"current!005fsetting" UESCAPE E'!'('work_mem')"#,
        r#"SELECT U&"\D83D\DE00" FROM threads"#,
        "SELECT \"PG_SLEEP\"(1)",
        "SELECT \"pg_catalog.pg_sleep\"(1)",
    ] {
        allowed(sql);
    }
}

#[test]
fn deep_read_expressions_use_the_checked_fallback_without_exhausting_the_stack() {
    let max_sum = vec!["1"; (pghacker_sqlquery::MAX_SQL_BYTES - 4) / 4].join(" + ");
    let max_sql = format!("SELECT {max_sum}");
    assert_eq!(max_sql.len(), pghacker_sqlquery::MAX_SQL_BYTES);
    assert_eq!(allowed(&max_sql), Checked::Tokens(Fallback::TooDeep));
    let sum = vec!["1"; 5000].join(" + ");
    assert_eq!(
        allowed(&format!("SELECT {sum}")),
        Checked::Tokens(Fallback::TooDeep)
    );
    refused(
        &format!("SELECT {sum} + pg_sleep(1)::text::integer"),
        "delays",
    );
    let union = vec!["SELECT 1"; 1000].join(" UNION ALL ");
    assert_eq!(allowed(&union), Checked::Tokens(Fallback::TooDeep));
    refused(
        &format!("{union} UNION ALL SELECT set_config('work_mem', '1MB', true)"),
        "session settings",
    );
}

/// `head`, then as many `repeated` as fit in the longest SQL query takes,
/// then `tail` as many times as `repeated` was, then `end`.
fn longest(head: &str, repeated: &str, tail: &str, end: &str) -> String {
    let max = pghacker_sqlquery::MAX_SQL_BYTES;
    let times = (max - head.len() - end.len()) / (repeated.len() + tail.len());
    let sql = format!(
        "{head}{}{}{end}",
        repeated.repeat(times),
        tail.repeat(times)
    );
    assert!(sql.len() <= max && sql.len() + repeated.len() + tail.len() > max);
    sql
}

/// Checks `sql` on a thread with `stack` bytes of stack.
fn check_on_a_stack(sql: String, stack: usize) -> Result<Checked, String> {
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || check(&sql))
        .unwrap()
        .join()
        .unwrap()
}

/// Tokio's default thread stack size.
const TOKIO_STACK_BYTES: usize = 2 * 1024 * 1024;

#[test]
fn the_longest_chains_are_checked_by_their_tokens() {
    for (head, repeated) in [
        ("SELECT 1", "+1"),
        ("SELECT 1", " + 1"),
        ("SELECT 1", "*1"),
        ("SELECT 1", "::int"),
        ("SELECT true", " AND true"),
        ("SELECT true", " OR true"),
        ("SELECT 'a'", "||'a'"),
        ("SELECT 1", " IS NOT NULL"),
        ("SELECT 1", " IS NULL"),
        ("SELECT 1", " BETWEEN 1 AND 1"),
        ("SELECT 1", " IN (1)"),
        ("SELECT 'a'", " LIKE 'a'"),
        ("SELECT 'a'", " COLLATE \"C\""),
        ("SELECT now()", " AT TIME ZONE 'UTC'"),
        ("SELECT '{}'::jsonb", "->'a'"),
        ("SELECT '{}'::jsonb", "#>'{a}'"),
        ("SELECT ARRAY[1]", "[1]"),
        ("SELECT 1", " OPERATOR(pg_catalog.+) 1"),
        ("SELECT 1", " UNION SELECT 1"),
        ("SELECT 1", " INTERSECT SELECT 1"),
        ("SELECT 1", " EXCEPT SELECT 1"),
    ] {
        let sql = longest(head, repeated, "", "");
        assert_eq!(
            check_on_a_stack(sql, TOKIO_STACK_BYTES),
            Ok(Checked::Tokens(Fallback::TooDeep)),
            "{head} {repeated}"
        );
    }
    // cypher()'s query text, which the checks of tokens parse.
    for repeated in ["::text", "||'a'"] {
        let sql = longest(
            "SELECT * FROM cypher('pghacker_graph', 'MATCH (n) RETURN n'",
            repeated,
            "",
            ") AS (n agtype)",
        );
        let reason = check_on_a_stack(sql, TOKIO_STACK_BYTES).expect_err(repeated);
        assert!(
            reason.contains("literal query text"),
            "{repeated}: {reason}"
        );
    }
    // Short chains are still parsed and walked.
    let sum = vec!["1"; 20].join(" + ");
    assert_eq!(allowed(&format!("SELECT {sum}")), Checked::Parsed);
}

#[test]
fn the_deepest_nestings_are_checked_by_their_tokens() {
    for (repeated, end, tail) in [
        ("- ", "1", ""),
        ("NOT ", "true", ""),
        ("(", "1", ")"),
        ("abs(", "1", ")"),
        ("ARRAY[", "1", "]"),
        ("(SELECT ", "1", ")"),
        ("CASE WHEN true THEN ", "1", " END"),
    ] {
        let sql = longest("SELECT ", repeated, tail, end);
        let checked = check_on_a_stack(sql, pghacker_sqlquery::STACK_BYTES);
        assert!(
            matches!(checked, Ok(Checked::Tokens(_))),
            "{repeated}: {checked:?}"
        );
    }
}

#[test]
fn nested_writes_table_creation_and_row_locks_are_refused() {
    for (sql, reason) in [
        (
            "WITH w AS (INSERT INTO authors DEFAULT VALUES RETURNING *) SELECT * FROM w",
            "not INSERT",
        ),
        (
            "WITH w AS (UPDATE authors SET display_name = 'x' RETURNING *) SELECT * FROM w",
            "not UPDATE",
        ),
        (
            "WITH w AS (DELETE FROM authors RETURNING *) SELECT * FROM w",
            "not DELETE",
        ),
        (
            "WITH w AS MATERIALIZED (DELETE FROM authors RETURNING *) \
             SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
            "not DELETE",
        ),
        (
            "WITH w(id) AS NOT MATERIALIZED (UPDATE authors SET display_name='x' RETURNING id) \
             SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
            "not UPDATE",
        ),
        (
            "WITH a AS (SELECT 1), w AS (DELETE FROM authors RETURNING *) \
             SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
            "not DELETE",
        ),
        ("SELECT 1 INTO copied", "SELECT INTO"),
        ("SELECT 1 INTO TEMP copied", "SELECT INTO"),
        ("SELECT 1 INTO UNLOGGED copied", "SELECT INTO"),
        (
            "SELECT * INTO copied FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
            "SELECT INTO",
        ),
        (
            "EXPLAIN (ANALYZE, SERIALIZE TEXT) DELETE FROM authors",
            "not DELETE",
        ),
        (
            "EXPLAIN ANALYSE UPDATE authors SET display_name='x'",
            "not UPDATE",
        ),
    ] {
        refused(sql, reason);
    }
    for clause in [
        "FOR UPDATE",
        "FOR NO KEY UPDATE",
        "FOR SHARE",
        "FOR KEY SHARE",
        "FOR UPDATE OF t NOWAIT",
        "FOR SHARE OF t SKIP LOCKED",
    ] {
        refused(&format!("SELECT * FROM threads t {clause}"), "locks rows");
        refused(
            &format!(
                "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM (SELECT * FROM threads t {clause}) q"
            ),
            "locks rows",
        );
    }
}

#[test]
fn writing_cypher_and_its_function_calls_are_refused() {
    for text in [
        "CREATE (:Feature)",
        "MATCH (n) MERGE (m:Feature)",
        "MATCH (n) SET n.name = 'x' RETURN n",
        "MATCH (n) SET n += {name: 'x'} RETURN n",
        "MATCH (n) SET n:Feature RETURN n",
        "MATCH (n) DELETE n",
        "MATCH (n) DETACH DELETE n",
        "MATCH (n) REMOVE n.name RETURN n",
        "MATCH (n) REMOVE n:Feature RETURN n",
        "UNWIND [1] AS x CREATE (n) RETURN n",
        "CALL a_procedure()",
        "LOAD CSV FROM 'x' AS row RETURN row",
        "MATCH (n) FOREACH (x IN [1] | SET n.name = x)",
        "MATCH (n) /* /* ends here */ CREATE (m) /* tail */ RETURN n",
        r"MATCH (`n\`:Feature) CREATE (m) RETURN m",
    ] {
        refused(&graph(text), "Cypher query has");
    }
    for text in [
        "RETURN pg_sleep(1)",
        "RETURN pg_catalog.set_config('work_mem', '1MB', false)",
        "RETURN `pg_advisory_lock`(1)",
        "RETURN create_barbell_graph('g', 5, 2)",
        "RETURN `CREATE_BARBELL_GRAPH`('g', 5, 2)",
        "RETURN ag_catalog.create_complete_graph('g', 5)",
    ] {
        refused(&graph(text), "query only reads");
    }
    refused(
        "SELECT * FROM cypher('g', 'MATCH (n) ' || 'RETURN n') AS (n agtype)",
        "literal query text",
    );
    refused(
        "SELECT * FROM cypher('g', $1) AS (n agtype)",
        "literal query text",
    );
    refused(
        r"SELECT * FROM cypher('g', U&'\0043REATE (n)') AS (n agtype)",
        "CREATE clause",
    );
}

#[test]
fn writes_after_a_star_projection_are_refused() {
    for projection in [
        "WITH *",
        "RETURN *",
        "with *",
        "Return *",
        "WITH DISTINCT *",
        "WITH\n  *\n",
        "RETURN\t*  ",
        "WITH /* all */ *",
    ] {
        for (clause, name) in [
            ("CREATE (m:Feature)", "CREATE"),
            ("create (m:Feature)", "CREATE"),
            ("MERGE (m:Feature {key: 'x'})", "MERGE"),
            ("DELETE n", "DELETE"),
            ("DETACH DELETE n", "DETACH"),
            ("detach\n delete n", "DETACH"),
            ("SET n.x = 1", "SET"),
            ("Set n:Feature", "SET"),
            ("REMOVE n.x", "REMOVE"),
            ("remove n:Feature", "REMOVE"),
        ] {
            refused(
                &graph(&format!("MATCH (n) {projection}\n{clause}")),
                &format!("{name} clause"),
            );
        }
    }
    // A star that multiplies is still an expression's.
    for text in [
        "MATCH (n) RETURN n.x * create",
        "WITH 2 AS create RETURN 3 * create",
        "MATCH (n) RETURN count(*) AS set",
        "MATCH (n) WITH * RETURN n",
        "MATCH (n) WITH * WHERE n.x = 1 RETURN *",
        "MATCH (n) RETURN * ORDER BY n.x LIMIT 1",
    ] {
        allowed(&graph(text));
    }
}

#[test]
fn age_prepare_cypher_is_refused_in_sql_and_cypher() {
    for sql in [
        "SELECT age_prepare_cypher('pghacker_graph', 'CREATE (n)')",
        "SELECT ag_catalog.age_prepare_cypher('pghacker_graph', 'CREATE (n)')",
        "SELECT \"age_prepare_cypher\"('pghacker_graph', 'CREATE (n)')",
        "SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype), \
         LATERAL ag_catalog.AGE_PREPARE_CYPHER('g', 'CREATE (n)') p",
        &graph("RETURN prepare_cypher('g', 'CREATE (n)')"),
        &graph("RETURN age_prepare_cypher('g', 'CREATE (n)')"),
        &graph("RETURN ag_catalog.age_prepare_cypher('g', 'CREATE (n)')"),
    ] {
        refused(sql, "age_prepare_cypher() hands cypher() query text");
    }
}

#[test]
fn other_connections_sql_is_refused() {
    for name in [
        "pg_stat_activity",
        "pg_stat_get_activity",
        "pg_stat_get_backend_activity",
        "pg_stat_statements",
    ] {
        for sql in [
            format!("SELECT * FROM {name}"),
            format!("SELECT query FROM pg_catalog.{name}"),
            format!("SELECT query FROM \"pg_catalog\".\"{name}\""),
            format!("SELECT query FROM PG_CATALOG.{}", name.to_uppercase()),
            format!("SELECT * FROM public.{name}"),
            format!("TABLE {name}"),
            format!("SELECT (SELECT max(query) FROM {name}) AS q"),
            format!("WITH a AS (SELECT * FROM {name}) SELECT * FROM a"),
            format!("SELECT * FROM threads t JOIN {name} a ON true"),
            format!("SELECT * FROM {name}(NULL)"),
            format!("SELECT {name}(1)"),
            format!("SELECT (pg_catalog.{name}(NULL)).query"),
            format!("SELECT * FROM ROWS FROM (pg_catalog.{name}(NULL))"),
            format!("EXPLAIN ANALYZE SELECT * FROM {name}"),
            format!("SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype), pg_catalog.{name} a"),
            graph(&format!("RETURN pg_catalog.{name}(NULL)")),
        ] {
            refused(&sql, &format!("doesn't read {name}"));
            refused(&sql, "other connections' SQL");
        }
    }
    refused(
        r#"SELECT query FROM U&"pg_stat_activit\0079""#,
        "doesn't read pg_stat_activity",
    );
    refused(
        "SELECT table_to_xml('pg_stat_activity', true, false, '')",
        "relations named in text",
    );
    refused(
        "SELECT schema_to_xml('pg_catalog', true, false, '')",
        "relations named in text",
    );
    // Another relation of that name isn't pg_catalog's, and text is data.
    allowed("SELECT * FROM \"PG_STAT_ACTIVITY\"");
    allowed("SELECT 'pg_stat_activity', 'pg_stat_activity'::regclass::oid");
    allowed("SELECT pid, mode, granted FROM pg_locks");
    allowed("SELECT * FROM pg_stat_database");
}

#[test]
fn read_only_cypher_keeps_identifiers_comments_strings_and_parameters() {
    for text in [
        "MATCH (a:Author)-[:AUTHORED]->(m:Message) RETURN a.name, count(m)",
        "OPTIONAL MATCH (n) WHERE n.name =~ '.*;.*' RETURN n",
        "MATCH (n) WITH n, collect(n.name) AS names UNWIND names AS x RETURN x",
        "MATCH (create:Feature) RETURN create, create.name",
        "MATCH (set:SET {create: 'CREATE'}) RETURN set.set, {delete: 'DELETE'}",
        "RETURN {CREATE: 1, SET: 2, DELETE: 3}, ['MERGE', 'REMOVE']",
        "RETURN 1 AS create, 2 AS delete",
        "WITH 1 AS remove RETURN remove",
        "RETURN $CREATE, $set, $delete",
        "RETURN 'pg_sleep(1); CREATE', \"DELETE (n)\"",
        r"RETURN 'escaped \' CREATE', 'emoji \ud83d\ude00'",
        "MATCH (`CREATE`:`DELETE`) RETURN `CREATE`.`SET`",
        r"MATCH (`name\`:Feature) RETURN `name\`",
        "MATCH (n) // CREATE (m)\nRETURN n",
        "MATCH (n) /* CREATE (m) */ RETURN n",
        "RETURN 1 UNION ALL RETURN 2",
        "RETURN 1 ORDER BY 1 SKIP 0 LIMIT 1",
    ] {
        allowed(&graph(text));
        allowed(&format!("EXPLAIN (ANALYZE, BUFFERS) {}", graph(text)));
    }
    allowed(
        "SELECT * FROM ag_catalog.cypher('g', CAST(($$ MATCH (n) RETURN n $$) AS cstring)) \
         AS result(n ag_catalog.agtype)",
    );
    allowed("SELECT * FROM cypher('g', 'RETURN 1') AS result(n agtype)");
}

#[test]
fn data_and_declarations_are_not_function_executions() {
    for sql in [
        "SELECT 'set_config(''work_mem'', ''1MB'', false)'",
        "SELECT $$pg_advisory_lock(1); CREATE$$",
        "SELECT 'pg_sleep(double precision)'::regprocedure",
        "SELECT \"set_config\", \"nextval\" FROM threads",
        "SELECT 1 AS pg_sleep, 2 AS \"CREATE\"",
        "SELECT 1 /* pg_sleep(1) /* nested */ nextval('x') */",
        "WITH pg_sleep(n) AS (SELECT 1) SELECT * FROM pg_sleep",
        "SELECT * FROM threads AS set_config(id)",
        "SELECT * FROM threads set_config(id)",
        "WITH set_config(n) AS (SELECT 1) \
         SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT * FROM threads AS t(id), cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT * FROM pghacker.threads t(id), cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT 1::numeric(10, 2), * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT * FROM threads AS cypher(id), cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT * FROM threads cypher(id), cypher('g', $$ RETURN 1 $$) AS (n agtype)",
    ] {
        allowed(sql);
    }
    // The fallback can't always tell an alias from a call, so it refuses denied names as either.
    for sql in [
        "SELECT * FROM threads AS set_config(id), \
         cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT * FROM pghacker.threads set_config(id), \
         cypher('g', $$ RETURN 1 $$) AS (n agtype)",
    ] {
        refused(sql, "session settings");
    }
}

#[test]
fn lateral_calls_are_not_mistaken_for_aliases_in_the_fallback() {
    let nested = format!("{}1{}", "(".repeat(70), ")".repeat(70));
    for (call, effect) in [
        ("pg_terminate_backend(a.pid)", "controls the server"),
        ("pg_cancel_backend(a.pid)", "controls the server"),
        (
            "pg_catalog.pg_terminate_backend(a.pid)",
            "controls the server",
        ),
        ("pg_sleep(30)", "delays"),
        (
            "query_to_xml('DELETE FROM authors', true, false, '')",
            "SQL supplied as text",
        ),
    ] {
        for join in [
            "FROM pg_locks a, LATERAL",
            "FROM LATERAL",
            "FROM pg_locks a JOIN LATERAL",
            "FROM pg_locks a CROSS JOIN LATERAL",
            "FROM pg_locks a LEFT JOIN LATERAL",
            "FROM pg_locks a INNER JOIN LATERAL",
        ] {
            let on = if join.contains("CROSS") || !join.contains("JOIN") {
                ""
            } else {
                " ON true"
            };
            for sql in [
                format!("SELECT 1 {join} {call} t{on}"),
                format!(
                    "SELECT * FROM cypher('g', $$ RETURN 1 $$) AS (n agtype), \
                     (SELECT 1 {join} {call} t{on}) q"
                ),
                format!("SELECT {nested} {join} {call} t{on}"),
                format!("SELECT {nested} {join} {call}{on}"),
            ] {
                refused(&sql, effect);
                let harmless = sql.replace(call, "generate_series(1, 2)");
                if sql.contains("agtype") || sql.contains(&nested) {
                    assert!(
                        matches!(allowed(&harmless), Checked::Tokens(_)),
                        "{harmless}"
                    );
                }
            }
        }
    }
    for sql in [
        "SELECT * FROM threads t JOIN LATERAL cypher('g', $$ CREATE (m) RETURN m $$) \
         AS (m agtype) ON true",
        "SELECT * FROM threads t CROSS JOIN LATERAL \
         ag_catalog.cypher('g', $$ CREATE (m) RETURN m $$) AS (m agtype)",
        "SELECT * FROM LATERAL cypher('g', $$ CREATE (m) RETURN m $$) AS (m agtype)",
        "SELECT * FROM threads t LEFT JOIN LATERAL cypher('g', $$ MATCH (n) DELETE n $$) \
         AS (m agtype) ON true",
    ] {
        refused(sql, "Cypher query has");
    }
    for sql in [
        "SELECT * FROM threads t JOIN LATERAL cypher('g', $1) AS (m agtype) ON true",
        &format!(
            "SELECT {nested} FROM threads t JOIN LATERAL cypher('g', 'MATCH (n) ' || 'RETURN n') \
             AS (m agtype) ON true"
        ),
    ] {
        refused(sql, "literal query text");
    }
    allowed(
        "SELECT * FROM threads t JOIN LATERAL cypher('g', $$ MATCH (n) RETURN n $$) \
         AS (m agtype) ON true",
    );
}

#[test]
fn operator_syntax_cannot_hide_calls_in_the_fallback() {
    for sql in [
        "SELECT 1 OPERATOR(pg_catalog.+) pg_sleep(1)::int \
         FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
        "SELECT OPERATOR(pg_catalog.-) pg_sleep(1)::int \
         FROM cypher('g', $$ RETURN 1 $$) AS (n agtype)",
    ] {
        refused(sql, "delays");
    }
}

#[test]
fn postgres_show_explain_and_read_only_workload_shapes_pass() {
    for sql in [
        "SHOW ALL",
        "SHOW search_path",
        "SHOW TIME ZONE",
        "SHOW TRANSACTION ISOLATION LEVEL",
        "SHOW SESSION AUTHORIZATION",
        "SHOW hnsw.ef_search",
        "SHOW status.value",
        "SHOW tables.value",
        "SHOW create.value",
        "SHOW global.value",
        "SHOW local.value",
        "SHOW variables.value",
        "SELECT current_setting('work_mem'), pg_typeof(1), random(), clock_timestamp()",
        "SELECT * FROM pg_catalog.pg_settings",
        "SELECT * FROM pg_locks",
        "SELECT pg_current_wal_lsn(), pg_wal_lsn_diff('0/10', '0/0')",
        "SELECT pg_replication_origin_oid('origin'), pg_replication_origin_session_progress(false)",
        "SELECT show_limit(), to_bm25query('planner', 'pghacker.message_chunks_bm25')",
        "SELECT bm25_level_counts('index_name'), bm25_cache_global_estimated_bytes()",
        "SELECT graph_exists('pghacker_graph'), agtype_volatile_wrapper('1'::agtype)",
        "SELECT json_agg(json_build_object('id', message_id, 'body', body)) FROM messages \
         WHERE message_id IN ('example@postgresql.org')",
        "WITH r AS MATERIALIZED (SELECT id, body FROM messages WHERE thread_id = 1) \
         SELECT id, left(body, 80) FROM r ORDER BY random() LIMIT 10",
        "SELECT position(regexp_replace('quoted text', '\\s+', ' ', 'g') IN \
         regexp_replace(coalesce(body_clean, body), '\\s+', ' ', 'g')) > 0 FROM messages",
        "SELECT date_bin(interval '1 day', sent_at, timestamptz '2000-01-01') AS day, \
         count(*), array_agg(id ORDER BY id) FROM messages GROUP BY day",
        "SELECT c.id FROM (VALUES ('one'), ('two')) AS c(id) \
         LEFT JOIN messages m ON m.message_id = c.id",
        "SELECT row_number() OVER (PARTITION BY thread_id ORDER BY sent_at), \
         count(*) FILTER (WHERE has_patch) OVER () FROM messages",
        "SELECT * FROM ROWS FROM (generate_series(1, 3), unnest(ARRAY[1, 2, 3]))",
        "SELECT id FROM message_chunks ORDER BY content <@> \
         to_bm25query('planner', 'pghacker.message_chunks_bm25') LIMIT 5",
        "SELECT id FROM message_chunks ORDER BY embedding <=> '[1,2,3]'::vector LIMIT 5",
        "SELECT path FROM code_file_versions WHERE path % 'jsonpath' OR path ~* 'json'",
        "EXPLAIN SELECT 1",
        "EXPLAIN ANALYZE SELECT 1",
        "EXPLAIN (ANALYZE, BUFFERS, WAL, SETTINGS, TIMING, SUMMARY, VERBOSE, COSTS, \
         MEMORY, SERIALIZE TEXT, FORMAT JSON) SELECT 1",
        "EXPLAIN (GENERIC_PLAN, FORMAT TEXT) SELECT 1",
        "SELECT 1 +",
        "SELECT FROM WHERE",
    ] {
        allowed(sql);
    }
}

#[test]
fn non_postgres_show_commands_are_not_advertised_as_reads() {
    for sql in [
        "SHOW TABLES",
        "SHOW DATABASES",
        "SHOW SCHEMAS",
        "SHOW CATALOGS",
        "SHOW FUNCTIONS",
        "SHOW CREATE TABLE threads",
        "SHOW COLUMNS FROM threads",
        "SHOW PROCESSLIST",
        "SHOW CHARSET",
        "SHOW CHARACTER SET",
        "SHOW COLLATION",
        "SHOW GLOBAL VARIABLES",
        "SHOW SESSION VARIABLES",
        "SHOW LOCAL VARIABLES",
        "SHOW VARIABLES",
        "SHOW STATUS",
        "DESCRIBE threads",
    ] {
        refused(sql, "query only reads");
    }
}
