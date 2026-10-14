//! Tool calls over HTTP, as crawler sessions make them, while find_duplicates
//! calls are stuck: ingest_status still answers and the jobs still run, a
//! stuck call runs no longer than the tool timeout, and one whose client gives
//! up is cancelled with its statement.

use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use pghacker_core::config::Env;
use pghacker_core::mcp::{McpServer, router};
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, jobs, tools};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Calls tool `name` as an MCP client does, without a session: the reply, or
/// an error once `client` gives up.
async fn call(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    arguments: Value,
) -> reqwest::Result<Value> {
    let text = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": { "name": name, "arguments": arguments } }))
        .send()
        .await?
        .text()
        .await?;
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map_or(text.as_str(), str::trim);
    Ok(serde_json::from_str(json).unwrap_or(Value::String(text.clone())))
}

/// Whether `check` holds within `within`.
async fn until<F, Fut>(within: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = Instant::now();
    while started.elapsed() < within {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Tool calls' statements waiting on a lock.
async fn stuck_statements(admin: &tokio_postgres::Client) -> i64 {
    admin
        .query_one(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND application_name = 'pghacker-indexer tools'
               AND wait_event_type = 'Lock'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn stuck_find_duplicates_calls_stall_neither_status_nor_jobs() {
    let db = TestDb::create().await.unwrap();
    // The administrator sees every connection's waits; the indexer's role
    // reads the jobs.
    let admin = db.admin().await.unwrap();
    let client = db.indexer().await.unwrap();
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let env = Env::from_pairs([("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str())]);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    settings.workers = 2;
    settings.tool_connections = 3;
    settings.tool_timeout = Duration::from_secs(6);
    let indexer = Indexer::new(settings, &env).unwrap();

    let server = McpServer::new(
        indexer.clone(),
        "pghacker-indexer",
        "test",
        tools::INSTRUCTIONS,
    )
    .cancel_on_disconnect();
    let app = router(server, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // The workers, once each recurring job has run.
    let shutdown = CancellationToken::new();
    let workers = tokio::spawn(jobs::run(indexer.clone(), 2, shutdown.clone()));
    let ran = until(Duration::from_secs(60), || async {
        client
            .query_one(
                "SELECT count(*) > 0 AND count(*) FILTER (WHERE finished_at IS NULL) = 0
                 FROM jobs WHERE key = 'recurring'",
                &[],
            )
            .await
            .unwrap()
            .get(0)
    })
    .await;
    assert!(ran, "the recurring jobs didn't run");

    // find_duplicates reads distinct_pairs, which nothing else here does:
    // locked, its calls get stuck.
    let mut blocker = db.indexer().await.unwrap();
    let lock = blocker.transaction().await.unwrap();
    lock.batch_execute("LOCK TABLE distinct_pairs IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();

    let patient = reqwest::Client::new();
    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let started = Instant::now();
    let stuck = tokio::spawn({
        let (client, base) = (patient.clone(), base.clone());
        async move {
            call(
                &client,
                &base,
                "find_duplicates",
                json!({ "kind": "decision" }),
            )
            .await
        }
    });
    assert!(
        until(Duration::from_secs(3), || async {
            stuck_statements(&admin).await == 1
        })
        .await
    );
    let gave_up: Vec<_> = (0..3)
        .map(|_| {
            let (client, base) = (impatient.clone(), base.clone());
            tokio::spawn(async move {
                call(
                    &client,
                    &base,
                    "find_duplicates",
                    json!({ "kind": "feature" }),
                )
                .await
            })
        })
        .collect();
    // Two find_duplicates at a time, at most: the others wait for room, not
    // for one of the three connections.
    assert!(
        until(Duration::from_secs(3), || async {
            stuck_statements(&admin).await == 2
        })
        .await
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stuck_statements(&admin).await, 2);

    // ingest_status still answers, at once.
    for _ in 0..3 {
        let asked = Instant::now();
        let status = call(&patient, &base, "ingest_status", json!({}))
            .await
            .unwrap();
        assert!(
            status["result"]["structuredContent"]["jobs"].is_array(),
            "{status}"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
    }

    // The jobs still run.
    let woken: DateTime<Utc> = client.query_one("SELECT now()", &[]).await.unwrap().get(0);
    jobs::wake(&client, jobs::kind::REPAIR_STATS).await.unwrap();
    let repaired = until(Duration::from_secs(4), || async {
        client
            .query_one(
                "SELECT finished_at > $1 FROM jobs WHERE kind = $2 AND key = 'recurring'",
                &[&woken, &jobs::kind::REPAIR_STATS],
            )
            .await
            .unwrap()
            .get::<_, Option<bool>>(0)
            .unwrap_or(false)
    })
    .await;
    assert!(repaired, "the woken job didn't run");
    assert!(!stuck.is_finished(), "the stuck call should still wait");

    // The impatient clients gave up: their calls were abandoned, and the
    // statement of the one that held a connection was cancelled.
    for call in gave_up {
        assert!(
            call.await.unwrap().is_err(),
            "an impatient client got an answer"
        );
    }
    assert!(
        until(Duration::from_secs(2), || async {
            stuck_statements(&admin).await == 1
        })
        .await,
        "an abandoned call's statement still waits"
    );

    // The patient call fails at the tool timeout, not after an hour.
    let answer = stuck.await.unwrap().unwrap();
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("statement timeout"), "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(stuck_statements(&admin).await, 0);

    // Unlocked, find_duplicates answers at once, on every connection.
    lock.rollback().await.unwrap();
    for _ in 0..4 {
        let asked = Instant::now();
        let answer = call(
            &patient,
            &base,
            "find_duplicates",
            json!({ "kind": "feature" }),
        )
        .await
        .unwrap();
        assert!(
            answer["result"]["structuredContent"]["pairs"].is_array(),
            "{answer}"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "{:?}",
            asked.elapsed()
        );
    }

    shutdown.cancel();
    workers.await.unwrap().unwrap();
}
