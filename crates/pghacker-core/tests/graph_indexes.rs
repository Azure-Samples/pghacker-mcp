use pghacker_core::graph_indexes::{self, Kind, State};
use pghacker_core::{EDGE_LABELS, VERTEX_LABELS, migrate, testing::TestDb};

#[tokio::test]
async fn fresh_indexes_are_concrete_nonunique_and_role_correct() {
    let db = TestDb::create().await.unwrap();
    let indexer = db.indexer().await.unwrap();
    let requirements = graph_indexes::inspect(&indexer).await.unwrap();
    assert_eq!(
        requirements
            .iter()
            .filter(|r| r.kind == Kind::EdgeId)
            .count(),
        EDGE_LABELS.len()
    );
    assert_eq!(
        requirements
            .iter()
            .filter(|r| r.kind == Kind::VertexId)
            .count(),
        VERTEX_LABELS.len()
    );
    assert!(requirements.iter().all(|r| r.state == State::Ready));
    let unique: i64 = indexer
        .query_one(
            "SELECT count(*) FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace
             WHERE n.nspname = 'pghacker_graph' AND t.relname = ANY($1)
               AND i.indisunique",
            &[&EDGE_LABELS],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(unique, 0);
    let denied = indexer
        .batch_execute("CREATE INDEX forbidden_indexer_id ON pghacker_graph.\"MENTIONS\" (id)")
        .await
        .unwrap_err();
    assert_eq!(
        denied.code(),
        Some(&tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    );
}

#[tokio::test]
async fn missing_existing_id_indexes_are_not_rebuilt_at_startup() {
    let db = TestDb::create().await.unwrap();
    let mut admin = db.admin().await.unwrap();
    admin
        .batch_execute("DROP INDEX pghacker_graph.\"MENTIONS_id_idx\"")
        .await
        .unwrap();
    let error = migrate::bootstrap_database(&mut admin).await.unwrap_err();
    assert!(format!("{error:#}").contains("Missing"), "{error:#}");
    let mut indexer = db.indexer().await.unwrap();
    assert!(migrate::migrate(&mut indexer).await.is_err());
    admin
        .batch_execute("CREATE INDEX equivalent_mentions_id ON pghacker_graph.\"MENTIONS\" (id)")
        .await
        .unwrap();
    migrate::bootstrap_database(&mut admin).await.unwrap();
    assert!(migrate::migrate(&mut indexer).await.unwrap().is_empty());
    let requirement = graph_indexes::inspect(&indexer)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.kind == Kind::EdgeId && r.label.as_deref() == Some("MENTIONS"))
        .unwrap();
    assert_eq!(requirement.usable_indexes, ["equivalent_mentions_id"]);
}

#[tokio::test]
async fn populated_upgrade_requires_preparation_and_preserves_mappings() {
    let db = TestDb::create().await.unwrap();
    let mut indexer = db.indexer().await.unwrap();
    indexer
        .batch_execute(
            "INSERT INTO graph_nodes (key, label, graph_id, props) VALUES
             ('message:one', 'Message', '1'::ag_catalog.graphid, '{}'),
             ('message:two', 'Message', '2'::ag_catalog.graphid, '{}');
         INSERT INTO graph_edges (label, start_key, end_key, graph_id)
             VALUES ('MENTIONS', 'message:one', 'message:two', '3'::ag_catalog.graphid);
         DROP INDEX pghacker.graph_edges_start_key_idx;
         DROP INDEX pghacker.graph_edges_end_key_idx;
         DELETE FROM schema_migrations WHERE version = '0009_graph_indexes';",
        )
        .await
        .unwrap();
    let error = migrate::migrate(&mut indexer).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("populated graph_edges"),
        "{error:#}"
    );
    let recorded: bool = indexer
        .query_one(
            "SELECT EXISTS (SELECT FROM schema_migrations WHERE version = '0009_graph_indexes')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!recorded);
    indexer.batch_execute(
        "CREATE INDEX CONCURRENTLY alternate_outgoing ON pghacker.graph_edges (start_key, label, end_key)"
    ).await.unwrap();
    indexer.batch_execute(
        "CREATE INDEX CONCURRENTLY alternate_incoming ON pghacker.graph_edges (end_key, label, start_key)"
    ).await.unwrap();
    assert_eq!(
        migrate::migrate(&mut indexer).await.unwrap(),
        ["0009_graph_indexes"]
    );
    let edge: (String, String, String) = {
        let row = indexer
            .query_one(
                "SELECT start_key, end_key, graph_id::text FROM graph_edges",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    };
    assert_eq!(
        edge,
        ("message:one".into(), "message:two".into(), "3".into())
    );
}

#[tokio::test]
async fn empty_setup_refuses_a_competing_writer_lock() {
    let db = TestDb::create().await.unwrap();
    let mut indexer = db.indexer().await.unwrap();
    indexer
        .batch_execute(
            "DROP INDEX pghacker.graph_edges_start_key_idx;
         DROP INDEX pghacker.graph_edges_end_key_idx;
         DELETE FROM schema_migrations WHERE version = '0009_graph_indexes';",
        )
        .await
        .unwrap();
    let mut writer = db.indexer().await.unwrap();
    let tx = writer.transaction().await.unwrap();
    tx.batch_execute("LOCK TABLE ONLY pghacker.graph_edges IN ROW EXCLUSIVE MODE")
        .await
        .unwrap();
    let error = migrate::migrate(&mut indexer).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("cannot be locked"),
        "{error:#}"
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        migrate::migrate(&mut indexer).await.unwrap(),
        ["0009_graph_indexes"]
    );
}

#[tokio::test]
async fn operator_protocol_and_recovery_are_exercised_on_a_private_fixture() {
    let db = TestDb::create().await.unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = tokio::process::Command::new("node")
        .args([
            "--test",
            "--test-concurrency=1",
            "test/graph-indexes.test.mjs",
            "test/psql.test.mjs",
        ])
        .current_dir(root)
        .env("PGHACKER_INDEX_TEST_URL", &db.admin_url)
        .env("PGHACKER_INDEX_TEST_READER_URL", &db.reader_url)
        .env_remove("PGH_READER_PASSWORD")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "operator tests failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
