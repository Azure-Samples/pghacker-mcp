// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Bootstrap and migrations on a fresh database, and what each role may do.

use pghacker_core::config::{Env, with_credentials, with_database};
use pghacker_core::testing::TestDb;
use pghacker_core::{db, migrate};
use tokio_postgres::error::SqlState;

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
    assert_eq!(timeout, "10s");
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

/// The bootstrap of a database it creates, with PilotSwarm's own role: it
/// creates its schemas and can't read schema pghacker.
#[tokio::test]
async fn bootstrap_creates_the_database_and_a_least_privileged_swarm_role() {
    let env = Env::local().unwrap();
    let admin = env.admin_database_url().unwrap();
    let name = format!("pgh_test_{}", uuid::Uuid::new_v4().simple());
    let url = with_database(&admin, &name).unwrap();
    assert!(migrate::create_database_if_missing(&url).await.unwrap());
    let result = async {
        assert!(!migrate::create_database_if_missing(&url).await?);
        let mut passwords = env.passwords()?;
        let swarm_password = format!("swarm-{}", uuid::Uuid::new_v4().simple());
        passwords.swarm = Some(swarm_password.clone());
        let mut client = db::connect(&url, "pghacker-test").await?;
        migrate::bootstrap(&mut client, &passwords).await?;
        let indexer_url = with_database(&env.indexer_database_url()?, &name)?;
        migrate::migrate(&mut db::connect(&indexer_url, "pghacker-test").await?).await?;

        let swarm = db::connect(
            &with_credentials(&url, "pgh_swarm", &swarm_password)?,
            "pghacker-test",
        )
        .await?;
        swarm
            .batch_execute("CREATE SCHEMA swarm_owned; CREATE TABLE swarm_owned.t (x int);")
            .await?;
        let denied = swarm
            .query("SELECT count(*) FROM pghacker.messages", &[])
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
        let settings: Vec<String> = client
            .query(
                "SELECT s FROM pg_db_role_setting d, unnest(d.setconfig) s
                 WHERE d.setrole = 'pgh_reader'::regrole AND d.setdatabase = 0
                   AND s LIKE 'hnsw.%'
                 ORDER BY s",
                &[],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            settings,
            [
                "hnsw.ef_search=100",
                "hnsw.iterative_scan=relaxed_order",
                "hnsw.max_scan_tuples=5000",
            ]
        );
        anyhow::Ok(())
    }
    .await;
    let maintenance = db::connect(&with_database(&admin, "postgres").unwrap(), "pghacker-test")
        .await
        .unwrap();
    maintenance
        .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .await
        .unwrap();
    result.unwrap();
}
