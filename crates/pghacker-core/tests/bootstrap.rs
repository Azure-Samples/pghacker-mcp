//! Bootstrap and migrations on a fresh database, and what each role may do.

use pghacker_core::migrate;
use pghacker_core::testing::TestDb;

#[tokio::test]
async fn bootstrap_and_migrations_are_idempotent() {
    let db = TestDb::create().await.unwrap();
    let mut admin = db.admin().await.unwrap();
    migrate::bootstrap_database(&mut admin).await.unwrap();
    let mut indexer = db.indexer().await.unwrap();
    let applied = migrate::migrate(&mut indexer).await.unwrap();
    assert!(applied.is_empty(), "a second run applied {applied:?}");
    let count: i64 = indexer
        .query_one("SELECT count(*) FROM schema_migrations", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count as usize, migrate::migrations().len());
}

#[tokio::test]
async fn the_reader_reads_but_cannot_write_or_see_emails() {
    let db = TestDb::create().await.unwrap();
    let indexer = db.indexer().await.unwrap();
    indexer
        .batch_execute(
            "INSERT INTO authors (id, display_name) VALUES (1, 'Tom Lane');
             INSERT INTO author_emails (email, author_id) VALUES ('tgl@example.org', 1);",
        )
        .await
        .unwrap();
    let reader = db.reader().await.unwrap();
    let name: String = reader
        .query_one("SELECT display_name FROM authors WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, "Tom Lane");
    let denied = reader
        .query("SELECT * FROM author_emails", &[])
        .await
        .unwrap_err();
    assert!(
        format!("{denied:?}").contains("permission denied"),
        "{denied:?}"
    );
    let denied = reader.query("SELECT * FROM jobs", &[]).await.unwrap_err();
    assert!(
        format!("{denied:?}").contains("permission denied"),
        "{denied:?}"
    );
    let refused = reader
        .execute("INSERT INTO authors (display_name) VALUES ('x')", &[])
        .await
        .unwrap_err();
    assert!(format!("{refused:?}").contains("read-only"), "{refused:?}");
    let timeout: String = reader
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(timeout, "5s");
}

#[tokio::test]
async fn the_reader_cannot_see_other_schemas() {
    let db = TestDb::create().await.unwrap();
    let admin = db.admin().await.unwrap();
    admin
        .batch_execute(
            "CREATE SCHEMA pilotswarm_like; CREATE TABLE pilotswarm_like.sessions (id int);
             CREATE TABLE public.facts (id int);",
        )
        .await
        .unwrap();
    let reader = db.reader().await.unwrap();
    for table in ["pilotswarm_like.sessions", "public.facts"] {
        let denied = reader
            .query(&format!("SELECT * FROM {table}"), &[])
            .await
            .unwrap_err();
        assert!(
            format!("{denied:?}").contains("permission denied"),
            "{table}: {denied:?}"
        );
    }
}

#[tokio::test]
async fn the_indexer_writes_the_graph_and_the_reader_queries_it_read_only() {
    let db = TestDb::create().await.unwrap();
    let indexer = db.indexer().await.unwrap();
    indexer
        .batch_execute(
            r#"
            WITH l AS (SELECT id FROM ag_catalog.ag_label WHERE name = 'Position'
                       AND graph = (SELECT graphid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))
            INSERT INTO pghacker_graph."Position" (id, properties)
            SELECT ag_catalog._graphid(l.id, nextval('pghacker_graph."Position_id_seq"')),
                   jsonb_build_object('key', 'position:1', 'stance', 'opposes')::text::ag_catalog.agtype FROM l;
            WITH l AS (SELECT id FROM ag_catalog.ag_label WHERE name = 'Feature'
                       AND graph = (SELECT graphid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))
            INSERT INTO pghacker_graph."Feature" (id, properties)
            SELECT ag_catalog._graphid(l.id, nextval('pghacker_graph."Feature_id_seq"')),
                   jsonb_build_object('key', 'feature:sql-pgq', 'name', 'SQL/PGQ')::text::ag_catalog.agtype FROM l;
            WITH l AS (SELECT id FROM ag_catalog.ag_label WHERE name = 'ABOUT'
                       AND graph = (SELECT graphid FROM ag_catalog.ag_graph WHERE name = 'pghacker_graph'))
            INSERT INTO pghacker_graph."ABOUT" (id, start_id, end_id, properties)
            SELECT ag_catalog._graphid(l.id, nextval('pghacker_graph."ABOUT_id_seq"')), p.id, f.id, '{}'::ag_catalog.agtype
            FROM l, pghacker_graph."Position" p, pghacker_graph."Feature" f;
            "#,
        )
        .await
        .unwrap();
    let mut reader = db.reader().await.unwrap();
    let tx = reader
        .build_transaction()
        .read_only(true)
        .start()
        .await
        .unwrap();
    let rows = tx
        .query(
            "SELECT f::text FROM ag_catalog.cypher('pghacker_graph', $$
                 MATCH (p:Position)-[:ABOUT]->(f:Feature) RETURN f.name
             $$) AS (f ag_catalog.agtype)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "SQL/PGQ");
    let refused = tx
        .query(
            "SELECT * FROM ag_catalog.cypher('pghacker_graph', $$ CREATE (:Feature {key: 'x'}) $$) \
             AS (x ag_catalog.agtype)",
            &[],
        )
        .await;
    assert!(refused.is_err());
}
