// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Threading, authors, addresses and thread statistics on a fresh database.

use std::sync::LazyLock;

use pghacker_core::testing::TestDb;
use pghacker_mail::parse::{self, ParsedMessage};
use pghacker_mail::store::{self, StoreContext, Stored};
use regex::Regex;
use tokio_postgres::Client;

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
async fn senders_addresses_and_placeholders_name_no_message() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    const TOM: &str = "Tom Lane <tom@example.org>";
    let bug = message("bug@example.org", TOM, "1 Nov 1998", "Parser bug?", &[]);
    let dump = message(
        "dump@example.org",
        TOM,
        "2 Nov 1998",
        "pg_dump inheritance",
        &[],
    );
    // exmh names the sender replied to before the Message-ID, and no
    // References: replies to anything Tom wrote shared tom@example.org.
    let exmh = |id: &str, date: &str, subject: &str, parent: &str| {
        let headers = format!(
            "Date: {date} 10:00:00 +0000\nTo: {TOM}\nIn-Reply-To: Message from Tom Lane <tom@example.org> \n\tof \"{date}\" <{parent}>\n"
        );
        raw_message(
            id,
            "Oliver Elphick <olly@example.org>",
            subject,
            &headers,
            "Text.",
        )
    };
    let on_bug = exmh(
        "on-bug@example.org",
        "3 Nov 1998",
        "Re: Parser bug?",
        "bug@example.org",
    );
    let on_dump = exmh(
        "on-dump@example.org",
        "4 Nov 1998",
        "Re: pg_dump inheritance",
        "dump@example.org",
    );
    // Mutt copied the address into References.
    let mutt = raw_message(
        "mutt@example.org",
        BOB,
        "Re: Parser bug?",
        "Date: 5 Nov 1998 10:00:00 +0000\nIn-Reply-To: <on-bug@example.org>\nReferences: <tom@example.org> <on-bug@example.org>\n",
        "Text.",
    );
    let only = raw_message(
        "only@example.org",
        BOB,
        "Re: Something else",
        "Date: 6 Nov 1998 10:00:00 +0000\nIn-Reply-To: <tom@example.org>\nReferences: <tom@example.org>\n",
        "Text.",
    );
    // Its own sender's address, before any message of that sender is stored.
    let own = raw_message(
        "own@example.org",
        "Philip Warner <pjw@example.org>",
        "Re: pg_dump inheritance",
        "Date: 7 Nov 1998 10:00:00 +0000\nIn-Reply-To: <dump@example.org>\nReferences: <Message from Philip Warner\n\t<pjw@example.org><dump@example.org>\n",
        "Text.",
    );
    // elm's placeholder for a message without a Message-ID.
    let elm = |id: &str, subject: &str| {
        raw_message(
            id,
            ALICE,
            subject,
            "Date: 8 Nov 1998 10:00:00 +0000\nIn-Reply-To: <no.id> from \"nolan\" at Nov 08, 1998 01:29:06 PM\n",
            "Text.",
        )
    };
    let elm1 = elm("elm1@example.org", "Re: initcap");
    let elm2 = elm("elm2@example.org", "Re: Var->sublevels_up");
    store_each(
        &mut client,
        &[
            &bug, &dump, &on_bug, &on_dump, &mutt, &only, &own, &elm1, &elm2,
        ],
    )
    .await;
    assert_eq!(
        threads(&client).await,
        [
            thread(
                "bug@example.org",
                &["bug@example.org", "on-bug@example.org", "mutt@example.org"]
            ),
            thread(
                "dump@example.org",
                &["dump@example.org", "on-dump@example.org", "own@example.org"]
            ),
            thread("only@example.org", &["only@example.org"]),
            thread("elm1@example.org", &["elm1@example.org"]),
            thread("elm2@example.org", &["elm2@example.org"]),
        ]
    );
    // Messages store the IDs that name messages.
    let stored: Vec<(String, Option<String>, Vec<String>)> = client
        .query(
            "SELECT message_id, parent_message_id, references_ids FROM messages ORDER BY sent_at, id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    let expect = |id: &str, parent: Option<&str>, references: &[&str]| {
        let row = (
            id.to_owned(),
            parent.map(str::to_owned),
            references.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
        );
        assert!(stored.contains(&row), "{row:?} in {stored:#?}");
    };
    expect("on-bug@example.org", Some("bug@example.org"), &[]);
    expect(
        "mutt@example.org",
        Some("on-bug@example.org"),
        &["on-bug@example.org"],
    );
    expect("only@example.org", None, &[]);
    expect(
        "own@example.org",
        Some("dump@example.org"),
        &["dump@example.org"],
    );
    expect("elm1@example.org", None, &[]);
}

/// A discussion; a reply to it 38 days later on a topic of its own, and the
/// reply to that; and replies that stay with the discussion: on its subject,
/// with a version, or too soon.
fn topic_mail() -> Vec<ParsedMessage> {
    let chain = ["p@example.org", "r@example.org"];
    vec![
        message(
            "p@example.org",
            ALICE,
            "1 Apr 2003",
            "Are we losing momentum?",
            &[],
        ),
        message(
            "r@example.org",
            BOB,
            "2 Apr 2003",
            "Re: Are we losing momentum?",
            &["p@example.org"],
        ),
        message(
            "soon@example.org",
            ALICE,
            "8 Apr 2003",
            "A question of its own",
            &chain,
        ),
        message(
            "iso@example.org",
            ALICE,
            "10 May 2003",
            "ISO 8601 intervals in the format with designators",
            &chain,
        ),
        message(
            "iso-re@example.org",
            BOB,
            "11 May 2003",
            "Re: ISO 8601 intervals in the format with designators",
            &["p@example.org", "r@example.org", "iso@example.org"],
        ),
        message(
            "late@example.org",
            BOB,
            "12 May 2003",
            "Re: Are we losing momentum?",
            &chain,
        ),
        message(
            "v5@example.org",
            ALICE,
            "13 May 2003",
            "Momentum patch v5",
            &chain,
        ),
    ]
}

fn topic_threads() -> Vec<Thread> {
    vec![
        thread(
            "p@example.org",
            &[
                "p@example.org",
                "r@example.org",
                "soon@example.org",
                "late@example.org",
                "v5@example.org",
            ],
        ),
        thread(
            "iso@example.org",
            &["iso@example.org", "iso-re@example.org"],
        ),
    ]
}

/// The threads of [`topic_mail`] stored in this order of Message-IDs.
async fn topic_threads_after(order: &[&str]) -> Vec<Thread> {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let mail = topic_mail();
    let messages: Vec<&ParsedMessage> = order
        .iter()
        .map(|id| mail.iter().find(|m| m.message_id == *id).unwrap())
        .collect();
    store_each(&mut client, &messages).await;
    threads(&client).await
}

#[tokio::test]
async fn replies_on_topics_of_their_own_start_threads() {
    let in_order: Vec<String> = topic_mail().into_iter().map(|m| m.message_id).collect();
    let in_order: Vec<&str> = in_order.iter().map(String::as_str).collect();
    assert_eq!(topic_threads_after(&in_order).await, topic_threads());
}

#[tokio::test]
async fn topics_gather_the_replies_that_arrived_first() {
    // The topic's reply first; then the topic and its reply before its parent.
    for order in [
        [
            "iso-re@example.org",
            "p@example.org",
            "r@example.org",
            "soon@example.org",
            "iso@example.org",
            "late@example.org",
            "v5@example.org",
        ],
        [
            "iso@example.org",
            "iso-re@example.org",
            "late@example.org",
            "p@example.org",
            "v5@example.org",
            "soon@example.org",
            "r@example.org",
        ],
    ] {
        assert_eq!(
            topic_threads_after(&order).await,
            topic_threads(),
            "{order:?}"
        );
    }
}

#[tokio::test]
async fn topics_leave_replies_that_extracts_cite() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let mail = topic_mail();
    let by_id = |id: &str| mail.iter().find(|m| m.message_id == id).unwrap();
    store_each(
        &mut client,
        &[
            by_id("iso-re@example.org"),
            by_id("p@example.org"),
            by_id("r@example.org"),
        ],
    )
    .await;
    client
        .batch_execute(
            "INSERT INTO features (slug, name, description, status) VALUES ('iso', 'ISO', 'd', 'proposed');
             INSERT INTO citations (extract_kind, extract_id, message_id, quote)
             SELECT 'feature', f.id, m.id, 'Text of iso-re' FROM features f, messages m
             WHERE m.message_id = 'iso-re@example.org'",
        )
        .await
        .unwrap();
    store_each(&mut client, &[by_id("iso@example.org")]).await;
    assert_eq!(
        threads(&client).await,
        [
            thread(
                "p@example.org",
                &["p@example.org", "r@example.org", "iso-re@example.org"]
            ),
            thread("iso@example.org", &["iso@example.org"]),
        ]
    );
}

#[tokio::test]
async fn a_parent_leaves_topics_that_extracts_cite() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let mail = topic_mail();
    let by_id = |id: &str| mail.iter().find(|m| m.message_id == id).unwrap();
    // The topic and its reply first, whose parent isn't stored yet: they
    // share the discussion's thread.
    store_each(
        &mut client,
        &[by_id("iso-re@example.org"), by_id("iso@example.org")],
    )
    .await;
    client
        .batch_execute(
            "INSERT INTO features (slug, name, description, status) VALUES ('iso', 'ISO', 'd', 'proposed');
             INSERT INTO citations (extract_kind, extract_id, message_id, quote)
             SELECT 'feature', f.id, m.id, 'Text of iso-re' FROM features f, messages m
             WHERE m.message_id = 'iso-re@example.org'",
        )
        .await
        .unwrap();
    // The parent's arrival makes the topic one, but extracts cite its reply:
    // nothing moves, and no thread is left empty.
    store_each(
        &mut client,
        &[by_id("p@example.org"), by_id("r@example.org")],
    )
    .await;
    assert_eq!(
        threads(&client).await,
        [thread(
            "p@example.org",
            &[
                "p@example.org",
                "r@example.org",
                "iso@example.org",
                "iso-re@example.org"
            ]
        )]
    );
    assert_eq!(count(&client, "SELECT count(*) FROM threads").await, 1);
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
async fn a_refresh_and_another_store_into_its_thread_do_not_deadlock() {
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let mut other = db.indexer().await.unwrap();
    let watcher = db.indexer().await.unwrap();
    let root = message("root@example.org", ALICE, "1 Jun 2026", "Topic", &[]);
    let thread = store_each(&mut client, &[&root]).await[0].thread_id;
    let first = message(
        "r1@example.org",
        BOB,
        "2 Jun 2026",
        "Re: Topic",
        &["root@example.org"],
    );
    let second = message(
        "r2@example.org",
        ALICE,
        "3 Jun 2026",
        "Re: Topic",
        &["root@example.org"],
    );
    // A transaction stores a reply, and updates the thread's counts.
    let tx = client.transaction().await.unwrap();
    let stored = store::upsert_message(&tx, &ctx(), &first).await.unwrap();
    assert_eq!(stored.thread_id, thread);
    tokio::join!(
        // Another stores a reply too: inserting it locks the thread FOR KEY
        // SHARE, and its count update waits for the first transaction.
        async {
            let tx = other.transaction().await.unwrap();
            let stored = store::upsert_message(&tx, &ctx(), &second).await.unwrap();
            assert_eq!(stored.thread_id, thread);
            tx.commit().await.unwrap();
        },
        // Then the first refreshes the thread, which a FOR UPDATE lock would
        // make wait for the second: a deadlock.
        async {
            let waiting = "SELECT count(*) FROM pg_stat_activity
                           WHERE datname = current_database() AND wait_event_type = 'Lock'";
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while count(&watcher, waiting).await == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the second store didn't wait"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            store::refresh_threads(&tx, &[thread]).await.unwrap();
            tx.commit().await.unwrap();
        },
    );
    assert_eq!(thread_stats(&client, thread).await.0, 3);
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
    // PostgreSQL rejects the message row, whose source doesn't exist, after
    // its thread and author were created.
    let mut broken = good_message.clone();
    broken.message_id = "broken@example.org".into();
    broken.from_email = Some("bob@example.org".into());
    broken.references = vec!["root@example.org".into()];
    let missing_source = StoreContext {
        source_id: Some(424_242),
        ..ctx()
    };
    assert!(
        store::upsert_message(&tx, &missing_source, &broken)
            .await
            .is_err()
    );
    let bad = good_message;
    let good = store::upsert_message(&tx, &ctx(), &bad).await.unwrap();
    assert!(good.inserted);
    tx.commit().await.unwrap();
    assert_eq!(count(&client, "SELECT count(*) FROM messages").await, 1);
    assert_eq!(count(&client, "SELECT count(*) FROM threads").await, 1);
    assert_eq!(count(&client, "SELECT count(*) FROM authors").await, 1);
}
