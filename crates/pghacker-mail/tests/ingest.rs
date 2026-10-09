//! The ingestion pipelines on a fresh database, with the archive's responses
//! already in the cache: no network.

use std::path::Path;

use pghacker_core::testing::TestDb;
use pghacker_mail::ingest::{self, IngestStats};
use pghacker_mail::{ArchiveClient, ArchiveConfig};
use tempfile::TempDir;

fn scratch() -> TempDir {
    TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// A client whose archive is unreachable: everything must come from the
/// cache.
fn offline_client(cache: &Path) -> ArchiveClient {
    ArchiveClient::new(ArchiveConfig {
        base_url: "http://127.0.0.1:9".into(),
        user: "archives".into(),
        password: "antispam".into(),
        requests_per_second: 100.0,
        cache_dir: cache.to_path_buf(),
        user_agent: "pghacker-mail tests".into(),
    })
    .unwrap()
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

const MAY: &str = r#"From list-owner+archive@lists.example.org Fri May 01 10:00:00 2026
From: Alice Example <alice@example.org>
Subject: Planner idea
Message-ID: <m1@example.org>
Date: Fri, 1 May 2026 10:00:00 +0000

From the planner's point of view, this line starts with From.

From list-owner+archive@lists.example.org Sat May 02 10:00:00 2026
From: Bob Example <bob@example.org>
Subject: Re: Planner idea
Message-ID: <m2@example.org>
In-Reply-To: <m1@example.org>
References: <m1@example.org>
Date: Sat, 2 May 2026 10:00:00 +0000
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="b"

--b
Content-Type: text/plain

Patch attached.

--b
Content-Type: text/x-patch; name="v1-0001-Idea.patch"
Content-Disposition: attachment; filename="v1-0001-Idea.patch"

From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001
From: Bob Example <bob@example.org>
Subject: [PATCH v1] Idea

diff --git a/src/backend/optimizer/plan/planner.c b/src/backend/optimizer/plan/planner.c
--- a/src/backend/optimizer/plan/planner.c
+++ b/src/backend/optimizer/plan/planner.c
@@ -1 +1 @@
-old
+new
--b--

From list-owner+archive@lists.example.org Sun May 03 10:00:00 2026
From: Carol Example <carol@example.org>
Subject: Unrelated
Message-ID: <m3@example.org>
Date: Sun, 3 May 2026 10:00:00 +0000

Hello.

From list-owner+archive@lists.example.org Sun May 03 11:00:00 2026
From: Nobody <nobody@example.org>
Subject: No Message-ID here
Date: Sun, 3 May 2026 11:00:00 +0000

This one fails.
"#;

fn summary(stats: &IngestStats) -> [usize; 6] {
    [
        stats.fetched,
        stats.parsed,
        stats.inserted,
        stats.existing,
        stats.failed,
        stats.threads_touched,
    ]
}

#[tokio::test]
async fn ingests_a_month_once() {
    let db = TestDb::create().await.unwrap();
    let pool = db.indexer_pool(2).unwrap();
    let cache = scratch();
    let client = offline_client(cache.path());
    write(&client.mbox_path("pgsql-hackers", "2026-05"), MAY);

    let first = ingest::ingest_mbox_month(&pool, &client, "pgsql-hackers", "2026-05", None, false)
        .await
        .unwrap();
    assert_eq!(summary(&first), [4, 3, 3, 0, 1, 2]);
    let again = ingest::ingest_mbox_month(&pool, &client, "pgsql-hackers", "2026-05", None, true)
        .await
        .unwrap();
    assert_eq!(summary(&again), [4, 0, 0, 3, 1, 0]);

    let indexer = db.indexer().await.unwrap();
    let rows = indexer
        .query(
            "SELECT m.message_id, t.root_message_id, m.archive_month, m.has_patch, m.body
             FROM messages m JOIN threads t ON t.id = m.thread_id ORDER BY m.sent_at",
            &[],
        )
        .await
        .unwrap();
    let summary: Vec<(String, String, Option<String>, bool)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    let may = Some("2026-05".to_owned());
    assert_eq!(
        summary,
        [
            (
                "m1@example.org".into(),
                "m1@example.org".into(),
                may.clone(),
                false
            ),
            (
                "m2@example.org".into(),
                "m1@example.org".into(),
                may.clone(),
                true
            ),
            ("m3@example.org".into(), "m3@example.org".into(), may, false),
        ]
    );
    let body: String = rows[0].get(4);
    assert!(body.contains("From the planner's point of view"));
    let paths: Vec<String> = indexer
        .query_one("SELECT paths FROM patches", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(paths, ["src/backend/optimizer/plan/planner.c"]);
}

const SEED: &str = "CA+t2=x/y@mail.example.org";

fn flat_page(ids: &[&str]) -> String {
    let mut html = String::from("<html><body><h1>Re: Thread</h1>\n");
    for id in ids {
        let encoded = pghacker_mail::encode_path_segment(id);
        html.push_str(&format!(
            "<table class=\"message-header\">\n <tr>\n  <th scope=\"row\">Message-ID:</th>\n  \
             <td><a href=\"/message-id/{encoded}\">{id}</a></td>\n </tr>\n</table>\n"
        ));
    }
    html + "</body></html>\n"
}

fn raw(id: &str, date: &str, subject: &str, refs: &[&str]) -> String {
    let mut text = format!(
        "From list-owner+archive@lists.example.org Mon Jun 01 00:00:00 2026\n\
         From: Dana Example <dana@example.org>\nSubject: {subject}\nMessage-ID: <{id}>\n\
         Date: {date} 10:00:00 +0000\n"
    );
    if let Some(parent) = refs.last() {
        let list: Vec<String> = refs.iter().map(|r| format!("<{r}>")).collect();
        text.push_str(&format!(
            "In-Reply-To: <{parent}>\nReferences: {}\n",
            list.join(" ")
        ));
    }
    text + "\nText.\n"
}

#[tokio::test]
async fn ingests_a_thread_and_threads_later_mail_with_it() {
    let db = TestDb::create().await.unwrap();
    let pool = db.indexer_pool(2).unwrap();
    let cache = scratch();
    let client = offline_client(cache.path());
    let ids = ["t1@example.org", SEED, "t3@example.org", "t4@example.org"];
    write(&client.flat_page_path(SEED), &flat_page(&ids));
    write(
        &client.raw_message_path(ids[0]),
        &raw(ids[0], "1 Feb 2024", "Topic", &[]),
    );
    write(
        &client.raw_message_path(ids[1]),
        &raw(ids[1], "2 Feb 2024", "Re: Topic", &[ids[0]]),
    );
    write(
        &client.raw_message_path(ids[2]),
        &raw(ids[2], "3 Feb 2024", "Re: Topic", &[ids[0], ids[1]]),
    );
    write(
        &client.raw_message_path(ids[3]),
        "From: x@example.org\n\nNo Message-ID.\n",
    );

    let first = ingest::ingest_thread(&pool, &client, "pgsql-hackers", &format!("<{SEED}>"), None)
        .await
        .unwrap();
    assert_eq!(summary(&first), [4, 3, 3, 0, 1, 1]);
    let again = ingest::ingest_thread(&pool, &client, "pgsql-hackers", SEED, None)
        .await
        .unwrap();
    assert_eq!(summary(&again), [1, 0, 0, 3, 1, 0]);

    // A month's reply to the thread joins it.
    let june = format!(
        "{}\n",
        raw(
            "t5@example.org",
            "1 Jun 2026",
            "Re: Topic",
            &[ids[0], ids[1], ids[2]]
        )
    );
    write(&client.mbox_path("pgsql-hackers", "2026-06"), &june);
    let month = ingest::ingest_mbox_month(&pool, &client, "pgsql-hackers", "2026-06", None, false)
        .await
        .unwrap();
    assert_eq!(summary(&month), [1, 1, 1, 0, 0, 1]);
    let indexer = db.indexer().await.unwrap();
    let row = indexer
        .query_one(
            "SELECT count(*), min(root_message_id), sum(message_count)::bigint FROM threads",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, String>(1), "t1@example.org");
    assert_eq!(row.get::<_, i64>(2), 4);
    let months: Vec<Option<String>> = indexer
        .query("SELECT archive_month FROM messages ORDER BY sent_at", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(months, [None, None, None, Some("2026-06".into())]);
}

/// A month of replies to six threads whose roots are in an earlier month,
/// from five authors.
fn month_of_replies(month: u32) -> String {
    let mut mbox = String::new();
    for i in 0..60 {
        let thread = i % 6;
        let author = i % 5;
        let day = 1 + i / 3;
        let id = format!("m{month}-{i}@example.org");
        let root = format!("root-{thread}@example.org");
        mbox.push_str(&format!(
            "From list-owner+archive@lists.example.org Mon Mar 02 00:00:00 2026\n\
             From: Author {author} <author{author}@example.org>\n\
             Subject: Re: Topic {thread}\nMessage-ID: <{id}>\n\
             Date: {day} {} 2026 10:{:02}:00 +0000\n\
             In-Reply-To: <{root}>\nReferences: <{root}>\n\nReply {i}.\n\n",
            if month == 3 { "Mar" } else { "Apr" },
            i % 60
        ));
    }
    mbox
}

#[tokio::test]
async fn concurrent_ingestions_agree() {
    let db = TestDb::create().await.unwrap();
    let pool = db.indexer_pool(6).unwrap();
    let cache = scratch();
    let client = offline_client(cache.path());
    write(
        &client.mbox_path("pgsql-hackers", "2026-03"),
        &month_of_replies(3),
    );
    write(
        &client.mbox_path("pgsql-hackers", "2026-04"),
        &month_of_replies(4),
    );
    let (march, april, march_again) = tokio::join!(
        ingest::ingest_mbox_month(&pool, &client, "pgsql-hackers", "2026-03", None, false),
        ingest::ingest_mbox_month(&pool, &client, "pgsql-hackers", "2026-04", None, false),
        ingest::ingest_mbox_month(&pool, &client, "pgsql-hackers", "2026-03", None, false),
    );
    let (march, april, march_again) = (march.unwrap(), april.unwrap(), march_again.unwrap());
    assert_eq!(march.failed + april.failed + march_again.failed, 0);
    assert_eq!(march.inserted + march_again.inserted, 60);
    assert_eq!(april.inserted, 60);
    let indexer = db.indexer().await.unwrap();
    let rows = indexer
        .query(
            "SELECT message_count FROM threads ORDER BY root_message_id",
            &[],
        )
        .await
        .unwrap();
    let counts: Vec<i32> = rows.into_iter().map(|r| r.get(0)).collect();
    assert_eq!(counts, [20; 6]);
    let authors: i64 = indexer
        .query_one("SELECT count(*) FROM authors", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(authors, 5);
    let counted: i64 = indexer
        .query_one("SELECT sum(message_count)::bigint FROM authors", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(counted, 120);
}
