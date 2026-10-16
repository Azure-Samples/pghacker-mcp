// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Threading stored mail again: mail that old threading glued together, its
//! dry run's report, the apply of the reviewed plan, and where extracts go;
//! on fresh databases, without the network.

use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, extraction, jobs, repair, rethread};
use serde_json::{Value, json};
use tokio_postgres::Client;

async fn indexer(db: &TestDb) -> Indexer {
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let env = Env::from_pairs([
        ("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str()),
        // Unreachable: the mail comes from the cache.
        ("PGHACKER_ARCHIVE_BASE_URL", "http://127.0.0.1:9"),
    ]);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    Indexer::new(settings, &env).unwrap()
}

/// Stores `months` (YYYY-MM and mbox) as ingest does, from the cache.
async fn ingest(indexer: &Indexer, months: &[(&str, &str)]) {
    for (month, mbox) in months {
        let path = indexer.archive.mbox_path("pgsql-hackers", month);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, mbox).unwrap();
        let stats = pghacker_mail::ingest::ingest_mbox_month(
            &indexer.pool,
            &indexer.archive,
            "pgsql-hackers",
            month,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(stats.failed, 0);
    }
}

/// Runs the queued rethread_mail job, and records its result as the job
/// runner does.
async fn run_job(indexer: &Indexer, client: &Client) -> Value {
    let job = jobs::claim(client, "test", Some(&[jobs::kind::RETHREAD_MAIL]))
        .await
        .unwrap()
        .expect("a queued rethread_mail job");
    let result = rethread::run(indexer, &job).await.unwrap();
    client
        .execute(
            "UPDATE jobs SET state = 'done', result = $2, finished_at = now() WHERE id = $1",
            &[&job.id, &result],
        )
        .await
        .unwrap();
    result
}

async fn dry_run(indexer: &Indexer, client: &Client) -> Value {
    let queued = rethread::queue(client, None).await.unwrap();
    assert!(queued.starts_with("queued the dry-run"), "{queued}");
    run_job(indexer, client).await
}

async fn apply(indexer: &Indexer, client: &Client, plan: &str) -> Value {
    let queued = rethread::queue(client, Some(plan)).await.unwrap();
    assert!(queued.starts_with("queued the apply"), "{queued}");
    run_job(indexer, client).await
}

/// Every thread: its root and its messages in date order.
async fn threads(client: &Client) -> Vec<(String, Vec<String>)> {
    client
        .query(
            "SELECT t.root_message_id, array_agg(m.message_id ORDER BY m.sent_at, m.id)
             FROM threads t JOIN messages m ON m.thread_id = t.id
             GROUP BY t.id ORDER BY min(m.sent_at), t.id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

fn thread(root: &str, messages: &[&str]) -> (String, Vec<String>) {
    (
        root.to_owned(),
        messages.iter().map(|m| m.to_string()).collect(),
    )
}

async fn id(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

const NOVEMBER: &str = r#"From list-owner+archive@lists.example.org Sun Nov 01 10:00:00 1998
From: Tom Lane <tom@example.org>
Subject: Parser bug?
Message-ID: <bug@example.org>
Date: Sun, 1 Nov 1998 10:00:00 +0000

The grammar accepts a trailing comma in column lists.

From list-owner+archive@lists.example.org Mon Nov 02 10:00:00 1998
From: Tom Lane <tom@example.org>
Subject: pg_dump inheritance
Message-ID: <dump@example.org>
Date: Mon, 2 Nov 1998 10:00:00 +0000

pg_dump repeats inherited constraints.

From list-owner+archive@lists.example.org Tue Nov 03 10:00:00 1998
From: Oliver Elphick <olly@example.org>
To: Tom Lane <tom@example.org>
Subject: Re: Parser bug?
Message-ID: <on-bug@example.org>
In-Reply-To: Message from Tom Lane <tom@example.org> 
	of "Sun, 01 Nov 1998 10:00:00 +0000." <bug@example.org>
Date: Tue, 3 Nov 1998 10:00:00 +0000

I see it too.

From list-owner+archive@lists.example.org Wed Nov 04 10:00:00 1998
From: Oliver Elphick <olly@example.org>
To: Tom Lane <tom@example.org>
Subject: Re: pg_dump inheritance
Message-ID: <on-dump@example.org>
In-Reply-To: Message from Tom Lane <tom@example.org> 
	of "Mon, 02 Nov 1998 10:00:00 +0000." <dump@example.org>
Date: Wed, 4 Nov 1998 10:00:00 +0000

My patch to src/bin/pg_dump/pg_dump.c skips the inherited constraints.

From list-owner+archive@lists.example.org Thu Nov 05 10:00:00 1998
From: Michael Meskes <meskes@example.org>
Subject: Re: Parser bug?
Message-ID: <mutt@example.org>
In-Reply-To: <on-bug@example.org>
References: <tom@example.org> <on-bug@example.org>
Date: Thu, 5 Nov 1998 10:00:00 +0000

Here as well.
"#;

const DECEMBER: &str = r#"From list-owner+archive@lists.example.org Sat Dec 05 10:00:00 1998
From: Oliver Elphick <olly@example.org>
Subject: ISO 8601 intervals with unit designators
Message-ID: <iso@example.org>
In-Reply-To: <bug@example.org>
References: <bug@example.org>
Date: Sat, 5 Dec 1998 10:00:00 +0000

Intervals could take the ISO 8601 format with designators.

From list-owner+archive@lists.example.org Sun Dec 06 10:00:00 1998
From: Tom Lane <tom@example.org>
Subject: Re: ISO 8601 intervals with unit designators
Message-ID: <iso-re@example.org>
In-Reply-To: <iso@example.org>
References: <bug@example.org> <iso@example.org>
Date: Sun, 6 Dec 1998 10:00:00 +0000

We should accept the format with designators on input.
"#;

/// Rewrites the stored mail as ingest stored it before it knew senders'
/// addresses and topics: Oliver's parents are Tom's address, Mutt's
/// References start with it, and the replies sharing it and the topic share
/// the thread of bug@example.org.
const AS_ONCE: &str = r#"
UPDATE messages SET parent_message_id = 'tom@example.org', references_ids = '{}'
WHERE message_id IN ('on-bug@example.org', 'on-dump@example.org');
UPDATE messages SET references_ids = '{tom@example.org,on-bug@example.org}'
WHERE message_id = 'mutt@example.org';
UPDATE messages SET thread_id = (SELECT thread_id FROM messages WHERE message_id = 'bug@example.org')
WHERE message_id IN ('on-dump@example.org', 'iso@example.org', 'iso-re@example.org');
UPDATE message_chunks c SET thread_id = m.thread_id FROM messages m WHERE m.id = c.message_id;
DELETE FROM threads t WHERE NOT EXISTS (SELECT FROM messages m WHERE m.thread_id = t.id);
"#;

/// Extracts of the glued thread, from before: a feature and its file from
/// the pg_dump reply, a decision from the topic's reply, positions, and a
/// feature merge that linked the thread.
const EXTRACTS: &str = r#"
INSERT INTO features (id, slug, name, description, status) VALUES
    (900, 'dump-inherited-constraints', 'pg_dump skips inherited constraints', 'd', 'proposed'),
    (901, 'trailing-comma', 'No trailing comma in column lists', 'd', 'proposed');
INSERT INTO feature_threads (feature_id, thread_id) SELECT f, thread_id FROM messages, (VALUES (900), (901)) v (f)
WHERE message_id = 'bug@example.org';
INSERT INTO feature_files (feature_id, path, thread_id) SELECT 900, 'src/bin/pg_dump/pg_dump.c', thread_id
FROM messages WHERE message_id = 'bug@example.org';
INSERT INTO decisions (id, thread_id, title, outcome, rationale) SELECT 950, thread_id, 'Accept ISO 8601 intervals', 'o', 'r'
FROM messages WHERE message_id = 'bug@example.org';
INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary)
SELECT 970, author_id, thread_id, 900, 'proposes', 's' FROM messages WHERE message_id = 'on-dump@example.org'
UNION ALL SELECT 971, author_id, thread_id, 901, 'concerned', 's' FROM messages WHERE message_id = 'bug@example.org';
INSERT INTO citations (extract_kind, extract_id, message_id, quote)
SELECT k, e, m.id, q FROM messages m JOIN (VALUES
    ('feature', 900, 'on-dump@example.org', 'skips the inherited constraints'),
    ('feature', 901, 'bug@example.org', 'accepts a trailing comma'),
    ('decision', 950, 'iso-re@example.org', 'accept the format with designators'),
    ('position', 970, 'on-dump@example.org', 'skips the inherited constraints'),
    ('position', 971, 'bug@example.org', 'accepts a trailing comma')) v (k, e, mid, q) ON m.message_id = v.mid;
INSERT INTO merge_log (kind, from_id, into_id, moved, reason)
SELECT 'feature', 999, 900, jsonb_build_object('threads_added', jsonb_build_array(thread_id), 'citations', '[]'::jsonb,
                                              'files_added', '[]'::jsonb), 'test'
FROM messages WHERE message_id = 'bug@example.org';
-- A feature of the pg_dump thread, with a duplicate from the glued thread
-- merged into it: the merge linked it to the glued thread.
INSERT INTO features (id, slug, name, description, status) VALUES
    (902, 'dump-constraints', 'pg_dump and constraints', 'd', 'proposed'),
    (903, 'dump-constraints-again', 'pg_dump and constraints again', 'd', 'proposed');
INSERT INTO feature_threads (feature_id, thread_id)
SELECT 902, thread_id FROM messages WHERE message_id IN ('dump@example.org', 'bug@example.org')
UNION ALL SELECT 903, thread_id FROM messages WHERE message_id = 'bug@example.org';
INSERT INTO citations (id, extract_kind, extract_id, message_id, quote)
SELECT 9020, 'feature', 902, id, 'pg_dump repeats inherited constraints' FROM messages WHERE message_id = 'dump@example.org'
UNION ALL SELECT 9030, 'feature', 902, id, 'skips the inherited constraints' FROM messages WHERE message_id = 'on-dump@example.org';
UPDATE features SET merged_into = 902 WHERE id = 903;
INSERT INTO merge_log (kind, from_id, into_id, moved, reason)
SELECT 'feature', 903, 902, jsonb_build_object('citations', '[9030]'::jsonb, 'threads_added', jsonb_build_array(thread_id),
                                              'files_added', '[]'::jsonb, 'decisions', '[]'::jsonb, 'positions', '[]'::jsonb), 'test'
FROM messages WHERE message_id = 'bug@example.org';
UPDATE threads SET extraction_state = 'extracted', extracted_version = content_version;
"#;

#[tokio::test]
async fn glued_threads_come_apart_with_their_extracts() {
    let db = TestDb::create().await.unwrap();
    let indexer = indexer(&db).await;
    let client = db.indexer().await.unwrap();
    ingest(&indexer, &[("1998-11", NOVEMBER), ("1998-12", DECEMBER)]).await;
    let threaded = [
        thread(
            "bug@example.org",
            &["bug@example.org", "on-bug@example.org", "mutt@example.org"],
        ),
        thread(
            "dump@example.org",
            &["dump@example.org", "on-dump@example.org"],
        ),
        thread(
            "iso@example.org",
            &["iso@example.org", "iso-re@example.org"],
        ),
    ];
    assert_eq!(
        threads(&client).await,
        threaded,
        "ingest threads them apart"
    );
    client.batch_execute(AS_ONCE).await.unwrap();
    repair::thread_and_author_stats(&client).await.unwrap();
    client.batch_execute(EXTRACTS).await.unwrap();
    let glued = threads(&client).await;
    assert_eq!(glued.len(), 2);
    let blob = id(
        &client,
        "SELECT thread_id FROM messages WHERE message_id = 'bug@example.org'",
    )
    .await;
    let dump = id(
        &client,
        "SELECT thread_id FROM messages WHERE message_id = 'dump@example.org'",
    )
    .await;
    let version = |t: i64| {
        let client = &client;
        async move {
            id(
                client,
                &format!("SELECT content_version::int8 FROM threads WHERE id = {t}"),
            )
            .await
        }
    };
    let (blob_version, dump_version) = (version(blob).await, version(dump).await);

    // The dry run reports and changes nothing.
    let report = dry_run(&indexer, &client).await;
    assert_eq!(report["reread"]["months"], 1, "{report:#}");
    assert_eq!(report["reread"]["header_changes"], 3);
    assert_eq!(report["summary"]["groups"], 1);
    assert_eq!(report["summary"]["moves"], 3);
    assert_eq!(report["summary"]["new_threads"], 1);
    assert_eq!(report["summary"]["deleted_threads"], 0);
    let group = &report["groups"][0];
    let pieces: Vec<(Value, i64)> = group["pieces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["destination"].clone(), p["messages"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        pieces,
        [
            (json!({ "thread": blob }), 3),
            (json!({ "thread": dump }), 1),
            (json!({ "new": "iso@example.org" }), 2),
        ]
    );
    let extracts = &group["extracts"];
    assert_eq!(
        extracts["decisions"],
        json!([{ "id": 950, "from": blob, "to": { "new": "iso@example.org" } }])
    );
    assert_eq!(
        extracts["positions"],
        json!([{ "id": 970, "from": blob, "to": { "thread": dump } }])
    );
    assert_eq!(
        extracts["feature_links"],
        json!([
            { "feature": 900, "from": blob, "to": [{ "thread": dump }], "keep": false },
            { "feature": 902, "from": blob, "to": [{ "thread": dump }], "keep": false },
            // Merged into 902: it follows what the merge moved.
            { "feature": 903, "from": blob, "to": [{ "thread": dump }], "keep": false },
        ])
    );
    assert_eq!(
        extracts["feature_files"],
        json!([{ "feature": 900, "path": "src/bin/pg_dump/pg_dump.c", "from": blob, "to": { "thread": dump } }])
    );
    // A merge records the links it added: 900 wasn't linked to the pg_dump
    // thread, 902 was.
    let merges: Vec<(Value, Value)> = extracts["merges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["feature"].clone(), m["to"].clone()))
        .collect();
    assert_eq!(
        merges,
        [
            (json!(900), json!([{ "thread": dump }])),
            (json!(902), json!([]))
        ]
    );
    assert_eq!(threads(&client).await, glued);
    let plan = report["plan"].as_str().unwrap().to_owned();

    // Only the reviewed plan applies, and not while a lease holds a thread.
    let wrong = apply(&indexer, &client, "0123").await;
    assert!(
        wrong["error"].as_str().unwrap().contains(&plan),
        "{wrong:#}"
    );
    client
        .execute(
            "INSERT INTO extraction_leases (token, thread_ids, expires_at)
             VALUES ('00000000-0000-0000-0000-000000000001', ARRAY[$1::int8], now() + interval '10 minutes')",
            &[&blob],
        )
        .await
        .unwrap();
    let leased = apply(&indexer, &client, &plan).await;
    assert_eq!(leased["applied"], json!([]), "{leased:#}");
    assert!(
        leased["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("leases")
    );
    assert_eq!(threads(&client).await, glued);
    client
        .execute("UPDATE extraction_leases SET released_at = now()", &[])
        .await
        .unwrap();

    let applied = apply(&indexer, &client, &plan).await;
    assert_eq!(
        applied["applied"].as_array().unwrap().len(),
        1,
        "{applied:#}"
    );
    assert_eq!(applied["skipped"], json!([]));
    assert_eq!(threads(&client).await, threaded);
    let iso = id(
        &client,
        "SELECT thread_id FROM messages WHERE message_id = 'iso@example.org'",
    )
    .await;
    let mut extract: Vec<i64> = serde_json::from_value(applied["extract"].clone()).unwrap();
    extract.sort_unstable();
    assert_eq!(extract, [blob, dump, iso]);
    // Messages store the IDs that name messages, and their chunks follow.
    let stored: (Option<String>, Vec<String>) = client
        .query_one(
            "SELECT parent_message_id, references_ids FROM messages WHERE message_id = 'mutt@example.org'",
            &[],
        )
        .await
        .map(|row| (row.get(0), row.get(1)))
        .unwrap();
    assert_eq!(
        stored,
        (
            Some("on-bug@example.org".into()),
            vec!["on-bug@example.org".into()]
        )
    );
    assert_eq!(
        id(
            &client,
            "SELECT count(*) FROM message_chunks c JOIN messages m ON m.id = c.message_id
             WHERE c.thread_id <> m.thread_id"
        )
        .await,
        0
    );
    // Extracts follow what they cite.
    let links = |sql: &'static str| {
        let client = &client;
        async move {
            client
                .query(sql, &[])
                .await
                .unwrap()
                .iter()
                .map(|row| (row.get::<_, i64>(0), row.get::<_, i64>(1)))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        links("SELECT feature_id, thread_id FROM feature_threads ORDER BY 1, 2").await,
        [(900, dump), (901, blob), (902, dump), (903, dump)]
    );
    assert_eq!(
        links("SELECT feature_id, thread_id FROM feature_files").await,
        [(900, dump)]
    );
    assert_eq!(
        links("SELECT id, thread_id FROM decisions").await,
        [(950, iso)]
    );
    assert_eq!(
        links("SELECT id, thread_id FROM positions ORDER BY id").await,
        [(970, dump), (971, blob)]
    );
    let added = |from: i64| {
        let client = &client;
        async move {
            client
                .query_one(
                    "SELECT moved -> 'threads_added' FROM merge_log WHERE from_id = $1",
                    &[&from],
                )
                .await
                .unwrap()
                .get::<_, Value>(0)
        }
    };
    assert_eq!(added(999).await, json!([dump]));
    assert_eq!(added(903).await, json!([]));
    // Undoing the merge leaves 902 the link it had before it.
    let merge = id(&client, "SELECT id FROM merge_log WHERE from_id = 903").await;
    extraction::undo_merge(
        &indexer,
        serde_json::from_value(json!({ "merge_id": merge })).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        links("SELECT feature_id, thread_id FROM feature_threads WHERE feature_id IN (902, 903) ORDER BY 1, 2")
            .await,
        [(902, dump), (903, dump)]
    );
    assert_eq!(
        links("SELECT extract_id, message_id FROM citations WHERE id = 9030").await,
        [(
            903,
            id(
                &client,
                "SELECT id FROM messages WHERE message_id = 'on-dump@example.org'"
            )
            .await
        )]
    );
    // The threads that changed are extracted again.
    assert!(version(blob).await > blob_version);
    assert!(version(dump).await > dump_version);
    let state: (String, i64) = client
        .query_one(
            "SELECT extraction_state, message_count::int8 FROM threads WHERE id = $1",
            &[&iso],
        )
        .await
        .map(|row| (row.get(0), row.get(1)))
        .unwrap();
    assert_eq!(state, ("pending".into(), 2));
    let subject: String = client
        .query_one("SELECT subject FROM threads WHERE id = $1", &[&iso])
        .await
        .unwrap()
        .get(0);
    assert_eq!(subject, "ISO 8601 intervals with unit designators");

    // Again: nothing left to do.
    let again = dry_run(&indexer, &client).await;
    assert_eq!(again["reread"]["header_changes"], 0, "{again:#}");
    assert_eq!(again["summary"]["groups"], 0);
}

const JULY: &str = r#"From list-owner+archive@lists.example.org Tue Jul 01 10:00:00 2003
From: nolan@example.org
Subject: initcap handles digits wrongly
Message-ID: <p@example.org>
Date: Tue, 1 Jul 2003 10:00:00 +0000

initcap capitalizes after digits.

From list-owner+archive@lists.example.org Wed Jul 02 10:00:00 2003
From: Alice Example <alice@example.org>
Subject: Re: initcap handles digits wrongly
Message-ID: <q@example.org>
In-Reply-To: <no.id> from "nolan" at Jul 01, 2003 10:00:00 AM
Date: Wed, 2 Jul 2003 10:00:00 +0000

Oracle does the same.

From list-owner+archive@lists.example.org Wed Jul 02 12:00:00 2003
From: Carol Example <carol@example.org>
Subject: Re: initcap handles digits wrongly
Message-ID: <q2@example.org>
In-Reply-To: <q@example.org>
References: <q@example.org>
Date: Wed, 2 Jul 2003 12:00:00 +0000

So does DB2.

From list-owner+archive@lists.example.org Thu Jul 03 10:00:00 2003
From: Bob Example <bob@example.org>
Subject: Var sublevels up question here
Message-ID: <r@example.org>
Date: Thu, 3 Jul 2003 10:00:00 +0000

Why does sublevels_up count from zero?

From list-owner+archive@lists.example.org Fri Jul 04 10:00:00 2003
From: nolan@example.org
Subject: Re: Var sublevels up question here
Message-ID: <s@example.org>
In-Reply-To: <no.id> from "Bob" at Jul 03, 2003 10:00:00 AM
Date: Fri, 4 Jul 2003 10:00:00 +0000

Because the current level is zero.
"#;

#[tokio::test]
async fn a_thread_glued_by_a_placeholder_goes_and_its_extracts_follow() {
    let db = TestDb::create().await.unwrap();
    let indexer = indexer(&db).await;
    let client = db.indexer().await.unwrap();
    ingest(&indexer, &[("2003-07", JULY)]).await;
    let threaded = [
        thread(
            "p@example.org",
            &["p@example.org", "q@example.org", "q2@example.org"],
        ),
        thread("r@example.org", &["r@example.org", "s@example.org"]),
    ];
    assert_eq!(threads(&client).await, threaded);
    // Once, elm's placeholder was the replies' parent and their thread's root.
    client
        .batch_execute(
            "INSERT INTO threads (list, root_message_id) VALUES ('pgsql-hackers', 'no.id');
             UPDATE messages SET parent_message_id = 'no.id' WHERE message_id IN ('q@example.org', 's@example.org');
             UPDATE messages SET thread_id = (SELECT id FROM threads WHERE root_message_id = 'no.id')
             WHERE message_id IN ('q@example.org', 'q2@example.org', 's@example.org');
             UPDATE message_chunks c SET thread_id = m.thread_id FROM messages m WHERE m.id = c.message_id;
             INSERT INTO extraction_leases (token, thread_ids, expires_at, released_at)
             SELECT '00000000-0000-0000-0000-000000000002', ARRAY[id], now(), now() FROM threads WHERE root_message_id = 'no.id';
             INSERT INTO drafts (lease_token, thread_id, kind, draft)
             SELECT '00000000-0000-0000-0000-000000000002', id, 'feature', '{}' FROM threads WHERE root_message_id = 'no.id';
             INSERT INTO features (id, slug, name, description, status) VALUES (800, 'initcap-digits', 'initcap', 'd', 'proposed');
             INSERT INTO feature_threads (feature_id, thread_id) SELECT 800, id FROM threads WHERE root_message_id = 'no.id';
             INSERT INTO positions (id, author_id, thread_id, feature_id, stance, summary)
             SELECT 870, author_id, thread_id, 800, 'supports', 's' FROM messages WHERE message_id = 'q@example.org';
             INSERT INTO citations (extract_kind, extract_id, message_id, quote)
             SELECT k, 800 + (k = 'position')::int * 70, id, 'Oracle does the same' FROM messages, (VALUES ('feature'), ('position')) v (k)
             WHERE message_id = 'q@example.org';",
        )
        .await
        .unwrap();
    repair::thread_and_author_stats(&client).await.unwrap();
    let glued = id(
        &client,
        "SELECT id FROM threads WHERE root_message_id = 'no.id'",
    )
    .await;
    let p = id(
        &client,
        "SELECT id FROM threads WHERE root_message_id = 'p@example.org'",
    )
    .await;
    let r = id(
        &client,
        "SELECT id FROM threads WHERE root_message_id = 'r@example.org'",
    )
    .await;

    let report = dry_run(&indexer, &client).await;
    assert_eq!(report["summary"]["deleted_threads"], 1, "{report:#}");
    assert_eq!(report["groups"][0]["deleted"], json!([glued]));
    let plan = report["plan"].as_str().unwrap().to_owned();
    let applied = apply(&indexer, &client, &plan).await;
    assert_eq!(applied["skipped"], json!([]), "{applied:#}");

    // The replies join their threads by subject; the glued thread goes, and
    // what cited it follows its messages.
    assert_eq!(threads(&client).await, threaded);
    assert_eq!(
        id(
            &client,
            &format!("SELECT count(*) FROM threads WHERE id = {glued}")
        )
        .await,
        0
    );
    let row = |sql: &'static str| {
        let client = &client;
        async move { id(client, sql).await }
    };
    assert_eq!(
        row("SELECT thread_id FROM positions WHERE id = 870").await,
        p
    );
    assert_eq!(
        row("SELECT thread_id FROM feature_threads WHERE feature_id = 800").await,
        p
    );
    assert_eq!(row("SELECT thread_id FROM drafts").await, p.min(r));
}

const JANUARY_1998: &str = r#"From list-owner+archive@lists.example.org Thu Jan 15 10:00:00 1998
From: Jan Example <jan@example.org>
Subject: RE: [HACKERS] Re: Postgres for Sunos 4.1.4
Message-ID: <sunos@example.org>
Date: Thu, 15 Jan 1998 10:00:00 +0000

It builds on SunOS with the patch.
"#;

const FEBRUARY_1999: &str = r#"From list-owner+archive@lists.example.org Tue Feb 16 10:00:00 1999
From: Jan Example <jan@example.org>
Subject: tutorial won't compile on SunOS
Message-ID: <tutorial@example.org>
In-Reply-To: <sunos@example.org>
References: <sunos@example.org>
Date: Tue, 16 Feb 1999 10:00:00 +0000

The tutorial's Makefile fails.

From list-owner+archive@lists.example.org Wed Feb 17 10:00:00 1999
From: Bob Example <bob@example.org>
Subject: Re: tutorial won't compile on SunOS
Message-ID: <tutorial-re@example.org>
In-Reply-To: <tutorial@example.org>
References: <sunos@example.org> <tutorial@example.org>
Date: Wed, 17 Feb 1999 10:00:00 +0000

Use gmake.
"#;

#[tokio::test]
async fn a_topic_leaves_the_thread_that_a_reply_without_headers_roots() {
    let db = TestDb::create().await.unwrap();
    let indexer = indexer(&db).await;
    let client = db.indexer().await.unwrap();
    ingest(
        &indexer,
        &[("1998-01", JANUARY_1998), ("1999-02", FEBRUARY_1999)],
    )
    .await;
    let threaded = [
        thread("sunos@example.org", &["sunos@example.org"]),
        thread(
            "tutorial@example.org",
            &["tutorial@example.org", "tutorial-re@example.org"],
        ),
    ];
    assert_eq!(threads(&client).await, threaded);
    // Once, the topic shared the thread of the reply it answered, which
    // lost its own headers and roots its thread.
    client
        .batch_execute(
            "UPDATE messages SET thread_id = (SELECT id FROM threads WHERE root_message_id = 'sunos@example.org')
             WHERE message_id IN ('tutorial@example.org', 'tutorial-re@example.org');
             UPDATE message_chunks c SET thread_id = m.thread_id FROM messages m WHERE m.id = c.message_id;
             DELETE FROM threads t WHERE NOT EXISTS (SELECT FROM messages m WHERE m.thread_id = t.id);",
        )
        .await
        .unwrap();
    repair::thread_and_author_stats(&client).await.unwrap();
    let sunos = id(
        &client,
        "SELECT id FROM threads WHERE root_message_id = 'sunos@example.org'",
    )
    .await;

    let report = dry_run(&indexer, &client).await;
    assert_eq!(report["reread"]["months"], 0, "{report:#}");
    assert_eq!(report["summary"]["misplaced_topics"], 1);
    assert_eq!(report["summary"]["reroots"], 0);
    assert_eq!(report["groups"][0]["deleted"], json!([]));
    let plan = report["plan"].as_str().unwrap().to_owned();
    let applied = apply(&indexer, &client, &plan).await;
    assert_eq!(applied["skipped"], json!([]), "{applied:#}");
    assert_eq!(threads(&client).await, threaded);
    assert_eq!(
        id(
            &client,
            "SELECT thread_id FROM messages WHERE message_id = 'sunos@example.org'"
        )
        .await,
        sunos,
        "the reply keeps its thread"
    );
}
