// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The query tool: one read-only SQL statement on pghacker's database, as
//! pgh_reader.
//!
//! - [`check`] takes only statements that read: SELECT, WITH, VALUES, TABLE,
//!   EXPLAIN of one of those, and SHOW; transaction statements are refused
//!   with a message of their own. Input that sqlparser can't parse, such as
//!   an AGE `cypher()` call with its column definition list, is checked by
//!   its tokens, including known writes, side-effecting calls and Cypher
//!   clauses. The first-word fallback is retained for other unsupported
//!   syntax; it is not a complete SQL sandbox. Untokenizable input is refused.
//! - [`QueryTool::run`] returns an [`Output`], `Failed` for refused
//!   statements, PostgreSQL's errors and timeouts alike. The SQL is logged at
//!   debug level only.
//! - `max_rows` is 100 by default and at most 1,000.
//! - The statement timeout is 10 seconds, and the call as a whole has a
//!   deadline of its own, [`CALL_TIMEOUT`].
//! - The pool is pghacker-core's, over pgh_reader's URL with its TLS, and
//!   the call waits for a connection with a tokio timeout rather than the
//!   pool's.
//! - A dropped call's cancel task isn't spawned outside a tokio runtime.
//! - Connection failures are logged and answered without their details.

use std::time::{Duration, Instant};

use deadpool_postgres::{Object, RecyclingMethod};
use pghacker_core::db::{self, Pool};
use pghacker_core::mcp::Output;
use pghacker_core::text::truncate_chars;

use crate::QueryArgs;
use crate::execute::{QueryResponse, run_readonly_query};

/// The longest SQL that query takes, in bytes.
pub const MAX_SQL_BYTES: usize = 64 * 1024;

/// The rows that query returns when the agent doesn't say.
pub const DEFAULT_MAX_ROWS: usize = 100;

/// The most rows that query returns; larger `max_rows` are lowered to it.
pub const MAX_ROWS: usize = 1000;

/// How long a statement may run: pgh_reader's own limit, which the
/// transaction sets again.
pub const STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// What query answers when its statement runs past [`STATEMENT_TIMEOUT`].
pub const TIMEOUT_MESSAGE: &str = "The statement ran past query's 10-second timeout, and was \
    cancelled. Narrow it, such as with a more selective WHERE clause or a LIMIT, and try again.";

/// How long a call waits for a connection of the pool.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a call may take in all, a backstop for a database that stops
/// answering: waiting for a connection, then the transaction's statements,
/// each under [`STATEMENT_TIMEOUT`], and the reset.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// The pool's connections' name in pg_stat_activity.
const APPLICATION_NAME: &str = "pghacker-mcp query";

const DEALLOCATE_SQL_PREPARED: &str = "
    DO $reset$
    DECLARE statement_name text;
    BEGIN
        FOR statement_name IN
            SELECT name FROM pg_catalog.pg_prepared_statements WHERE from_sql
        LOOP
            EXECUTE pg_catalog.format('DEALLOCATE %I', statement_name);
        END LOOP;
    END
    $reset$;
";

/// Checks `sql` before it runs: at most [`MAX_SQL_BYTES`], one statement,
/// and a statement that reads. Rejects nested writes, settings changes,
/// known side-effecting functions and writing Cypher. Unsupported SQL keeps
/// the first-word fallback after token-aware checks of known unsafe forms;
/// the read-only transaction and database privileges remain backstops.
/// Returns why a statement is refused.
pub fn check(sql: &str) -> Result<(), String> {
    crate::policy::check(sql, MAX_SQL_BYTES)
}

/// The query tool, with its pool of pgh_reader connections. Cheap to clone:
/// clones share the pool.
#[derive(Clone)]
pub struct QueryTool {
    pool: Pool,
}

impl QueryTool {
    /// The tool, with a pool of up to `max_connections` connections to
    /// `reader_database_url`, opened as calls need them.
    pub fn new(reader_database_url: &str, max_connections: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            max_connections > 0,
            "the query pool needs at least one connection"
        );
        Ok(Self {
            pool: db::pool(reader_database_url, max_connections, APPLICATION_NAME)?,
        })
    }

    /// Runs `args`' statement: the CSV, or why it failed.
    pub async fn run(&self, args: QueryArgs) -> Output {
        let max_rows = match args.max_rows {
            None => DEFAULT_MAX_ROWS,
            Some(0) => return Output::Failed("max_rows must be at least 1".to_owned()),
            Some(rows) => usize::try_from(rows).map_or(MAX_ROWS, |rows| rows.min(MAX_ROWS)),
        };
        let sql = args.sql;
        tracing::debug!(sql = %truncate_chars(&sql, 1000), "a query");
        if let Err(reason) = check(&sql) {
            tracing::info!(outcome = "refused", reason = %reason, "refused a query");
            return Output::Failed(reason);
        }

        let started = Instant::now();
        let response = tokio::time::timeout(CALL_TIMEOUT, self.execute(&sql, max_rows)).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(message)) => return Output::Failed(message),
            Err(_) => {
                tracing::warn!(
                    outcome = "abandoned",
                    duration_ms,
                    "a query ran past the call's deadline"
                );
                return Output::Failed(format!(
                    "the database didn't answer within {} seconds; try again shortly",
                    CALL_TIMEOUT.as_secs()
                ));
            }
        };
        match response {
            QueryResponse::TimedOut => {
                tracing::info!(outcome = "timeout", duration_ms, "a query timed out");
                Output::Failed(TIMEOUT_MESSAGE.to_owned())
            }
            QueryResponse::Failed(message) => {
                tracing::info!(outcome = "failed", duration_ms, error = %truncate_chars(&message, 300), "a query failed");
                Output::Failed(message)
            }
            QueryResponse::Rows {
                csv,
                row_count,
                truncated,
            } => {
                tracing::info!(
                    outcome = "ok",
                    duration_ms,
                    rows = row_count,
                    truncated,
                    "ran a query"
                );
                let count = if row_count == 1 {
                    "(1 row)".to_owned()
                } else {
                    format!("({row_count} rows)")
                };
                Output::Text(if csv.is_empty() {
                    count
                } else {
                    format!("{csv}\n\n{count}")
                })
            }
        }
    }

    /// Runs `sql`, which [`check`] passed, on a connection of the pool, and
    /// resets the connection. Errs if no connection is to be had.
    pub(crate) async fn execute(
        &self,
        sql: &str,
        max_rows: usize,
    ) -> Result<QueryResponse, String> {
        let client = match tokio::time::timeout(CHECKOUT_TIMEOUT, self.pool.get()).await {
            Ok(Ok(client)) => client,
            Err(_) => {
                return Err(
                    "every connection of the query pool is busy with another query; try \
                            again shortly"
                        .to_owned(),
                );
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "the query pool couldn't connect to the database");
                return Err("query couldn't connect to the database; try again shortly".to_owned());
            }
        };
        let mut checkout = Checkout {
            client: Some(client),
            returned: false,
        };
        let timeout_ms = i64::try_from(STATEMENT_TIMEOUT.as_millis()).unwrap_or(i64::MAX);
        let response = run_readonly_query(checkout.client(), sql, max_rows, Some(timeout_ms)).await;
        checkout.reset().await;
        Ok(response)
    }
}

/// A connection that a call took from the query pool. Once [`Self::reset`]
/// has reset it, it goes back to the pool when dropped. Dropped before
/// then, such as when the call is abandoned mid-statement, it cancels
/// whatever the connection runs and closes it, so that the pool never hands
/// it out again.
struct Checkout {
    client: Option<Object>,
    returned: bool,
}

impl Checkout {
    fn client(&mut self) -> &mut tokio_postgres::Client {
        self.client.as_mut().expect("a checked-out connection")
    }

    /// Resets the session's state, so that the connection can go back to
    /// the pool; if the reset fails, it's dropped instead.
    async fn reset(&mut self) {
        // SQL-created statements outlive rollback; protocol statements back driver caches.
        let clean = RecyclingMethod::Clean.query().expect("Clean has a query");
        let sql = format!("{clean}; {DEALLOCATE_SQL_PREPARED}");
        match self.client().batch_execute(&sql).await {
            Ok(()) => self.returned = true,
            Err(error) => tracing::warn!(%error, "couldn't reset a query connection; it's dropped"),
        }
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        let Some(client) = self.client.take() else {
            return;
        };
        let cancel = client.cancel_token();
        let connection = Object::take(client);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = db::cancel_query(&cancel).await {
                tracing::debug!(error = %format!("{error:#}"), "couldn't cancel a query's statement");
            }
            drop(connection);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(sql: &str) -> String {
        check(sql).expect_err(sql)
    }

    #[test]
    fn transaction_statements_are_refused() {
        for (sql, name) in [
            ("BEGIN", "BEGIN"),
            ("begin read write", "BEGIN"),
            (
                "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
                "START TRANSACTION",
            ),
            ("COMMIT", "COMMIT"),
            ("COMMIT AND CHAIN", "COMMIT"),
            ("END", "END"),
            ("ROLLBACK", "ROLLBACK"),
            ("ABORT", "ABORT"),
            ("ROLLBACK TO SAVEPOINT s", "ROLLBACK TO SAVEPOINT"),
            ("SAVEPOINT s", "SAVEPOINT"),
            ("RELEASE SAVEPOINT s", "RELEASE SAVEPOINT"),
            ("SET TRANSACTION READ WRITE", "SET TRANSACTION"),
            (
                "SET TRANSACTION SNAPSHOT '00000003-0000001B-1'",
                "SET TRANSACTION",
            ),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                "SET SESSION CHARACTERISTICS AS TRANSACTION",
            ),
            // sqlparser can't parse these; their tokens are refused.
            ("COMMIT PREPARED 'x'", "COMMIT"),
            ("ROLLBACK PREPARED 'x'", "ROLLBACK"),
        ] {
            let reason = refused(sql);
            assert!(
                reason.contains(name) && reason.contains("read-only transaction"),
                "{sql}: {reason}"
            );
        }
    }

    #[test]
    fn several_statements_or_none_are_refused() {
        assert!(refused("SELECT 1; SELECT 2").contains("2 statements"));
        assert!(refused("select 1; commit").contains("2 statements"));
        assert!(refused("SELECT 1; DROP TABLE threads; SELECT 2").contains("3 statements"));
        // What sqlparser can't parse is split by its tokens.
        assert!(
            refused(
                "SELECT * FROM cypher('pghacker_graph', $$ MATCH (n) RETURN n $$) AS (n agtype); \
                 DROP TABLE threads"
            )
            .contains("2 statements")
        );
        assert!(refused("").contains("no statement"));
        assert!(refused("   \n\t ").contains("no statement"));
        assert!(refused(";").contains("no statement"));
        assert!(refused("-- just a comment").contains("no statement"));
        assert!(refused("/* only a comment */").contains("no statement"));
        assert_eq!(check("SELECT 1;"), Ok(()));
        assert_eq!(check("SELECT 1; -- done"), Ok(()));
    }

    #[test]
    fn reads_pass() {
        for sql in [
            "SELECT 1",
            "select count(*) from threads",
            "WITH r AS (SELECT 1 AS a) SELECT a FROM r",
            "WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM t WHERE n < 5) \
             SELECT * FROM t",
            "VALUES (1, 'a'), (2, 'b')",
            "TABLE authors",
            "(SELECT 1) UNION (SELECT 2)",
            "EXPLAIN SELECT 1",
            "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM messages",
            "EXPLAIN ANALYZE SELECT 1",
            "SHOW search_path",
            "SHOW ALL",
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY n) FROM t",
            "SELECT count(*) FILTER (WHERE has_patch) FROM messages",
            "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a, b DESC",
            "SELECT * FROM t, LATERAL jsonb_array_elements(t.j) e",
            "SELECT x::numeric(10,2), now() - interval '7 days' FROM t",
            "SELECT 'a;b' AS s",
            "SELECT $$a;b$$ AS s",
            "SELECT id FROM message_chunks \
             ORDER BY content <@> to_bm25query('planner', 'pghacker.message_chunks_bm25') LIMIT 5",
            "SELECT id FROM message_chunks ORDER BY embedding <=> '[1,2,3]' LIMIT 5",
            "SELECT path FROM code_file_versions WHERE path % 'jsonpath' OR path ~* 'json'",
            "SELECT random(), clock_timestamp(), current_setting('search_path')",
            "SELECT 1 + /* nested /* comment */ ; */ 1",
            "-- a comment first\nSELECT 1",
        ] {
            assert_eq!(check(sql), Ok(()), "{sql}");
        }
    }

    #[test]
    fn cypher_calls_pass() {
        for sql in [
            "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author)-[:AUTHORED]->(m:Message) \
             RETURN a.name, count(m) $$) AS (name agtype, n agtype)",
            "SELECT * FROM ag_catalog.cypher('pghacker_graph', $q$ MATCH (n:Feature) \
             WHERE n.name =~ '.*;.*' RETURN n $q$) AS (n ag_catalog.agtype)",
            "WITH f AS (SELECT * FROM cypher('pghacker_graph', $$ MATCH (f:Feature) RETURN f.key \
             $$) AS (key agtype)) SELECT count(*) FROM f",
            "EXPLAIN SELECT * FROM cypher('pghacker_graph', $$ MATCH (n) RETURN n $$) AS (n agtype)",
            "  -- the graph\n  SELECT * FROM cypher('pghacker_graph', $$ MATCH (n) RETURN n $$) \
             AS (n agtype);  ",
        ] {
            assert_eq!(check(sql), Ok(()), "{sql}");
        }
    }

    #[test]
    fn statements_that_dont_read_are_refused() {
        for (sql, name) in [
            ("INSERT INTO authors (display_name) VALUES ('x')", "INSERT"),
            ("UPDATE authors SET display_name = 'x'", "UPDATE"),
            ("DELETE FROM authors", "DELETE"),
            (
                "MERGE INTO authors a USING authors b ON a.id = b.id WHEN MATCHED THEN DELETE",
                "MERGE",
            ),
            ("CREATE TABLE x (a int)", "CREATE"),
            ("CREATE TEMP TABLE x (a int)", "CREATE"),
            ("create index on threads (subject)", "CREATE"),
            ("DROP TABLE threads", "DROP"),
            ("ALTER TABLE threads ADD COLUMN x int", "ALTER"),
            ("GRANT SELECT ON author_emails TO pgh_reader", "GRANT"),
            ("REVOKE SELECT ON threads FROM public", "REVOKE"),
            ("TRUNCATE threads", "TRUNCATE"),
            ("COPY threads TO STDOUT", "COPY"),
            ("COPY (SELECT 1) TO STDOUT", "COPY"),
            ("VACUUM threads", "VACUUM"),
            ("ANALYZE threads", "ANALYZE"),
            ("CALL p()", "CALL"),
            ("DO $$ BEGIN PERFORM 1; END $$", "DO"),
            ("do $body$ begin execute 'select 1'; end $body$", "DO"),
            ("SET work_mem = '1MB'", "SET"),
            ("SET LOCAL statement_timeout = 0", "SET"),
            ("set search_path to public", "SET"),
            ("SET ROLE pgh_indexer", "SET"),
            ("SET SESSION AUTHORIZATION DEFAULT", "SET"),
            ("RESET ALL", "RESET"),
            ("RESET statement_timeout", "RESET"),
            ("PREPARE p AS SELECT 1", "PREPARE"),
            ("PREPARE TRANSACTION 'x'", "PREPARE"),
            ("EXECUTE p", "EXECUTE"),
            ("DEALLOCATE p", "DEALLOCATE"),
            ("LISTEN x", "LISTEN"),
            ("NOTIFY x", "NOTIFY"),
            ("LOAD 'age'", "LOAD"),
            ("LOCK TABLE threads", "LOCK"),
            ("DECLARE c CURSOR FOR SELECT 1", "DECLARE"),
            ("FETCH c", "FETCH"),
            ("CLOSE c", "CLOSE"),
            ("DISCARD ALL", "DISCARD"),
            ("COMMENT ON TABLE threads IS 'x'", "COMMENT"),
            ("CHECKPOINT", "CHECKPOINT"),
            ("REFRESH MATERIALIZED VIEW v", "REFRESH"),
            ("REINDEX TABLE threads", "REINDEX"),
            ("CLUSTER threads", "CLUSTER"),
            ("SECURITY LABEL ON TABLE threads IS 'x'", "SECURITY"),
            (
                "EXPLAIN ANALYZE INSERT INTO authors (display_name) VALUES ('x')",
                "INSERT",
            ),
            ("EXPLAIN (ANALYZE) DELETE FROM authors", "DELETE"),
            ("\"SELECT\" 1", "\"SELECT\""),
        ] {
            let reason = refused(sql);
            assert!(
                reason.contains(&format!("not {name}")) && reason.contains("only reads"),
                "{sql}: {reason}"
            );
        }
    }

    #[test]
    fn writes_inside_reads_are_refused_before_execution() {
        for (sql, reason) in [
            (
                "WITH x AS (INSERT INTO authors (display_name) VALUES ('x') RETURNING id) \
                 SELECT * FROM x",
                "not INSERT",
            ),
            ("SELECT 1 INTO t", "SELECT INTO"),
            (
                "SELECT * FROM cypher('pghacker_graph', $$ CREATE (:Feature {key: 'x'}) $$) \
                 AS (x agtype)",
                "CREATE clause",
            ),
        ] {
            assert!(refused(sql).contains(reason), "{sql}");
        }
    }

    #[test]
    fn syntax_errors_in_reads_are_left_to_postgres() {
        assert_eq!(check("SELECT 1 +"), Ok(()));
        assert_eq!(check("SELECT FROM WHERE"), Ok(()));
    }

    #[test]
    fn untokenizable_sql_is_refused() {
        assert!(refused("SELECT 'unterminated").contains("can't read the SQL"));
        assert!(refused("SELECT $$ unterminated").contains("can't read the SQL"));
    }

    /// Runs `sql` as query would if [`check`] let it through.
    async fn unchecked(tool: &QueryTool, sql: &str) -> QueryResponse {
        tool.execute(sql, 10).await.expect("a connection")
    }

    async fn csv(tool: &QueryTool, sql: &str) -> String {
        match unchecked(tool, sql).await {
            QueryResponse::Rows { csv, .. } => csv,
            other => panic!("{sql}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn refused_calls_do_not_need_a_database_connection() {
        let tool = QueryTool::new("postgresql://pgh_reader@127.0.0.1:1/no_database", 1).unwrap();
        for sql in [
            "SET work_mem = '1MB'",
            "SELECT set_config('work_mem', '1MB', true)",
            "SELECT pg_advisory_lock(1)",
            "SELECT pg_cancel_backend(1)",
            "SELECT pg_sleep(1)",
            "SELECT nextval('some_sequence')",
            "SELECT * FROM cypher('g', $$ CREATE (n) $$) AS (n agtype)",
            "WITH w AS (DELETE FROM authors RETURNING *) SELECT * FROM w",
        ] {
            let expected = check(sql).unwrap_err();
            assert!(
                matches!(tool.run(QueryArgs { sql: sql.to_owned(), max_rows: None }).await,
                    Output::Failed(reason) if reason == expected),
                "{sql}"
            );
        }
    }

    #[tokio::test]
    async fn the_transaction_still_refuses_writes_if_the_checker_is_bypassed() {
        let db = pghacker_core::testing::TestDb::create().await.unwrap();
        let tool = QueryTool::new(&db.reader_url, 1).unwrap();
        for sql in [
            "WITH w AS (INSERT INTO authors (display_name) VALUES ('x') RETURNING id) \
             SELECT * FROM w",
            "SELECT 1 INTO TEMP copied",
            "SELECT * FROM cypher('pghacker_graph', $$ CREATE (:Feature {key:'x'}) $$) AS (n agtype)",
        ] {
            assert!(check(sql).is_err(), "{sql}");
            assert!(
                matches!(unchecked(&tool, sql).await,
                    QueryResponse::Failed(reason) if reason.contains("read-only transaction")),
                "{sql}"
            );
        }
    }

    #[tokio::test]
    async fn what_outlives_a_rollback_does_not_outlive_the_call() {
        let db = pghacker_core::testing::TestDb::create().await.unwrap();
        let tool = QueryTool::new(&db.reader_url, 1).unwrap();
        let pid = csv(&tool, "SELECT pg_backend_pid() AS pid").await;
        let work_mem = csv(&tool, "SHOW work_mem").await;
        // pg_cursors lists the driver's own portals too; only the held one counts.
        let left = "SELECT (SELECT count(*) FROM pg_prepared_statements WHERE from_sql) AS prepared, \
                    (SELECT count(*) FROM pg_listening_channels()) AS channels, \
                    (SELECT count(*) FROM pg_cursors WHERE name = 'held') AS held";
        // Each call resolves an extension type that the driver hasn't seen,
        // with its own protocol-level statements, which the reset keeps.
        for (sql, typed) in [
            (
                r#"PREPARE "caller, ""one""" AS SELECT 42"#,
                "SELECT '1'::agtype AS value",
            ),
            (
                r#"DO $$ BEGIN EXECUTE 'PREPARE "caller, ""one""" AS SELECT 42'; END $$"#,
                "SELECT '[1,2]'::vector AS value",
            ),
            (
                "SET work_mem = '1234kB'",
                "SELECT '[1,2]'::halfvec AS value",
            ),
            (
                "LISTEN pghacker_test",
                "SELECT '{1:1}/2'::sparsevec AS value",
            ),
            (
                "DECLARE held CURSOR WITH HOLD FOR SELECT 1",
                "SELECT ag_catalog._graphid(1, 1) AS value",
            ),
            (
                "SELECT pg_advisory_lock(4242)",
                "SELECT '1'::agtype AS value",
            ),
            (
                "SELECT set_config('work_mem', '1234kB', false)",
                "SELECT '[1,2]'::vector AS value",
            ),
        ] {
            assert!(check(sql).is_err(), "{sql}");
            let response = unchecked(&tool, sql).await;
            assert!(
                matches!(response, QueryResponse::Rows { .. }),
                "{sql}: {response:?}"
            );
            assert_eq!(csv(&tool, "SELECT pg_backend_pid() AS pid").await, pid);
            assert_eq!(
                csv(&tool, left).await,
                "prepared,channels,held\n0,0,0",
                "{sql}"
            );
            assert_eq!(csv(&tool, "SHOW work_mem").await, work_mem, "{sql}");
            let missing = unchecked(&tool, r#"EXECUTE "caller, ""one""""#).await;
            assert!(
                matches!(&missing, QueryResponse::Failed(message) if message.contains("does not exist")),
                "{missing:?}"
            );
            for _ in 0..2 {
                let response = unchecked(&tool, typed).await;
                assert!(
                    matches!(response, QueryResponse::Rows { row_count: 1, .. }),
                    "{typed}: {response:?}"
                );
            }
        }
        let failed = unchecked(
            &tool,
            "DO $$ BEGIN EXECUTE 'PREPARE abandoned AS SELECT 1'; \
             RAISE EXCEPTION 'after prepare'; END $$",
        )
        .await;
        assert!(
            matches!(&failed, QueryResponse::Failed(message) if message.contains("after prepare")),
            "{failed:?}"
        );
        assert_eq!(csv(&tool, "SELECT pg_backend_pid() AS pid").await, pid);
        assert_eq!(csv(&tool, left).await, "prepared,channels,held\n0,0,0");
        let held: i64 = db
            .admin()
            .await
            .unwrap()
            .query_one(
                "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 4242",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(held, 0);
    }

    #[test]
    fn long_sql_is_refused() {
        let sql = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES));
        assert!(refused(&sql).contains("at most 65536"));
        let longest = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES - 9));
        assert_eq!(longest.len(), MAX_SQL_BYTES);
        assert_eq!(check(&longest), Ok(()));
    }
}
