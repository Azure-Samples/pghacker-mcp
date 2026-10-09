//! The query tool against a fresh database, as pgh_reader: CSV, quoting,
//! cuts, the timeout, what the read-only transaction and the grants refuse,
//! the AGE graph, types without binary decoders, and connection hygiene.

use std::time::{Duration, Instant};

use pghacker_core::mcp::Output;
use pghacker_core::testing::TestDb;
use pghacker_sqlquery::{MAX_CSV_BYTES, QueryArgs, QueryTool, TIMEOUT_MESSAGE};

async fn tool(pool: usize) -> (TestDb, QueryTool) {
    let db = TestDb::create().await.unwrap();
    let tool = QueryTool::new(&db.reader_url, pool).unwrap();
    (db, tool)
}

async fn run(tool: &QueryTool, sql: &str, max_rows: Option<u32>) -> Result<String, String> {
    let args = QueryArgs {
        sql: sql.to_owned(),
        max_rows,
    };
    match tool.run(args).await {
        Output::Text(text) => Ok(text),
        Output::Failed(message) => Err(message),
        Output::Json(value) => panic!("{sql} returned JSON: {value}"),
    }
}

async fn text(tool: &QueryTool, sql: &str) -> String {
    run(tool, sql, None)
        .await
        .unwrap_or_else(|message| panic!("{sql} failed: {message}"))
}

async fn failure(tool: &QueryTool, sql: &str) -> String {
    match run(tool, sql, None).await {
        Ok(text) => panic!("{sql} should fail, but returned {text}"),
        Err(message) => message,
    }
}

/// The one field of a one-column, one-row result, as the CSV has it.
async fn value(tool: &QueryTool, sql: &str) -> String {
    let text = text(tool, sql).await;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{sql}: {text}");
    assert_eq!(lines[3], "(1 row)", "{sql}: {text}");
    lines[1].to_owned()
}

#[tokio::test]
async fn selects_return_csv_with_a_header_and_the_row_count() {
    let (_db, tool) = tool(2).await;
    assert_eq!(
        text(&tool, "SELECT 1 AS one, 'two' AS two").await,
        "one,two\n1,two\n\n(1 row)"
    );
    assert_eq!(
        text(&tool, "SELECT g FROM generate_series(1, 3) g").await,
        "g\n1\n2\n3\n\n(3 rows)"
    );
    assert_eq!(
        text(&tool, "SELECT 1 AS one WHERE false").await,
        "one\n\n(0 rows)"
    );
    assert_eq!(
        text(&tool, "VALUES (1, 'a'), (2, 'b')").await,
        "column1,column2\n1,a\n2,b\n\n(2 rows)"
    );
    // Columns that share a name keep their own values.
    assert_eq!(
        text(&tool, "SELECT 1 AS id, 2 AS id").await,
        "id,id\n1,2\n\n(1 row)"
    );
    assert_eq!(
        text(&tool, "SELECT FROM generate_series(1, 2)").await,
        "(2 rows)"
    );
    // Unqualified names resolve to schema pghacker.
    assert_eq!(
        text(&tool, "SELECT count(*) FROM threads").await,
        "count\n0\n\n(1 row)"
    );
    assert_eq!(
        text(&tool, "TABLE sources").await,
        "id,kind,name,config,enabled,created_at,updated_at\n\n(0 rows)"
    );
    assert_eq!(
        text(&tool, "SHOW search_path").await,
        "search_path\n\"pghacker, ag_catalog, public\"\n\n(1 row)"
    );
    let plan = text(&tool, "EXPLAIN SELECT * FROM threads WHERE id = 1").await;
    assert!(
        plan.starts_with("QUERY PLAN\n") && plan.contains("threads"),
        "{plan}"
    );
    let analyzed = text(&tool, "EXPLAIN (ANALYZE, COSTS OFF) SELECT 1").await;
    assert!(analyzed.contains("actual time"), "{analyzed}");
}

#[tokio::test]
async fn null_empty_strings_and_special_characters_are_told_apart() {
    let (_db, tool) = tool(1).await;
    assert_eq!(
        text(
            &tool,
            "SELECT NULL::text AS n, '' AS e, 'None' AS t, 'a,b' AS c, 'say \"hi\"' AS q, \
             E'l1\\nl2' AS nl, E'cr\\r' AS cr, ' pad ' AS p, 'plain text' AS x"
        )
        .await,
        "n,e,t,c,q,nl,cr,p,x\n\
         None,\"\",\"None\",\"a,b\",\"say \"\"hi\"\"\",\"l1\nl2\",\"cr\r\",\" pad \",plain text\n\n\
         (1 row)"
    );
    assert_eq!(
        text(&tool, "SELECT 1 AS \"a,b\", 2 AS \"say \"\"hi\"\"\"").await,
        "\"a,b\",\"say \"\"hi\"\"\"\n1,2\n\n(1 row)"
    );
    assert_eq!(
        text(&tool, "SELECT NULL AS a, NULL AS b").await,
        "a,b\nNone,None\n\n(1 row)"
    );
}

#[tokio::test]
async fn max_rows_cuts_rows_with_a_banner() {
    let (_db, tool) = tool(2).await;
    let five = "SELECT g FROM generate_series(1, 5) g";
    assert_eq!(
        run(&tool, five, Some(2)).await.unwrap(),
        "Result has more than 2 rows, and is too large to return. Only returning the first 2 \
         results.\n\ng\n1\n2\n\n(2 rows)"
    );
    assert_eq!(
        run(&tool, five, Some(5)).await.unwrap(),
        "g\n1\n2\n3\n4\n5\n\n(5 rows)"
    );
    let default = text(&tool, "SELECT g FROM generate_series(1, 150) g").await;
    assert!(
        default.starts_with("Result has more than 100 rows,")
            && default.ends_with("\n100\n\n(100 rows)"),
        "{default}"
    );
    let most = run(
        &tool,
        "SELECT g FROM generate_series(1, 1500) g",
        Some(5000),
    )
    .await
    .unwrap();
    assert!(
        most.starts_with("Result has more than 1000 rows,")
            && most.ends_with("\n1000\n\n(1000 rows)"),
        "{most}"
    );
    let refused = run(&tool, five, Some(0)).await.unwrap_err();
    assert!(refused.contains("max_rows"), "{refused}");
}

#[tokio::test]
async fn large_results_are_cut_at_the_size_limit() {
    let (_db, tool) = tool(1).await;
    let wide = text(
        &tool,
        "SELECT g, repeat('x', 300000) AS x FROM generate_series(1, 10) g",
    )
    .await;
    let banner = format!(
        "Result is larger than {MAX_CSV_BYTES} bytes, and is too large to return. Only returning \
         the first 3 rows.\n\n"
    );
    assert!(wide.starts_with(&banner), "{}", &wide[..200]);
    assert!(
        wide.ends_with("\n\n(3 rows)"),
        "{}",
        &wide[wide.len() - 200..]
    );
    assert!(wide.len() < MAX_CSV_BYTES + banner.len() + 20);
    assert_eq!(
        text(&tool, "SELECT repeat('x', 2000000) AS x").await,
        format!(
            "Result is larger than {MAX_CSV_BYTES} bytes, and is too large to return. Only \
             returning the first 0 rows.\n\nx\n\n(0 rows)"
        )
    );
}

#[tokio::test]
async fn a_statement_past_the_timeout_is_cancelled() {
    let (_db, tool) = tool(1).await;
    let started = Instant::now();
    let message = failure(&tool, "SELECT pg_sleep(6)").await;
    let elapsed = started.elapsed();
    assert_eq!(message, TIMEOUT_MESSAGE);
    assert!(
        elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_millis(6500),
        "{elapsed:?}"
    );
    assert_eq!(value(&tool, "SELECT 1").await, "1");
}

#[tokio::test]
async fn writes_fail_in_the_read_only_transaction() {
    let (db, tool) = tool(1).await;
    let message = failure(
        &tool,
        "WITH x AS (INSERT INTO authors (display_name) VALUES ('Ada Example') RETURNING id) \
         SELECT * FROM x",
    )
    .await;
    // PostgreSQL names the statement, a SELECT, rather than its INSERT.
    assert_eq!(
        message,
        "ERROR:  cannot execute SELECT in a read-only transaction"
    );
    assert_eq!(
        failure(&tool, "SELECT 1 AS a INTO TEMP copied").await,
        "ERROR:  cannot execute SELECT INTO in a read-only transaction"
    );
    assert_eq!(
        failure(
            &tool,
            "SELECT * FROM cypher('pghacker_graph', $$ CREATE (:Feature {key: 'feature:x'}) $$) \
             AS (x agtype)",
        )
        .await,
        "ERROR:  cannot execute SELECT in a read-only transaction"
    );
    let message = failure(&tool, "INSERT INTO authors (display_name) VALUES ('x')").await;
    assert!(
        message.contains("only reads") && message.contains("not INSERT"),
        "{message}"
    );

    let indexer = db.indexer().await.unwrap();
    let written: i64 = indexer
        .query_one(
            "SELECT (SELECT count(*) FROM authors) + (SELECT count(*) FROM pghacker_graph.\"Feature\")",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(written, 0);
}

#[tokio::test]
async fn emails_and_internal_tables_are_refused() {
    let (db, tool) = tool(1).await;
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO authors (id, display_name) VALUES (1, 'Ada Example');
             INSERT INTO author_emails (email, author_id) VALUES ('ada@example.org', 1);",
        )
        .await
        .unwrap();
    assert_eq!(
        text(&tool, "SELECT id, display_name FROM authors").await,
        "id,display_name\n1,Ada Example\n\n(1 row)"
    );
    assert_eq!(
        failure(&tool, "SELECT * FROM author_emails").await,
        "ERROR:  permission denied for table author_emails"
    );
    let message = failure(
        &tool,
        "SELECT a.display_name, e.email FROM authors a \
         JOIN pghacker.author_emails e ON e.author_id = a.id",
    )
    .await;
    assert!(
        message.starts_with("ERROR:  permission denied for table author_emails"),
        "{message}"
    );
    let message = failure(&tool, "SELECT * FROM jobs").await;
    assert!(
        message.starts_with("ERROR:  permission denied for table jobs"),
        "{message}"
    );
}

#[tokio::test]
async fn postgres_errors_read_as_psql_prints_them() {
    let (_db, tool) = tool(1).await;
    assert_eq!(
        failure(&tool, "SELECT * FROM nope").await,
        format!(
            "ERROR:  relation \"nope\" does not exist\nLINE 1: SELECT * FROM nope\n{}^",
            " ".repeat(22)
        )
    );
    assert_eq!(
        failure(&tool, "SELECT 1 +").await,
        format!(
            "ERROR:  syntax error at end of input\nLINE 1: SELECT 1 +\n{}^",
            " ".repeat(18)
        )
    );
    let message = failure(&tool, "SELECT 1\nFROM threads\nWHERE nope = 1").await;
    assert!(
        message.starts_with(&format!(
            "ERROR:  column \"nope\" does not exist\nLINE 3: WHERE nope = 1\n{}^",
            " ".repeat(14)
        )),
        "{message}"
    );
    let message = failure(&tool, "SELECT nope(1)").await;
    assert!(
        message.starts_with("ERROR:  function nope(integer) does not exist\nLINE 1:")
            && message.contains("\nHINT:  "),
        "{message}"
    );
    assert_eq!(
        failure(&tool, "SELECT 1 / 0").await,
        "ERROR:  division by zero"
    );
    let refused = failure(&tool, "BEGIN").await;
    assert!(refused.contains("BEGIN"), "{refused}");
    let refused = failure(&tool, "SELECT 1; SELECT 2").await;
    assert!(refused.contains("2 statements"), "{refused}");
}

/// Ada Example authored two messages, in the graph as the indexer writes it.
async fn add_authored_messages(db: &TestDb) {
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            r#"
            WITH l AS (SELECT id FROM ag_catalog.ag_label WHERE name = 'Author'
                       AND graph = (SELECT graphid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))
            INSERT INTO pghacker_graph."Author" (id, properties)
            SELECT ag_catalog._graphid(l.id, nextval('pghacker_graph."Author_id_seq"')),
                   jsonb_build_object('key', 'author:1', 'name', 'Ada Example')::text::ag_catalog.agtype
            FROM l;
            WITH l AS (SELECT id FROM ag_catalog.ag_label WHERE name = 'Message'
                       AND graph = (SELECT graphid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))
            INSERT INTO pghacker_graph."Message" (id, properties)
            SELECT ag_catalog._graphid(l.id, nextval('pghacker_graph."Message_id_seq"')),
                   jsonb_build_object('key', 'message:' || n, 'subject', 'Subject, ' || n)::text::ag_catalog.agtype
            FROM l, generate_series(1, 2) n;
            WITH l AS (SELECT id FROM ag_catalog.ag_label WHERE name = 'AUTHORED'
                       AND graph = (SELECT graphid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))
            INSERT INTO pghacker_graph."AUTHORED" (id, start_id, end_id, properties)
            SELECT ag_catalog._graphid(l.id, nextval('pghacker_graph."AUTHORED_id_seq"')), a.id, m.id,
                   '{}'::ag_catalog.agtype
            FROM l, pghacker_graph."Author" a, pghacker_graph."Message" m;
            "#,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn cypher_reads_the_graph() {
    let (db, tool) = tool(1).await;
    add_authored_messages(&db).await;
    assert_eq!(
        text(
            &tool,
            "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author)-[:AUTHORED]->(m:Message) \
             RETURN a.name, count(m) $$) AS (name agtype, n agtype)"
        )
        .await,
        "name,n\n\"\"\"Ada Example\"\"\",2\n\n(1 row)"
    );
    assert_eq!(
        text(
            &tool,
            "SELECT * FROM ag_catalog.cypher('pghacker_graph', $q$ MATCH (m:Message) \
             RETURN m.key, m.subject ORDER BY m.key $q$) \
             AS (key ag_catalog.agtype, subject ag_catalog.agtype)"
        )
        .await,
        "key,subject\n\
         \"\"\"message:1\"\"\",\"\"\"Subject, 1\"\"\"\n\
         \"\"\"message:2\"\"\",\"\"\"Subject, 2\"\"\"\n\n\
         (2 rows)"
    );
    let vertex = text(
        &tool,
        "SELECT * FROM cypher('pghacker_graph', $$ MATCH (a:Author) RETURN a $$) AS (a agtype)",
    )
    .await;
    assert!(
        vertex.starts_with("a\n\"{\"\"id\"\": ")
            && vertex.contains("\"\"label\"\": \"\"Author\"\"")
            && vertex.ends_with("}::vertex\"\n\n(1 row)"),
        "{vertex}"
    );
    // A cast to text drops agtype's quotes.
    assert_eq!(
        text(
            &tool,
            "SELECT name::text AS name FROM cypher('pghacker_graph', $$ MATCH (a:Author) \
             RETURN a.name $$) AS (name agtype)"
        )
        .await,
        "name\nAda Example\n\n(1 row)"
    );
}

#[tokio::test]
async fn types_without_binary_decoders_render_as_text() {
    let (_db, tool) = tool(1).await;
    let csv = text(
        &tool,
        r#"SELECT '1 day 02:03:04'::interval AS i, '10.0.0.1'::inet AS ip, '10.0.0.0/8'::cidr AS net,
                  '\x0102ff'::bytea AS b, '{"a": [1, 2]}'::jsonb AS j, ARRAY[1, 2, 3] AS ints,
                  ARRAY['a b', NULL] AS texts, true AS yes, false AS no, ARRAY[true, false] AS bools,
                  1.5::float8 AS f, 0.1::float8 AS tenth, 12.50::numeric AS n,
                  '2026-10-09'::date AS d, '12:34:56'::time AS t,
                  '2026-10-09 12:34:56+00'::timestamptz AT TIME ZONE 'UTC' AS ts,
                  '[1,5)'::int4range AS r, 'b a'::tsvector AS tsv, '00:11:22:33:44:55'::macaddr AS mac,
                  '00000000-0000-0000-0000-000000000001'::uuid AS id, '(1,2)'::point AS p,
                  'threads'::regclass AS rel, '{"k": 1}'::agtype AS ag, '[1,2,3]'::vector AS v,
                  '[1,2]'::halfvec AS hv"#,
    )
    .await;
    let (header, rest) = csv.split_once('\n').unwrap();
    assert_eq!(
        header,
        "i,ip,net,b,j,ints,texts,yes,no,bools,f,tenth,n,d,t,ts,r,tsv,mac,id,p,rel,ag,v,hv"
    );
    assert_eq!(
        rest,
        "1 day 02:03:04,10.0.0.1,10.0.0.0/8,\\x0102ff,\"{\"\"a\"\": [1, 2]}\",\"{1,2,3}\",\
         \"{\"\"a b\"\",NULL}\",True,False,\"{t,f}\",1.5,0.1,12.50,2026-10-09,12:34:56,\
         2026-10-09 12:34:56,\"[1,5)\",'a' 'b',00:11:22:33:44:55,\
         00000000-0000-0000-0000-000000000001,\"(1,2)\",threads,\"{\"\"k\"\": 1}\",\"[1,2,3]\",\
         \"[1,2]\"\n\n(1 row)"
    );
}

#[tokio::test]
async fn what_a_call_leaves_on_its_connection_does_not_reach_the_next() {
    let (db, tool) = tool(1).await;
    let pid = value(&tool, "SELECT pg_backend_pid()").await;

    // A session-level advisory lock outlives the rollback; the reset releases it.
    assert_eq!(value(&tool, "SELECT pg_advisory_lock(4242)").await, "\"\"");
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

    // What a SELECT sets for the session is undone.
    for (setting, set_to) in [
        ("work_mem", "1234kB"),
        ("search_path", "public"),
        ("default_transaction_read_only", "off"),
    ] {
        let show = format!("SHOW {setting}");
        let before = value(&tool, &show).await;
        assert_ne!(before, set_to);
        value(
            &tool,
            &format!("SELECT set_config('{setting}', '{set_to}', false)"),
        )
        .await;
        assert_eq!(value(&tool, &show).await, before, "{setting}");
    }
    assert_eq!(
        value(&tool, "SHOW default_transaction_read_only").await,
        "on"
    );
    assert_eq!(value(&tool, "SHOW statement_timeout").await, "5s");

    // SET and PREPARE don't run at all.
    let message = failure(&tool, "SET work_mem = '1234kB'").await;
    assert!(message.contains("not SET"), "{message}");
    let message = failure(&tool, "PREPARE p AS SELECT 1").await;
    assert!(message.contains("not PREPARE"), "{message}");

    // All of it on the pool's one connection.
    assert_eq!(value(&tool, "SELECT pg_backend_pid()").await, pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_call_cancels_its_statement() {
    let (db, tool) = tool(1).await;
    let sql = "SELECT pg_sleep(4.5) AS abandoned";
    let call = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.run(QueryArgs {
                sql: sql.to_owned(),
                max_rows: None,
            })
            .await
        }
    });
    let admin = db.admin().await.unwrap();
    let running = || async {
        admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND query = $1 AND state = 'active'",
                &[&sql],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    let deadline = Instant::now() + Duration::from_secs(4);
    while running().await == 0 {
        assert!(Instant::now() < deadline, "the statement never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    call.abort();
    let abandoned = Instant::now();
    while running().await > 0 {
        assert!(
            abandoned.elapsed() < Duration::from_secs(2),
            "the abandoned statement is still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The pool's one connection was closed rather than returned; the next
    // call opens another.
    assert_eq!(value(&tool, "SELECT 1").await, "1");
}
