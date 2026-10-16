// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The mail and graph tools on a synthetic corpus, as the read-only role.

use pghacker_api::{extracts, ontology, people, threads};
use pghacker_core::embed::{EmbedConfig, Embedder};
use pghacker_core::testing::TestDb;
use serde_json::{Value, json};

const FIXTURE: &str = r#"
INSERT INTO authors (id, display_name, message_count, first_seen, last_seen) VALUES
    (1, 'Alice Hacker', 2, '2026-06-01', '2026-06-03'),
    (2, 'Bob Reviewer', 2, '2026-06-02', '2026-06-10');
INSERT INTO author_names (author_id, name, count) VALUES (1, 'Alice Hacker', 2), (2, 'Bob Reviewer', 2), (2, 'B. Reviewer', 1);
INSERT INTO author_emails (email, author_id) VALUES ('alice@example.org', 1), ('bob@example.org', 2);
INSERT INTO sources (kind, name, config) VALUES ('mailing_list', 'pgsql-hackers', '{"follow": true}');
INSERT INTO threads (id, list, root_message_id, subject, started_at, last_at, message_count, participant_count) VALUES
    (10, 'pgsql-hackers', 'm1@example.org', 'JSON_TABLE plan clause', '2026-06-01', '2026-06-03', 3, 2),
    (11, 'pgsql-hackers', 'm4@example.org', 'Vacuum tuning', '2026-06-10', '2026-06-10', 1, 1);
INSERT INTO messages (id, message_id, list, thread_id, parent_message_id, author_id, from_name, subject, sent_at, body, body_clean, has_patch) VALUES
    (100, 'm1@example.org', 'pgsql-hackers', 10, NULL, 1, 'Alice Hacker', 'JSON_TABLE plan clause', '2026-06-01',
     'Here is a patch adding the PLAN clause to JSON_TABLE, as the standard describes.',
     'Here is a patch adding the PLAN clause to JSON_TABLE, as the standard describes.', true),
    (101, 'm2@example.org', 'pgsql-hackers', 10, 'm1@example.org', 2, 'Bob Reviewer', 'Re: JSON_TABLE plan clause', '2026-06-02',
     '> Here is a patch\nI am not convinced the PLAN clause is worth its complexity in the executor.',
     'I am not convinced the PLAN clause is worth its complexity in the executor.', false),
    (102, 'm3@example.org', 'pgsql-hackers', 10, 'm2@example.org', 1, 'Alice Hacker', 'Re: JSON_TABLE plan clause', '2026-06-03',
     'Fair point; I will simplify the nested path handling first.',
     'Fair point; I will simplify the nested path handling first.', false),
    (103, 'm4@example.org', 'pgsql-hackers', 11, NULL, 2, 'Bob Reviewer', 'Vacuum tuning', '2026-06-10',
     'Autovacuum cost limits are too low for modern disks.', 'Autovacuum cost limits are too low for modern disks.', false);
INSERT INTO message_chunks (message_id, thread_id, ord, content)
SELECT id, thread_id, 0, subject || E'\n' || body_clean FROM messages;
INSERT INTO patches (message_id, filename, series_version, part, paths, added, removed, content) VALUES
    (100, 'v1-0001-json-table-plan.patch', '1', 1, '{src/backend/parser/parse_jsontable.c}', 120, 4, 'diff');
INSERT INTO anchors (message_id, kind, value, origin) VALUES
    (100, 'file', 'src/backend/parser/parse_jsontable.c', 'patch'), (100, 'release', '19', 'body');
INSERT INTO features (id, slug, name, description, status, release) VALUES
    (1, 'json-table-plan', 'JSON_TABLE PLAN clause', 'The SQL standard PLAN clause for JSON_TABLE nested paths.', 'proposed', '19');
INSERT INTO feature_threads (feature_id, thread_id) VALUES (1, 10);
INSERT INTO feature_files (feature_id, path, thread_id) VALUES (1, 'src/backend/parser/parse_jsontable.c', 10);
INSERT INTO decisions (id, feature_id, thread_id, title, outcome, rationale, decided_at) VALUES
    (1, 1, 10, 'Simplify nested paths before PLAN', 'Simplify the nested path handling first', 'Reviewer doubts the complexity', '2026-06-03');
INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary) VALUES
    (1, 2, 10, 1, 'concerned', 'Doubts that the PLAN clause is worth its executor complexity.'),
    (2, 1, 10, 1, 'proposes', 'Proposes the PLAN clause for JSON_TABLE per the standard.');
INSERT INTO citations (extract_kind, extract_id, message_id, quote) VALUES
    ('feature', 1, 100, 'adding the PLAN clause to JSON_TABLE'),
    ('decision', 1, 102, 'I will simplify the nested path handling first'),
    ('position', 1, 101, 'not convinced the PLAN clause is worth its complexity'),
    ('position', 2, 100, 'adding the PLAN clause to JSON_TABLE');
"#;

async fn corpus() -> TestDb {
    let db = TestDb::create().await.unwrap();
    db.indexer()
        .await
        .unwrap()
        .batch_execute(FIXTURE)
        .await
        .unwrap();
    db
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn no_emails(value: &Value) {
    let text = value.to_string();
    assert!(
        !text.contains("@example.org>") && !text.contains("alice@") && !text.contains("bob@"),
        "{text}"
    );
}

#[tokio::test]
async fn searches_threads_by_full_text_and_falls_back_from_hybrid() {
    let db = corpus().await;
    let reader = db.reader().await.unwrap();
    let out = threads::search_threads(
        &reader,
        None,
        args(json!({ "query": "PLAN clause", "mode": "fulltext" })),
    )
    .await
    .unwrap();
    assert_eq!(out["threads"][0]["thread_id"], 10, "{out:#}");
    assert_eq!(out["threads"][0]["features"][0]["slug"], "json-table-plan");
    assert!(
        out["threads"][0]["matches"][0]["message_id"]
            .as_str()
            .unwrap()
            .ends_with("@example.org")
    );
    assert_eq!(out["threads"].as_array().unwrap().len(), 1);

    let hybrid = threads::search_threads(&reader, None, args(json!({ "query": "autovacuum" })))
        .await
        .unwrap();
    assert_eq!(hybrid["mode"], "fulltext");
    assert!(hybrid["notes"][0].as_str().unwrap().contains("fell back"));
    assert_eq!(hybrid["threads"][0]["thread_id"], 11);

    let semantic = threads::search_threads(
        &reader,
        None,
        args(json!({ "query": "x", "mode": "semantic" })),
    )
    .await;
    assert!(semantic.is_err());

    let by_author = threads::search_threads(
        &reader,
        None,
        args(json!({ "query": "PLAN clause", "mode": "fulltext", "author": "Bob Reviewer" })),
    )
    .await
    .unwrap();
    assert_eq!(
        by_author["threads"][0]["matches"][0]["message_id"],
        "m2@example.org"
    );
    let dated = threads::search_threads(
        &reader,
        None,
        args(json!({ "query": "PLAN clause", "mode": "fulltext", "since": "2026-06-05" })),
    )
    .await
    .unwrap();
    assert!(dated["threads"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn gets_a_thread_by_message_id_with_extracts() {
    let db = corpus().await;
    let reader = db.reader().await.unwrap();
    let out = threads::get_thread(&reader, args(json!({ "message_id": "<m2@example.org>" })))
        .await
        .unwrap();
    assert_eq!(out["thread_id"], 10);
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["in_reply_to"], "m1@example.org");
    assert!(!messages[1]["text"].as_str().unwrap().starts_with('>'));
    assert_eq!(
        messages[0]["patches"][0]["files"][0],
        "src/backend/parser/parse_jsontable.c"
    );
    assert_eq!(out["extracts"]["features"][0]["slug"], "json-table-plan");
    assert_eq!(out["extracts"]["positions"].as_array().unwrap().len(), 2);
    assert!(out["next_offset"].is_null());
    let paged = threads::get_thread(&reader, args(json!({ "thread_id": 10, "limit": 2 })))
        .await
        .unwrap();
    assert_eq!(paged["next_offset"], 2);
    let full = threads::get_thread(&reader, args(json!({ "thread_id": 10, "full_text": true })))
        .await
        .unwrap();
    assert!(
        full["messages"][1]["text"]
            .as_str()
            .unwrap()
            .starts_with('>')
    );
    no_emails(&out);
}

#[tokio::test]
async fn finds_authors_and_their_perspective() {
    let db = corpus().await;
    let reader = db.reader().await.unwrap();
    let found = people::search_authors(&reader, args(json!({ "query": "Reviewer" })))
        .await
        .unwrap();
    assert_eq!(found["authors"][0]["author_id"], 2);
    assert_eq!(found["authors"][0]["other_names"][0], "B. Reviewer");

    let author = people::get_author(&reader, args(json!({ "author": 2 })))
        .await
        .unwrap();
    assert_eq!(author["name"], "Bob Reviewer");
    assert_eq!(author["positions_by_stance"]["concerned"], 1);
    assert_eq!(author["features"][0]["slug"], "json-table-plan");

    let perspective = people::author_perspective(
        &reader,
        None,
        args(json!({ "author": "Bob Reviewer", "topic": "PLAN clause complexity", "mode": "fulltext" })),
    )
    .await
    .unwrap();
    assert_eq!(
        perspective["positions"][0]["stance"], "concerned",
        "{perspective:#}"
    );
    assert_eq!(
        perspective["positions"][0]["citations"][0]["message_id"],
        "m2@example.org"
    );
    assert_eq!(perspective["messages"][0]["message_id"], "m2@example.org");
    no_emails(&perspective);

    let none = people::author_perspective(
        &reader,
        None,
        args(json!({ "author": 2, "topic": "autovacuum", "mode": "fulltext" })),
    )
    .await
    .unwrap();
    assert!(none["positions"].as_array().unwrap().is_empty());
    assert!(none["note"].is_string());
    assert_eq!(none["messages"][0]["message_id"], "m4@example.org");
}

/// An embeddings endpoint with fixed unit vectors: text that mentions "plan"
/// points along the first axis, anything else along the second.
async fn fake_embedder() -> Embedder {
    async fn embeddings(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
        let data: Vec<Value> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, input)| {
                let mut vector = vec![0.0f32; 1536];
                let plan = input.as_str().unwrap().to_lowercase().contains("plan");
                vector[usize::from(!plan)] = 1.0;
                json!({ "index": index, "embedding": vector })
            })
            .collect();
        axum::Json(json!({ "data": data }))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().route("/", axum::routing::post(embeddings));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Embedder::new(EmbedConfig {
        url,
        model: "test".into(),
        dim: 1536,
        api_key: None,
        api_key_header: "api-key".into(),
        bearer: false,
    })
    .unwrap()
}

/// The unit vector along `axis` (1-based), as SQL.
fn axis(axis: usize) -> String {
    format!(
        "(SELECT array_agg(CASE WHEN g = {axis} THEN 1 ELSE 0 END ORDER BY g)::real[]::vector::halfvec
          FROM generate_series(1, 1536) g)"
    )
}

#[tokio::test]
async fn narrows_an_author_perspective_to_the_topic() {
    let db = corpus().await;
    db.indexer()
        .await
        .unwrap()
        .batch_execute(&format!(
            "INSERT INTO features (id, slug, name, description, status) VALUES
                 (2, 'autovacuum-cost-limits', 'Autovacuum cost limits', 'Raise the autovacuum cost limits.', 'proposed');
             INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary) VALUES
                 (3, 2, 11, 2, 'proposes', 'Wants a plan to raise the limits for modern disks.');
             UPDATE features SET embedding = CASE id WHEN 1 THEN {plan} ELSE {other} END;
             UPDATE positions SET embedding = {far};
             UPDATE message_chunks SET embedding = CASE thread_id WHEN 10 THEN {plan} ELSE {other} END;",
            plan = axis(1),
            other = axis(2),
            far = axis(3),
        ))
        .await
        .unwrap();
    let embedder = fake_embedder().await;
    let reader = db.reader().await.unwrap();
    for mode in ["semantic", "hybrid"] {
        let out = people::author_perspective(
            &reader,
            Some(&embedder),
            args(json!({ "author": 2, "topic": "the PLAN clause", "mode": mode })),
        )
        .await
        .unwrap();
        // Position 1's summary is far from the topic but its feature is the
        // topic. Position 3 mentions a plan but is about another feature, and
        // the vacuum message is off topic: both drop out.
        let positions: Vec<i64> = out["positions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["position_id"].as_i64().unwrap())
            .collect();
        assert_eq!(positions, [1], "{mode}: {out:#}");
        assert_eq!(out["stances"], json!({ "concerned": 1 }));
        let messages: Vec<&str> = out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["message_id"].as_str().unwrap())
            .collect();
        assert_eq!(messages, ["m2@example.org"], "{mode}: {out:#}");
    }
    let fulltext = people::author_perspective(
        &reader,
        None,
        args(json!({ "author": 2, "topic": "the PLAN clause", "mode": "fulltext" })),
    )
    .await
    .unwrap();
    assert_eq!(fulltext["positions"].as_array().unwrap().len(), 2);
    assert!((people::topic_cutoff(0.2) - 0.45).abs() < 1e-9);
    assert!((people::topic_cutoff(0.6) - 0.7).abs() < 1e-9);
}

#[tokio::test]
async fn adds_positions_on_the_paths_and_keeps_only_evidence_from_before_until() {
    let db = corpus().await;
    // Bob's position on a vacuum feature, cited in his message of 2026-06-10.
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO features (id, slug, name, description, status) VALUES
                 (2, 'autovacuum-cost-limits', 'Autovacuum cost limits', 'Raise the autovacuum cost limits.', 'proposed');
             INSERT INTO feature_files (feature_id, path, thread_id) VALUES (2, 'src/backend/postmaster/autovacuum.c', 11);
             INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary) VALUES
                 (3, 2, 11, 2, 'proposes', 'Wants higher autovacuum cost limits for modern disks.');
             INSERT INTO citations (extract_kind, extract_id, message_id, quote) VALUES
                 ('position', 3, 103, 'cost limits are too low for modern disks');",
        )
        .await
        .unwrap();
    let reader = db.reader().await.unwrap();
    let perspective = |extra: Value| {
        let mut arguments =
            json!({ "author": 2, "topic": "autovacuum cost limits", "mode": "fulltext" });
        for (key, value) in extra.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        let reader = &reader;
        async move {
            people::author_perspective(reader, None, args(arguments))
                .await
                .unwrap()
        }
    };
    let ids = |out: &Value| -> Vec<i64> {
        out["positions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["position_id"].as_i64().unwrap())
            .collect()
    };

    // The topic finds the vacuum position; a folder of the parser adds the
    // position on the JSON_TABLE feature implemented there.
    let out =
        perspective(json!({ "paths": ["./src/backend/parser/", "src/backend/parser"] })).await;
    assert_eq!(ids(&out), [3, 1], "{out:#}");
    assert_eq!(out["positions"][0]["matched"], json!(["topic"]));
    assert_eq!(out["positions"][1]["matched"], json!(["paths"]));
    assert_eq!(out["paths"], json!(["src/backend/parser"]));
    // A file matches as itself; `_` in a path is a character, not a wildcard.
    let file = perspective(json!({ "paths": ["src/backend/parser/parse_jsontable.c"] })).await;
    assert_eq!(ids(&file), [3, 1], "{file:#}");
    let wildcard = perspective(json!({ "paths": ["src/backend/pars_r"] })).await;
    assert_eq!(ids(&wildcard), [3], "{wildcard:#}");
    // Without paths, no position says how it matched.
    let plain = perspective(json!({})).await;
    assert_eq!(ids(&plain), [3]);
    assert!(plain["positions"][0].get("matched").is_none());
    assert!(plain.get("paths").is_none());

    // Before Bob's vacuum message, he had no position on vacuum, nor any
    // message about it.
    let before =
        perspective(json!({ "until": "2026-06-10", "paths": ["src/backend/parser"] })).await;
    assert_eq!(ids(&before), [1], "{before:#}");
    assert_eq!(before["until"], "2026-06-10");
    assert!(
        before["messages"].as_array().unwrap().is_empty(),
        "{before:#}"
    );
    // A position counts only once every message it cites came before.
    let earlier =
        perspective(json!({ "until": "2026-06-02", "paths": ["src/backend/parser"] })).await;
    assert!(ids(&earlier).is_empty(), "{earlier:#}");
    let plan = people::author_perspective(
        &reader,
        None,
        args(json!({ "author": 2, "topic": "PLAN clause complexity", "mode": "fulltext", "until": "2026-06-03" })),
    )
    .await
    .unwrap();
    assert_eq!(ids(&plan), [1], "{plan:#}");
    assert_eq!(plan["messages"][0]["message_id"], "m2@example.org");
}

#[tokio::test]
async fn searches_extracts_with_citations() {
    let db = corpus().await;
    let reader = db.reader().await.unwrap();
    let out = extracts::search_extracts(
        &reader,
        None,
        args(json!({ "query": "PLAN clause", "mode": "fulltext" })),
    )
    .await
    .unwrap();
    let results = out["results"].as_array().unwrap();
    assert!(results.len() >= 3, "{out:#}");
    let kinds: Vec<&str> = results
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"feature") && kinds.contains(&"position"));
    let feature = results.iter().find(|r| r["kind"] == "feature").unwrap();
    assert_eq!(
        feature["citations"][0]["quote"],
        "adding the PLAN clause to JSON_TABLE"
    );
    assert_eq!(feature["files"][0], "src/backend/parser/parse_jsontable.c");
    let decisions = extracts::search_extracts(
        &reader,
        None,
        args(json!({ "query": "nested path", "mode": "fulltext", "kind": "decision" })),
    )
    .await
    .unwrap();
    assert_eq!(decisions["results"][0]["kind"], "decision");
}

#[tokio::test]
async fn describes_the_commit_history_the_code_tools_read() {
    let db = corpus().await;
    let reader = db.reader().await.unwrap();
    let out = ontology::describe(&reader, "pghacker").await.unwrap();
    assert_eq!(
        out["loaded"]["code"],
        Value::Null,
        "no history is built yet"
    );
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "INSERT INTO code_history (master, url, head)
               VALUES ('master', 'https://git.postgresql.org/git/postgresql.git', repeat('a', 40));
             INSERT INTO code_lanes (id, name, fork_addr, lo, hi, head_addr) VALUES
               (0, 'master', NULL, 0, 1099511627776, 8589934592),
               (1, 'REL_19_STABLE', 4294967296, 4294967297, 4311744513, 4294967297);
             INSERT INTO code_commits (addr, sha, lane, committed_at, authored_at, author_name,
                                       committer_name, subject, message) VALUES
               (4294967296, repeat('b', 40), 0, '2026-01-01', '2026-01-01', 'A', 'A', 'b', 'b'),
               (8589934592, repeat('a', 40), 0, '2026-02-01', '2026-02-01', 'A', 'A', 'a', 'a'),
               (4294967297, repeat('c', 40), 1, '2026-03-01', '2026-03-01', 'A', 'A', 'c', 'c');
             INSERT INTO code_refs (kind, name, addr) VALUES
               ('branch', 'master', 8589934592), ('branch', 'REL_19_STABLE', 4294967297),
               ('tag', 'REL_19_0', 4294967297);",
        )
        .await
        .unwrap();
    let out = ontology::describe(&reader, "pghacker").await.unwrap();
    let code = &out["loaded"]["code"];
    assert_eq!((&code["commits"], &code["tags"]), (&json!(3), &json!(1)));
    let mut branches: Vec<&str> = code["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap())
        .collect();
    branches.sort();
    assert_eq!(branches, ["REL_19_STABLE", "master"]);
    // The commit the code tools read without a version.
    assert_eq!(
        code["default"],
        json!({ "ref": "REL_19_STABLE", "sha": "c".repeat(40) })
    );
    assert_eq!(code["paths"], "the whole tree");
    assert_eq!(code["url"], "https://git.postgresql.org/git/postgresql.git");
    let note = out["loaded"]["code_note"].as_str().unwrap();
    for needed in [
        "/branch/<name>/",
        "REL_17_STABLE@",
        "alive @> X",
        "git blame",
        "git repository (url)",
        "synced_at",
        "isn't listed in sources",
    ] {
        assert!(note.contains(needed), "{needed}");
    }
    // The history's SQL examples run as pgh_reader.
    let sql = out["examples"]["sql"].as_array().unwrap();
    assert_eq!(sql.len(), 5);
    for q in sql {
        let q = q.as_str().unwrap();
        if let Err(error) = reader.query(q, &[]).await {
            panic!("{q}: {error}");
        }
    }
}

#[tokio::test]
async fn describes_the_ontology() {
    let db = corpus().await;
    // Mail from two monthly archives with two months between them, and from
    // a thread page.
    db.indexer()
        .await
        .unwrap()
        .batch_execute(
            "UPDATE messages SET archive_month = CASE id WHEN 100 THEN '2026-03' WHEN 103 THEN NULL
                                                         ELSE '2026-06' END",
        )
        .await
        .unwrap();
    let reader = db.reader().await.unwrap();
    let out = match ontology::get_ontology(&reader).await {
        Ok(out) => out,
        Err(error) => panic!("{error:?}"),
    };
    assert_eq!(out["loaded"]["code"], Value::Null, "no history is built");
    assert!(
        out["loaded"]["code_note"]
            .as_str()
            .unwrap()
            .contains("commit history")
    );
    let tables: Vec<&str> = out["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["table"].as_str().unwrap())
        .collect();
    assert!(tables.contains(&"messages") && tables.contains(&"features"));
    assert!(tables.contains(&"code_commits") && !tables.contains(&"code_lines"));
    assert!(!tables.contains(&"author_emails") && !tables.contains(&"jobs"));
    assert!(
        !tables.contains(&"distinct_pairs"),
        "judged pairs are bookkeeping"
    );
    assert_eq!(out["graph"]["vertices"].as_array().unwrap().len(), 11);
    let feature = out["graph"]["vertices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["label"] == "Feature")
        .unwrap();
    for status in [
        "proposed",
        "in_progress",
        "committed",
        "reverted",
        "rejected",
        "returned",
        "withdrawn",
        "unknown",
    ] {
        assert!(
            feature["properties"].as_str().unwrap().contains(status),
            "{status}"
        );
    }
    assert_eq!(out["loaded"]["mail"]["messages"], 4);
    // The lists come from sources, not from the test slices' description.
    assert_eq!(
        out["loaded"]["mail"]["lists"],
        json!([{ "name": "pgsql-hackers", "enabled": true, "follow": true }])
    );
    assert!(out["loaded"]["mail"].get("note").is_none());
    // The months in brief: their range, and the months missing from it.
    assert_eq!(
        out["loaded"]["mail"]["months"],
        json!({ "first": "2026-03", "last": "2026-06", "count": 2, "missing": ["2026-04 to 2026-05"] })
    );

    // Columns with their types; arrays by their elements' type.
    let columns = |table: &str| -> String {
        out["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["table"] == table)
            .unwrap_or_else(|| panic!("{table}"))["columns"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        columns("messages").starts_with("id bigint, message_id text, list text, thread_id bigint")
    );
    assert!(columns("messages").contains("references_ids text[]"));
    assert!(columns("patches").contains("paths text[]"));
    assert!(columns("patches").contains("text_ids integer[]"));
    assert!(columns("code_history").contains("paths text[]"));
    assert!(columns("code_file_versions").contains("alive int8multirange"));
    assert!(columns("threads").contains("last_at timestamp with time zone"));
    assert!(!out["tables"].to_string().contains("ARRAY"));
    assert!(
        !columns("features").contains("embedding") && !columns("features").contains("search_text")
    );

    // The schema's comments describe every table listed and all its
    // columns, the ones the list leaves out too; get_ontology leaves the
    // comments out, and says how to read them, with SQL the query tool takes.
    let note = out["tables_note"].as_str().unwrap();
    for sql in [ontology::TABLE_COMMENTS_SQL, ontology::COLUMN_COMMENTS_SQL] {
        assert!(note.contains(&format!("`{sql}`")), "{note}");
        if let Err(error) = pghacker_sqlquery::check(sql) {
            panic!("{sql}: {error}");
        }
    }
    let described: Vec<(String, Option<String>)> = reader
        .query(ontology::TABLE_COMMENTS_SQL, &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    for table in &tables {
        let description = described.iter().find(|(t, _)| t == table);
        assert!(matches!(description, Some((_, Some(_)))), "{table}");
    }
    assert!(
        !described
            .iter()
            .any(|(t, _)| t == "author_emails" || t == "jobs")
    );
    let message_columns = reader
        .query(ontology::COLUMN_COMMENTS_SQL, &[])
        .await
        .unwrap();
    assert_eq!(message_columns.len(), 18);
    for row in &message_columns {
        assert!(
            row.get::<_, Option<String>>(2).is_some(),
            "{}",
            row.get::<_, String>(0)
        );
    }
    let undescribed: Vec<String> = reader
        .query(
            "SELECT c.relname || '.' || a.attname FROM pg_class c
             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
             WHERE c.relnamespace = 'pghacker'::regnamespace AND c.relname = ANY($1)
               AND col_description(c.oid, a.attnum) IS NULL",
            &[&tables],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(undescribed, Vec::<String>::new());
    assert!(
        !out.to_string()
            .contains("without angle brackets: what answers cite")
    );

    // The examples pass the query tool's checks; the first finds positions
    // on SQL/PGQ by its slug.
    let examples = &out["examples"];
    for example in examples["sql"]
        .as_array()
        .unwrap()
        .iter()
        .chain(examples["cypher"].as_array().unwrap())
    {
        let sql = example.as_str().unwrap();
        if let Err(error) = pghacker_sqlquery::check(sql) {
            panic!("{sql}: {error}");
        }
    }
    assert!(
        examples["cypher"][0]
            .as_str()
            .unwrap()
            .contains("(f:Feature {slug: 'sql-property-graph-queries'})")
    );
}
