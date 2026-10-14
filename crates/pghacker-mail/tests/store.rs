//! Threading, authors, addresses and thread statistics on a fresh database.

use std::sync::LazyLock;

use pghacker_core::testing::TestDb;
use pghacker_mail::parse::{self, ParsedMessage};
use pghacker_mail::store::{self, Restored, StoreContext, Stored};
use regex::Regex;
use tokio_postgres::Client;

mod common;
use common::{store_as_once, stored_text};

const MULTIPART: &[u8] = include_bytes!("fixtures/multipart_patch.eml");

fn ctx() -> StoreContext {
    StoreContext {
        list: "pgsql-hackers".into(),
        source_id: None,
        archive_month: Some("2026-06".into()),
    }
}

/// A message sent on `date` (such as "2 Jun 2026"); `refs` become
/// References, and the last one In-Reply-To.
fn message(id: &str, from: &str, date: &str, subject: &str, refs: &[&str]) -> ParsedMessage {
    let mut headers = format!("Date: {date} 10:00:00 +0000\n");
    if let Some(parent) = refs.last() {
        headers.push_str(&format!("In-Reply-To: <{parent}>\n"));
        let list: Vec<String> = refs.iter().map(|r| format!("<{r}>")).collect();
        headers.push_str(&format!("References: {}\n", list.join(" ")));
    }
    raw_message(id, from, subject, &headers, &format!("Text of {id}."))
}

fn raw_message(id: &str, from: &str, subject: &str, headers: &str, body: &str) -> ParsedMessage {
    let raw = format!("From: {from}\nSubject: {subject}\nMessage-ID: <{id}>\n{headers}\n{body}\n");
    parse::parse(raw.as_bytes()).unwrap()
}

const ALICE: &str = "Alice Example <alice@example.org>";
const BOB: &str = "Bob Example <bob@example.org>";

/// Stores each message in a transaction of its own, as separate batches do.
async fn store_each(client: &mut Client, messages: &[&ParsedMessage]) -> Vec<Stored> {
    let mut stored = Vec::new();
    for msg in messages {
        let tx = client.transaction().await.unwrap();
        let result = store::upsert_message(&tx, &ctx(), msg).await.unwrap();
        store::refresh_threads(&tx, &[result.thread_id])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        stored.push(result);
    }
    stored
}

#[derive(Debug, PartialEq)]
struct Thread {
    root: String,
    messages: Vec<String>,
}

/// Every thread: its root and its messages in date order.
async fn threads(client: &Client) -> Vec<Thread> {
    let rows = client
        .query(
            "SELECT t.root_message_id, array_agg(m.message_id ORDER BY m.sent_at, m.id)
             FROM threads t JOIN messages m ON m.thread_id = t.id
             GROUP BY t.id ORDER BY min(m.sent_at), t.id",
            &[],
        )
        .await
        .unwrap();
    rows.into_iter()
        .map(|row| Thread {
            root: row.get(0),
            messages: row.get(1),
        })
        .collect()
}

fn thread(root: &str, messages: &[&str]) -> Thread {
    Thread {
        root: root.into(),
        messages: messages.iter().map(|m| m.to_string()).collect(),
    }
}

async fn count(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

#[tokio::test]
async fn threads_messages_that_arrive_out_of_order() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let a = message("a@example.org", ALICE, "1 Jun 2026", "Fix the planner", &[]);
    let b = message(
        "b@example.org",
        BOB,
        "2 Jun 2026",
        "Re: Fix the planner",
        &["a@example.org"],
    );
    let c = message(
        "c@example.org",
        ALICE,
        "3 Jun 2026",
        "Re: Fix the planner",
        &["a@example.org", "b@example.org"],
    );
    let d = message(
        "d@example.org",
        BOB,
        "4 Jun 2026",
        "Re: Fix the planner",
        &["a@example.org", "b@example.org", "c@example.org"],
    );
    store_each(&mut client, &[&d, &b, &a, &c]).await;
    assert_eq!(
        threads(&client).await,
        [thread(
            "a@example.org",
            &[
                "a@example.org",
                "b@example.org",
                "c@example.org",
                "d@example.org"
            ]
        )]
    );
    let row = client
        .query_one(
            "SELECT message_count, participant_count, subject, started_at::date::text, last_at::date::text
             FROM threads",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 4);
    assert_eq!(row.get::<_, i32>(1), 2);
    assert_eq!(row.get::<_, String>(2), "Fix the planner");
    assert_eq!(row.get::<_, String>(3), "2026-06-01");
    assert_eq!(row.get::<_, String>(4), "2026-06-04");
    let parent: Option<String> = client
        .query_one(
            "SELECT parent_message_id FROM messages WHERE message_id = 'c@example.org'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(parent.as_deref(), Some("b@example.org"));
}

#[tokio::test]
async fn threads_keep_a_root_that_was_never_ingested() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let b = message(
        "b@example.org",
        BOB,
        "2 Jun 2026",
        "Re: Old topic",
        &["x@example.org"],
    );
    let c = message(
        "c@example.org",
        ALICE,
        "3 Jun 2026",
        "Re: Old topic",
        &["x@example.org", "b@example.org"],
    );
    // A later month, ingested first, replying to an earlier message.
    let e = message(
        "e@example.org",
        ALICE,
        "2 Jul 2026",
        "Re: Old topic",
        &["x@example.org", "b@example.org"],
    );
    store_each(&mut client, &[&e, &c, &b]).await;
    assert_eq!(
        threads(&client).await,
        [thread(
            "x@example.org",
            &["b@example.org", "c@example.org", "e@example.org"]
        )]
    );
    let subject: String = client
        .query_one("SELECT subject FROM threads", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(subject, "Old topic");
}

#[tokio::test]
async fn threads_survive_clients_that_drop_references() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let a = message("a@example.org", ALICE, "1 Jun 2026", "Topic", &[]);
    let b = message(
        "b@example.org",
        BOB,
        "2 Jun 2026",
        "Re: Topic",
        &["a@example.org"],
    );
    let c = message(
        "c@example.org",
        ALICE,
        "3 Jun 2026",
        "Re: Topic",
        &["a@example.org", "b@example.org"],
    );
    // Only In-Reply-To.
    let d = raw_message(
        "d@example.org",
        BOB,
        "Re: Topic",
        "Date: 4 Jun 2026 10:00:00 +0000\nIn-Reply-To: <c@example.org>\n",
        "Text.",
    );
    store_each(&mut client, &[&d]).await;
    assert_eq!(
        threads(&client).await,
        [thread("c@example.org", &["d@example.org"])]
    );
    // c joins the thread its reply started, which takes c's root.
    store_each(&mut client, &[&c, &a, &b]).await;
    assert_eq!(
        threads(&client).await,
        [thread(
            "a@example.org",
            &[
                "a@example.org",
                "b@example.org",
                "c@example.org",
                "d@example.org"
            ]
        )]
    );
}

#[tokio::test]
async fn replies_that_arrived_first_merge_into_the_thread() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let a = message("a@example.org", ALICE, "1 Jun 2026", "Topic", &[]);
    let e = raw_message(
        "e@example.org",
        BOB,
        "Re: Topic",
        "Date: 5 Jun 2026 10:00:00 +0000\nIn-Reply-To: <y@example.org>\n",
        "Text.",
    );
    let y = message(
        "y@example.org",
        ALICE,
        "4 Jun 2026",
        "Re: Topic",
        &["a@example.org"],
    );
    let stored = store_each(&mut client, &[&a, &e]).await;
    let (thread_a, thread_y) = (stored[0].thread_id, stored[1].thread_id);
    assert_ne!(thread_a, thread_y);
    let version_before: i32 = client
        .query_one(
            "SELECT content_version FROM threads WHERE id = $1",
            &[&thread_a],
        )
        .await
        .unwrap()
        .get(0);
    let stored = store_each(&mut client, &[&y]).await;
    assert_eq!(stored[0].thread_id, thread_a);
    assert_eq!(
        threads(&client).await,
        [thread(
            "a@example.org",
            &["a@example.org", "y@example.org", "e@example.org"]
        )]
    );
    assert_eq!(
        count(
            &client,
            &format!("SELECT count(*) FROM message_chunks WHERE thread_id <> {thread_a}")
        )
        .await,
        0
    );
    let row = client
        .query_one(
            "SELECT content_version, message_count FROM threads WHERE id = $1",
            &[&thread_a],
        )
        .await
        .unwrap();
    assert!(row.get::<_, i32>(0) > version_before);
    assert_eq!(row.get::<_, i32>(1), 3);
}

#[tokio::test]
async fn threads_that_extracts_cite_are_not_merged() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let a = message("a@example.org", ALICE, "1 Jun 2026", "Topic", &[]);
    let e = raw_message(
        "e@example.org",
        BOB,
        "Re: Topic",
        "Date: 5 Jun 2026 10:00:00 +0000\nIn-Reply-To: <y@example.org>\n",
        "Text.",
    );
    let stored = store_each(&mut client, &[&a, &e]).await;
    client
        .execute(
            "INSERT INTO decisions (thread_id, title, outcome, rationale) VALUES ($1, 't', 'o', 'r')",
            &[&stored[1].thread_id],
        )
        .await
        .unwrap();
    let y = message(
        "y@example.org",
        ALICE,
        "4 Jun 2026",
        "Re: Topic",
        &["a@example.org"],
    );
    let result = store_each(&mut client, &[&y]).await;
    assert!(result[0].inserted);
    assert_eq!(
        threads(&client).await,
        [
            thread("a@example.org", &["a@example.org", "y@example.org"]),
            thread("y@example.org", &["e@example.org"])
        ]
    );
}

#[tokio::test]
async fn replies_without_headers_join_by_subject() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let a = message(
        "a@example.org",
        ALICE,
        "1 Jun 2026",
        "Fix the frobnicator",
        &[],
    );
    let r1 = message(
        "r1@example.org",
        BOB,
        "10 Jun 2026",
        "Re: Fix the frobnicator",
        &[],
    );
    let r2 = message(
        "r2@example.org",
        ALICE,
        "12 Jun 2026",
        "RE: [PATCH v2] fix the frobnicator.",
        &[],
    );
    let late = message(
        "late@example.org",
        BOB,
        "1 Oct 2026",
        "Re: Fix the frobnicator",
        &[],
    );
    let other = message(
        "other@example.org",
        BOB,
        "11 Jun 2026",
        "Re: Something else",
        &[],
    );
    let fresh = message(
        "fresh@example.org",
        BOB,
        "11 Jun 2026",
        "Fix the frobnicator",
        &[],
    );
    store_each(&mut client, &[&a, &r1, &r2, &late, &other, &fresh]).await;
    assert_eq!(
        threads(&client).await,
        [
            thread(
                "a@example.org",
                &["a@example.org", "r1@example.org", "r2@example.org"]
            ),
            thread("other@example.org", &["other@example.org"]),
            thread("fresh@example.org", &["fresh@example.org"]),
            thread("late@example.org", &["late@example.org"]),
        ]
    );
}

/// Row counts and sums that any change to the stored mail would move.
async fn snapshot(client: &Client) -> Vec<i64> {
    let row = client
        .query_one(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM patches),
                    (SELECT count(*) FROM anchors), (SELECT count(*) FROM message_chunks),
                    (SELECT count(*) FROM threads),
                    (SELECT coalesce(sum(content_version), 0)::bigint FROM threads),
                    (SELECT coalesce(sum(message_count), 0)::bigint FROM authors),
                    (SELECT coalesce(sum(count), 0)::bigint FROM author_names),
                    (SELECT count(*) FROM authors)",
            &[],
        )
        .await
        .unwrap();
    (0..row.len()).map(|i| row.get::<_, i64>(i)).collect()
}

#[tokio::test]
async fn replies_without_headers_that_arrive_first_are_adopted() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    // The thread's root is in a month not ingested; a reply without headers
    // and an answer to it come first.
    let orphan = message(
        "orphan@example.org",
        BOB,
        "4 Jun 2026",
        "Re: Release notes draft",
        &[],
    );
    let answer = message(
        "answer@example.org",
        ALICE,
        "4 Jun 2026",
        "Re: Release notes draft",
        &["orphan@example.org"],
    );
    let reply = message(
        "reply@example.org",
        ALICE,
        "5 Jun 2026",
        "Re: Release notes draft",
        &["root@example.org"],
    );
    // A new topic posted after a headerless "reply" with its subject: they
    // stay apart. Posted before one, arriving later: adopted.
    let early = message(
        "early@example.org",
        BOB,
        "4 Jun 2026",
        "Re: Other topic about costs",
        &[],
    );
    let other = message(
        "other@example.org",
        ALICE,
        "6 Jun 2026",
        "Other topic about costs",
        &[],
    );
    let late = message(
        "late@example.org",
        BOB,
        "8 Jun 2026",
        "Re: Third topic about costs",
        &[],
    );
    let third = message(
        "third@example.org",
        ALICE,
        "6 Jun 2026",
        "Third topic about costs",
        &[],
    );
    store_each(
        &mut client,
        &[&orphan, &answer, &reply, &early, &other, &late, &third],
    )
    .await;
    assert_eq!(
        threads(&client).await,
        [
            thread(
                "root@example.org",
                &[
                    "orphan@example.org",
                    "answer@example.org",
                    "reply@example.org"
                ]
            ),
            thread("early@example.org", &["early@example.org"]),
            thread("other@example.org", &["other@example.org"]),
            thread(
                "third@example.org",
                &["third@example.org", "late@example.org"]
            ),
        ]
    );
    let subject: String = client
        .query_one(
            "SELECT subject FROM threads WHERE root_message_id = 'root@example.org'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(subject, "Release notes draft");
}

#[tokio::test]
async fn subject_matching_does_not_depend_on_arrival_order() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    // The same messages as above, the other way around.
    let early = message(
        "early@example.org",
        BOB,
        "4 Jun 2026",
        "Re: Other topic about costs",
        &[],
    );
    let other = message(
        "other@example.org",
        ALICE,
        "6 Jun 2026",
        "Other topic about costs",
        &[],
    );
    let late = message(
        "late@example.org",
        BOB,
        "8 Jun 2026",
        "Re: Third topic about costs",
        &[],
    );
    let third = message(
        "third@example.org",
        ALICE,
        "6 Jun 2026",
        "Third topic about costs",
        &[],
    );
    store_each(&mut client, &[&other, &early, &third, &late]).await;
    assert_eq!(
        threads(&client).await,
        [
            thread("early@example.org", &["early@example.org"]),
            thread("other@example.org", &["other@example.org"]),
            thread(
                "third@example.org",
                &["third@example.org", "late@example.org"]
            ),
        ]
    );
}

#[tokio::test]
async fn messages_that_share_an_unstored_ancestor_share_a_thread() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    // A later month's reply lists the whole chain; an earlier month's
    // replies name only their parent, which neither month has.
    let later = message(
        "later@example.org",
        ALICE,
        "8 Jul 2026",
        "Re: Topic",
        &[
            "root@example.org",
            "middle@example.org",
            "parent@example.org",
        ],
    );
    let only_parent = raw_message(
        "only-parent@example.org",
        BOB,
        "Re: Topic",
        "Date: 12 Jun 2026 10:00:00 +0000\nIn-Reply-To: <parent@example.org>\n",
        "Text.",
    );
    store_each(&mut client, &[&later, &only_parent]).await;
    assert_eq!(
        threads(&client).await,
        [thread(
            "root@example.org",
            &["only-parent@example.org", "later@example.org"]
        )]
    );
}

#[tokio::test]
async fn reingesting_a_message_changes_nothing() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let msg = parse::parse(MULTIPART).unwrap();
    let first = store_each(&mut client, &[&msg]).await;
    assert!(first[0].inserted);
    let before = snapshot(&client).await;
    let second = store_each(&mut client, &[&msg]).await;
    assert!(!second[0].inserted);
    assert_eq!(second[0].message_row_id, first[0].message_row_id);
    assert_eq!(second[0].thread_id, first[0].thread_id);
    assert_eq!(snapshot(&client).await, before);
    // What the message produced.
    assert_eq!(count(&client, "SELECT count(*) FROM patches").await, 2);
    let has_patch: bool = client
        .query_one("SELECT has_patch FROM messages", &[])
        .await
        .unwrap()
        .get(0);
    assert!(has_patch);
    let anchors: Vec<(String, String, String)> = client
        .query("SELECT kind, value, origin FROM anchors ORDER BY id", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    for expected in [
        ("file", "src/backend/utils/adt/frob.c", "subject"),
        ("commit", "1a2b3c4d", "body"),
        ("commitfest", "6123", "body"),
        ("release", "17", "body"),
        ("file", "src/include/utils/frob.h", "patch"),
        ("file", "src/test/regress/sql/frob.sql", "patch"),
    ] {
        assert!(
            anchors
                .iter()
                .any(|(k, v, o)| (k.as_str(), v.as_str(), o.as_str()) == expected),
            "{expected:?} in {anchors:?}"
        );
    }
    let resolved: i64 = count(
        &client,
        "SELECT count(*) FROM anchors WHERE resolved IS NOT NULL",
    )
    .await;
    assert_eq!(resolved, 0);
    let unembedded: i64 = count(
        &client,
        "SELECT count(*) FROM message_chunks WHERE embedding IS NULL",
    )
    .await;
    assert_eq!(
        unembedded,
        count(&client, "SELECT count(*) FROM message_chunks").await
    );
}

#[tokio::test]
async fn authors_match_by_address_and_keep_their_most_frequent_name() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let m1 = message(
        "m1@example.org",
        "Alice Example <Alice@Example.org>",
        "1 Jun 2026",
        "A",
        &[],
    );
    let m2 = message(
        "m2@example.org",
        "alice <alice@example.org>",
        "2 Jun 2026",
        "B",
        &[],
    );
    let m3 = message(
        "m3@example.org",
        "\"Alice Example\" <alice@example.org>",
        "3 Jun 2026",
        "C",
        &[],
    );
    let m4 = message(
        "m4@example.org",
        "Alice Example <alice@other.example.org>",
        "4 Jun 2026",
        "D",
        &[],
    );
    store_each(&mut client, &[&m2, &m1, &m3, &m4]).await;
    let rows = client
        .query(
            "SELECT a.id, a.display_name, a.message_count, a.first_seen::date::text, a.last_seen::date::text,
                    array_agg(e.email ORDER BY e.email)
             FROM authors a JOIN author_emails e ON e.author_id = a.id
             GROUP BY a.id ORDER BY a.id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let alice: i64 = rows[0].get(0);
    assert_eq!(rows[0].get::<_, String>(1), "Alice Example");
    assert_eq!(rows[0].get::<_, i32>(2), 3);
    assert_eq!(rows[0].get::<_, String>(3), "2026-06-01");
    assert_eq!(rows[0].get::<_, String>(4), "2026-06-03");
    assert_eq!(rows[0].get::<_, Vec<String>>(5), ["alice@example.org"]);
    assert_eq!(
        rows[1].get::<_, Vec<String>>(5),
        ["alice@other.example.org"]
    );
    let names: Vec<(String, i32)> = client
        .query(
            "SELECT name, count FROM author_names WHERE author_id = $1 ORDER BY count DESC",
            &[&alice],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        names,
        [("Alice Example".to_owned(), 2), ("alice".to_owned(), 1)]
    );
    assert_eq!(
        count(
            &client,
            "SELECT (SELECT count(*) FROM authors WHERE display_name LIKE '%@%')
                  + (SELECT count(*) FROM author_names WHERE name LIKE '%@%')
                  + (SELECT count(*) FROM messages WHERE from_name LIKE '%@%')"
        )
        .await,
        0
    );
    // After a merge, the merged author's address counts for the one it became.
    let other: i64 = rows[1].get(0);
    client
        .execute(
            "UPDATE authors SET merged_into = $1 WHERE id = $2",
            &[&alice, &other],
        )
        .await
        .unwrap();
    let m5 = message(
        "m5@example.org",
        "Alice Example <alice@other.example.org>",
        "5 Jun 2026",
        "E",
        &[],
    );
    store_each(&mut client, &[&m5]).await;
    let author: i64 = client
        .query_one(
            "SELECT author_id FROM messages WHERE message_id = 'm5@example.org'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(author, alice);
}

static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\S*[A-Za-z0-9._%+=-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+\S*").unwrap()
});

/// The address-like tokens of `text` that aren't Message-ID URLs.
fn addresses(text: &str) -> Vec<String> {
    ADDRESS
        .find_iter(text)
        .map(|m| m.as_str().to_owned())
        .filter(|token| !token.contains("/message-id/") && !token.contains("postgr.es/m/"))
        .collect()
}

/// A message with addresses in its subject, sender's name, body and patch.
fn addressed_message() -> ParsedMessage {
    let body = "Ask carol@example.org, or see\nhttps://www.postgresql.org/message-id/CAB+x=y@mail.example.org\nand https://postgr.es/m/abc@example.org for details.\n\nOn Mon, Dave <dave@example.org> wrote:\n> quoted\n";
    let mut msg = raw_message(
        "redact@example.org",
        "\"erin@example.org\" <erin@example.org>",
        "Ping frank@example.org",
        "Date: 1 Jun 2026 10:00:00 +0000\n",
        body,
    );
    msg.attachments = parse::parse(MULTIPART).unwrap().attachments;
    msg
}

#[tokio::test]
async fn stored_text_keeps_addresses() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    store_each(&mut client, &[&addressed_message()]).await;
    let row = client
        .query_one(
            "SELECT body, body_clean, subject, from_name FROM messages",
            &[],
        )
        .await
        .unwrap();
    let (body, clean, subject, from_name): (String, String, String, String) =
        (row.get(0), row.get(1), row.get(2), row.get(3));
    assert!(body.contains("Ask carol@example.org, or see"), "{body}");
    assert!(body.contains("On Mon, Dave <dave@example.org> wrote:"));
    assert!(body.contains("https://www.postgresql.org/message-id/CAB+x=y@mail.example.org"));
    assert!(clean.contains("carol@example.org"), "{clean}");
    assert_eq!(subject, "Ping frank@example.org");
    assert_eq!(from_name, "erin@example.org");
    let chunks: Vec<String> = client
        .query("SELECT content FROM message_chunks", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(chunks.iter().any(|c| c.contains("frank@example.org")));
    let patches: Vec<String> = client
        .query("SELECT content FROM patches", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        patches
            .iter()
            .any(|p| addresses(p).contains(&"<alice@example.org>".to_owned())),
        "{patches:?}"
    );
    let emails: Vec<String> = client
        .query("SELECT email FROM author_emails", &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(emails, ["erin@example.org"]);
}

/// Restores a message in a transaction of its own, as a restore's batch
/// does.
async fn restore(client: &mut Client, msg: &ParsedMessage) -> Restored {
    let tx = client.transaction().await.unwrap();
    let restored = store::restore_addresses(&tx, msg).await.unwrap();
    if let Restored::Restored { thread_id } = restored {
        store::refresh_threads(&tx, &[thread_id]).await.unwrap();
    }
    tx.commit().await.unwrap();
    restored
}

#[tokio::test]
async fn restoring_gives_stored_text_its_addresses_back() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let msg = addressed_message();
    store_each(&mut client, &[&msg]).await;
    let written = stored_text(&client, "redact@example.org").await;
    store_as_once(&client, "redact@example.org", msg.from_email.as_deref()).await;
    let once = stored_text(&client, "redact@example.org").await;
    assert!(once.contains(&"Ping [email]".to_owned()), "{once:#?}");
    assert!(once.contains(&"erin".to_owned()), "{once:#?}");
    client
        .batch_execute("DELETE FROM graph_changes")
        .await
        .unwrap();

    let restored = restore(&mut client, &msg).await;
    assert!(
        matches!(restored, Restored::Restored { .. }),
        "{restored:?}"
    );
    assert_eq!(stored_text(&client, "redact@example.org").await, written);
    // Its chunks are embedded again and its patches indexed again; the
    // author's names, which decide who committed commits, go to the graph.
    let (embedded, indexed): (i64, i64) = client
        .query_one(
            "SELECT (SELECT count(*) FROM message_chunks WHERE embedding IS NOT NULL),
                    (SELECT count(*) FROM patches WHERE text_ids IS NOT NULL)",
            &[],
        )
        .await
        .map(|r| (r.get(0), r.get(1)))
        .unwrap();
    assert_eq!((embedded, indexed), (0, 0));
    let author_changed: bool = client
        .query_one(
            "SELECT EXISTS (SELECT FROM graph_changes c JOIN messages m ON c.key = 'author:' || m.author_id)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(author_changed);
    // Again, nothing to do.
    assert_eq!(restore(&mut client, &msg).await, Restored::Unchanged);
    assert_eq!(stored_text(&client, "redact@example.org").await, written);
}

#[tokio::test]
async fn restoring_leaves_text_that_differs() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let in_subject = raw_message(
        "subject@example.org",
        ALICE,
        "Ping carol@example.org",
        "Date: 1 Jun 2026 10:00:00 +0000\n",
        "Plain text.\n",
    );
    let in_body = raw_message(
        "body@example.org",
        ALICE,
        "Plain subject",
        "Date: 1 Jun 2026 11:00:00 +0000\n",
        "Ask carol@example.org.\n",
    );
    let plain = message("plain@example.org", BOB, "2 Jun 2026", "Plain", &[]);
    let missing = message("missing@example.org", BOB, "3 Jun 2026", "Gone", &[]);
    store_each(&mut client, &[&in_subject, &in_body, &plain]).await;
    for msg in [&in_subject, &in_body] {
        store_as_once(&client, &msg.message_id, msg.from_email.as_deref()).await;
    }
    // As if parsing or cleaning had changed since ingest: these fields
    // aren't what ingest once made of the archive's.
    client
        .batch_execute(
            "UPDATE messages SET subject = subject || ' (edited)' WHERE message_id = 'subject@example.org';
             UPDATE messages SET body = body || 'Edited.' WHERE message_id = 'body@example.org';",
        )
        .await
        .unwrap();
    let before = stored_text(&client, "subject@example.org").await;
    assert_eq!(restore(&mut client, &in_subject).await, Restored::Differs);
    assert_eq!(stored_text(&client, "subject@example.org").await, before);
    // Each field on its own: the cleaned body gets its address back, the
    // edited body stays.
    assert!(matches!(
        restore(&mut client, &in_body).await,
        Restored::Restored { .. }
    ));
    let (body, clean): (String, String) = client
        .query_one(
            "SELECT body, body_clean FROM messages WHERE message_id = 'body@example.org'",
            &[],
        )
        .await
        .map(|r| (r.get(0), r.get(1)))
        .unwrap();
    assert!(
        body.starts_with("Ask [email].") && body.ends_with("Edited."),
        "{body:?}"
    );
    assert_eq!(clean, "Ask carol@example.org.");
    assert_eq!(restore(&mut client, &plain).await, Restored::Unchanged);
    assert_eq!(restore(&mut client, &missing).await, Restored::Unchanged);
}

type ThreadStats = (i32, i32, String, String, i32, String, String);

/// Message and participant counts, subject, key, version, first and last day.
async fn thread_stats(client: &Client, id: i64) -> ThreadStats {
    let row = client
        .query_one(
            "SELECT message_count, participant_count, subject, subject_key, content_version,
                    started_at::date::text, last_at::date::text
             FROM threads WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap();
    (
        row.get(0),
        row.get(1),
        row.get(2),
        row.get(3),
        row.get(4),
        row.get(5),
        row.get(6),
    )
}

#[tokio::test]
async fn thread_statistics_follow_their_messages() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let a = message("a@example.org", ALICE, "1 Jun 2026", "[PATCH] Topic", &[]);
    let b = message(
        "b@example.org",
        BOB,
        "2 Jun 2026",
        "Re: [PATCH] Topic",
        &["a@example.org"],
    );
    let c = message(
        "c@example.org",
        ALICE,
        "3 Jun 2026",
        "Re: [PATCH] Topic",
        &["a@example.org", "b@example.org"],
    );
    let stored = store_each(&mut client, &[&b, &c]).await;
    let id = stored[0].thread_id;
    let (n, p, subject, key, version, first, last) = thread_stats(&client, id).await;
    assert_eq!((n, p), (2, 2));
    // The root isn't stored: the earliest message's subject.
    assert_eq!(subject, "[PATCH] Topic");
    assert_eq!(key, "topic");
    assert_eq!(
        (first.as_str(), last.as_str()),
        ("2026-06-02", "2026-06-03")
    );
    // Refreshing without changes changes nothing.
    let tx = client.transaction().await.unwrap();
    store::refresh_threads(&tx, &[id, id]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(thread_stats(&client, id).await.4, version);
    // The root arrives: its subject, dates and a new version.
    store_each(&mut client, &[&a]).await;
    let (n, p, subject, _, new_version, first, _) = thread_stats(&client, id).await;
    assert_eq!((n, p), (3, 2));
    assert_eq!(subject, "[PATCH] Topic");
    assert_eq!(first, "2026-06-01");
    assert!(new_version > version);
}

#[tokio::test]
async fn a_failed_message_leaves_the_transaction_usable() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let tx = client.transaction().await.unwrap();
    let good_message = ParsedMessage {
        message_id: "good@example.org".into(),
        from_name: "Alice Example".into(),
        from_email: Some("alice@example.org".into()),
        subject: "x".into(),
        body: "text".into(),
        ..Default::default()
    };
    // PostgreSQL rejects the NUL in References when the message row is
    // inserted, after its thread and author were created.
    let mut broken = good_message.clone();
    broken.message_id = "broken@example.org".into();
    broken.from_email = Some("bob@example.org".into());
    broken.references = vec!["root\u{0}@example.org".into()];
    assert!(store::upsert_message(&tx, &ctx(), &broken).await.is_err());
    let bad = good_message;
    let good = store::upsert_message(&tx, &ctx(), &bad).await.unwrap();
    assert!(good.inserted);
    tx.commit().await.unwrap();
    assert_eq!(count(&client, "SELECT count(*) FROM messages").await, 1);
    assert_eq!(count(&client, "SELECT count(*) FROM threads").await, 1);
    assert_eq!(count(&client, "SELECT count(*) FROM authors").await, 1);
}
