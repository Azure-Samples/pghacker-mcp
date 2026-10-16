// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use chrono::TimeZone;

use super::*;

fn day(d: i64) -> Option<DateTime<Utc>> {
    Some(Utc.with_ymd_and_hms(2003, 1, 1, 12, 0, 0).unwrap() + TimeDelta::days(d))
}

/// A message of thread `thread`, sent on day `d`, linked to `ancestors`.
fn msg(id: i64, message_id: &str, thread: i64, d: i64, subject: &str, ancestors: &[&str]) -> Msg {
    Msg {
        id,
        message_id: message_id.into(),
        thread,
        list: "pgsql-hackers".into(),
        subject: subject.into(),
        sent_at: day(d),
        starts_topic: false,
        changed: false,
        ancestors: ancestors.iter().map(|a| a.to_string()).collect(),
    }
}

/// `m`, with threading IDs that the reread corrected.
fn corrected(mut m: Msg) -> Msg {
    m.changed = true;
    m
}

fn new(root: &str) -> Destination {
    Destination::New(root.into())
}

#[test]
fn ancestors_stop_at_the_newest_topic() {
    let refs = |ids: &[&str]| ids.iter().map(|i| i.to_string()).collect::<Vec<_>>();
    let is_topic = |id: &str| id == "topic";
    assert_eq!(
        ancestors(
            "m",
            Some("p"),
            &refs(&["root", "topic", "q", "p"]),
            false,
            is_topic
        ),
        ["p", "q", "topic"]
    );
    // Its own ID and repeats go; a topic has none.
    assert_eq!(
        ancestors("m", Some("m"), &refs(&["a", "a"]), false, |_| false),
        ["a"]
    );
    assert!(ancestors("m", Some("p"), &refs(&["p"]), true, |_| false).is_empty());
}

#[test]
fn pieces_keep_their_thread_join_others_or_become_threads() {
    let mut input = Input::default();
    input.roots.insert(1, "r1".into());
    input.messages = vec![
        msg(1, "r1", 1, 0, "Heads up: 7.3.3", &[]),
        msg(2, "a", 1, 1, "Re: Heads up: 7.3.3", &["r1"]),
        // Glued on by an address, now gone: a conversation whose root isn't
        // stored, and a reply to another thread's message.
        corrected(msg(3, "x", 1, 300, "Re: Windows build", &["u1"])),
        msg(4, "y", 1, 301, "Re: Windows build", &["x", "u1"]),
        corrected(msg(5, "z", 1, 500, "Re: Parser bug?", &["o1"])),
    ];
    input.outside_messages.insert("o1".into(), 9);
    let p = partition(&input);
    assert_eq!(
        p.destinations,
        BTreeMap::from([
            (1, Destination::Thread(1)),
            (2, Destination::Thread(1)),
            (3, new("u1")),
            (4, new("u1")),
            (5, Destination::Thread(9)),
        ])
    );
    assert!(p.reroots.is_empty());

    // An unstored ancestor that another thread's messages share links to it.
    input
        .outside_shared
        .insert("u1".into(), BTreeSet::from([7]));
    assert_eq!(partition(&input).destinations[&3], Destination::Thread(7));
}

#[test]
fn topics_start_threads_of_their_own() {
    let mut input = Input::default();
    input.roots.insert(1, "p".into());
    let mut topic = msg(3, "iso", 1, 40, "ISO 8601 intervals", &[]);
    topic.starts_topic = true;
    input.messages = vec![
        msg(1, "p", 1, 0, "Are we losing momentum?", &[]),
        msg(2, "r", 1, 1, "Re: Are we losing momentum?", &["p"]),
        topic,
        msg(4, "iso-re", 1, 41, "Re: ISO 8601 intervals", &["iso"]),
    ];
    let p = partition(&input);
    assert_eq!(p.destinations[&1], Destination::Thread(1));
    assert_eq!(p.destinations[&2], Destination::Thread(1));
    assert_eq!(p.destinations[&3], new("iso"));
    assert_eq!(p.destinations[&4], new("iso"));
}

#[test]
fn a_thread_rooted_at_no_message_keeps_its_largest_piece() {
    let mut input = Input::default();
    input.roots.insert(2, "no.id".into());
    input.messages = vec![
        msg(1, "s", 2, 0, "Something", &[]),
        msg(2, "p", 2, 10, "Var->sublevels_up", &[]),
        msg(3, "q", 2, 11, "Re: Var->sublevels_up", &["p"]),
    ];
    let p = partition(&input);
    assert_eq!(p.destinations[&2], Destination::Thread(2));
    assert_eq!(p.destinations[&3], Destination::Thread(2));
    assert_eq!(p.destinations[&1], new("s"));
    assert_eq!(p.reroots, BTreeMap::from([(2, "p".to_owned())]));
}

#[test]
fn replies_without_ancestors_join_by_subject() {
    let mut input = Input::default();
    input.roots.insert(1, "big".into());
    let subject = "Big discussion about the planner";
    let reply = format!("Re: {subject}");
    input.messages = vec![
        msg(1, "big", 1, 0, subject, &[]),
        msg(2, "re", 1, 2, &reply, &["big"]),
        // Within 60 days of the discussion: it joins.
        msg(3, "h1", 1, 30, &reply, &[]),
        // Far later: alone, with the next such reply.
        msg(4, "h2", 1, 200, &reply, &[]),
        msg(5, "h3", 1, 220, &reply, &[]),
        // Another subject, which other threads have.
        msg(6, "h4", 1, 30, "Re: An unrelated long subject line", &[]),
    ];
    input
        .outside_subjects
        .insert(6, vec![(8, day(20)), (9, day(31))]);
    let p = partition(&input);
    assert_eq!(p.destinations[&3], Destination::Thread(1));
    assert_eq!(p.destinations[&4], new("h2"));
    assert_eq!(p.destinations[&5], new("h2"));
    assert_eq!(p.destinations[&6], Destination::Thread(9), "the nearest");
}

#[test]
fn a_piece_linked_to_two_threads_joins_one_and_tells() {
    let mut input = Input::default();
    input.roots.insert(1, "r".into());
    input.messages = vec![
        msg(1, "r", 1, 0, "Topic", &[]),
        corrected(msg(2, "m", 1, 1, "Re: Topic", &["o1", "o2"])),
        msg(3, "n", 1, 2, "Re: Topic", &["o2", "m"]),
    ];
    input.outside_messages.insert("o1".into(), 9);
    input.outside_messages.insert("o2".into(), 8);
    let p = partition(&input);
    assert_eq!(p.destinations[&2], Destination::Thread(8), "linked most");
    assert_eq!(p.destinations[&3], Destination::Thread(8));
    assert_eq!(p.destinations[&1], Destination::Thread(1));
    assert_eq!(p.also, BTreeMap::from([(8, BTreeSet::from([9]))]));
}

/// No thread the partition makes or re-roots takes a root that a thread
/// keeps, inside or outside the input.
fn assert_roots_free(input: &Input, p: &Partition) {
    let kept: BTreeSet<i64> = p
        .destinations
        .values()
        .filter_map(|d| match d {
            Destination::Thread(t) => Some(*t),
            Destination::New(_) => None,
        })
        .collect();
    let mut taken: BTreeSet<&str> = input.outside_roots.keys().map(String::as_str).collect();
    for (t, root) in &input.roots {
        if kept.contains(t) && !p.reroots.contains_key(t) {
            taken.insert(root);
        }
    }
    for root in p.reroots.values() {
        assert!(taken.insert(root), "re-rooted at a taken root {root}");
    }
    let new: BTreeSet<&str> = p
        .destinations
        .values()
        .filter_map(|d| match d {
            Destination::New(root) => Some(root.as_str()),
            Destination::Thread(_) => None,
        })
        .collect();
    for root in new {
        assert!(
            !taken.contains(root),
            "a new thread rooted at a taken root {root}"
        );
    }
}

#[test]
fn replies_without_ancestors_take_their_replies_along() {
    let mut input = Input::default();
    input.roots.insert(56800, "no.id".into());
    let mut topic = msg(4, "topic", 56800, 120, "Proposal: release 7.2b2 today", &[]);
    topic.starts_topic = true;
    input.messages = vec![
        // Glued on by elm's placeholder: a reply that lost its parent, with
        // replies of its own.
        msg(
            1,
            "initcap",
            56800,
            0,
            "Re: initcap incompatibility issue",
            &[],
        ),
        msg(
            2,
            "initcap-re",
            56800,
            1,
            "Re: initcap incompatibility issue",
            &["initcap"],
        ),
        msg(
            3,
            "initcap-re2",
            56800,
            2,
            "Re: initcap incompatibility issue",
            &["initcap-re", "initcap"],
        ),
        topic,
        msg(
            5,
            "topic-re",
            56800,
            121,
            "Re: Proposal: release 7.2b2 today",
            &["topic"],
        ),
        msg(
            6,
            "topic-re2",
            56800,
            122,
            "Re: Proposal: release 7.2b2 today",
            &["topic-re", "topic"],
        ),
        // Replies to the topic that lost their headers, one with a reply.
        msg(
            7,
            "late",
            56800,
            125,
            "Re: Proposal: release 7.2b2 today",
            &[],
        ),
        msg(
            8,
            "late-re",
            56800,
            126,
            "Re: Proposal: release 7.2b2 today",
            &["late"],
        ),
    ];
    input.outside_subjects.insert(1, vec![(56799, day(0))]);
    let p = partition(&input);
    for m in 1..=3 {
        assert_eq!(
            p.destinations[&m],
            Destination::Thread(56799),
            "message {m}"
        );
    }
    // The rootless thread keeps the topic's piece; the replies that lost
    // their headers join it with theirs.
    for m in 4..=8 {
        assert_eq!(
            p.destinations[&m],
            Destination::Thread(56800),
            "message {m}"
        );
    }
    assert_eq!(p.reroots, BTreeMap::from([(56800, "topic".to_owned())]));
    assert_roots_free(&input, &p);

    // Without the rootless thread, they join the topic's new thread.
    input.roots.clear();
    input.roots.insert(1, "initcap".into());
    let p = partition(&input);
    assert_eq!(
        p.destinations[&1],
        Destination::Thread(1),
        "it holds the root"
    );
    for m in 4..=8 {
        assert_eq!(p.destinations[&m], new("topic"), "message {m}");
    }
    assert_roots_free(&input, &p);
}

#[test]
fn a_reply_without_ancestors_keeps_the_thread_it_roots() {
    // A thread rooted at a reply that lost its headers, and a topic below it
    // a year later.
    let mut input = Input::default();
    input.roots.insert(72095, "sunos".into());
    let mut topic = msg(2, "tutorial", 72095, 397, "tutorial won't compile", &[]);
    topic.starts_topic = true;
    input.messages = vec![
        msg(
            1,
            "sunos",
            72095,
            0,
            "RE: [HACKERS] Re: Postgres for Sunos 4.1.4",
            &[],
        ),
        topic,
        msg(
            3,
            "tutorial-re",
            72095,
            398,
            "Re: tutorial won't compile",
            &["tutorial"],
        ),
    ];
    let p = partition(&input);
    assert_eq!(p.destinations[&1], Destination::Thread(72095));
    assert_eq!(p.destinations[&2], new("tutorial"));
    assert_eq!(p.destinations[&3], new("tutorial"));
    assert!(p.reroots.is_empty());
    assert_roots_free(&input, &p);
}

#[test]
fn a_piece_with_its_threads_root_keeps_it_and_leaves_linked_threads_apart() {
    // "support for MERGE" loses a topic. One of its replies also answers
    // "MERGE and parsing with prepared statements", which ingest kept apart
    // because extracts cite both: it stays with the root, and the other
    // thread stays as it is.
    let mut input = Input::default();
    input.roots.insert(12364, "merge".into());
    let mut topic = msg(
        3,
        "bulk",
        12364,
        495,
        "Should use MERGE use BulkInsertState ?",
        &[],
    );
    topic.starts_topic = true;
    input.messages = vec![
        msg(1, "merge", 12364, 0, "support for MERGE", &[]),
        msg(2, "r2", 12364, 450, "Re: support for MERGE", &["merge"]),
        topic,
        msg(
            4,
            "simon",
            12364,
            590,
            "Re: MERGE and parsing with prepared statements",
            &["prep2", "prep1", "r2"],
        ),
    ];
    input.outside_messages.insert("prep1".into(), 11580);
    input.outside_messages.insert("prep2".into(), 11580);
    let p = partition(&input);
    for m in [1, 2, 4] {
        assert_eq!(
            p.destinations[&m],
            Destination::Thread(12364),
            "message {m}"
        );
    }
    assert_eq!(p.destinations[&3], new("bulk"));
    assert!(p.reroots.is_empty());
    assert_eq!(p.also, BTreeMap::from([(12364, BTreeSet::from([11580]))]));
    assert_roots_free(&input, &p);

    // A root that isn't stored, which the piece's messages name, holds it
    // as well.
    let mut input = Input::default();
    input.roots.insert(11641, "u0".into());
    let mut topic = msg(3, "restart", 11641, 400, "storing an explicit nonce", &[]);
    topic.starts_topic = true;
    input.messages = vec![
        msg(1, "a", 11641, 0, "Re: storing an explicit nonce", &["u0"]),
        msg(
            2,
            "b",
            11641,
            1,
            "Re: storing an explicit nonce",
            &["o1", "a", "u0"],
        ),
        topic,
    ];
    input.outside_messages.insert("o1".into(), 12411);
    let p = partition(&input);
    assert_eq!(p.destinations[&1], Destination::Thread(11641));
    assert_eq!(p.destinations[&2], Destination::Thread(11641));
    assert_eq!(p.destinations[&3], new("restart"));
    assert!(p.reroots.is_empty());
    assert_roots_free(&input, &p);
}

#[test]
fn a_piece_holding_two_roots_leaves_each_thread_its_messages() {
    // As above, but "MERGE and parsing with prepared statements" is
    // affected too: each thread keeps its messages.
    let mut input = Input::default();
    input.roots.insert(12364, "merge".into());
    input.roots.insert(11580, "prep0".into());
    let mut topic = msg(
        5,
        "bulk",
        12364,
        495,
        "Should use MERGE use BulkInsertState ?",
        &[],
    );
    topic.starts_topic = true;
    let prepared = "MERGE and parsing with prepared statements";
    let re_prepared = format!("Re: {prepared}");
    input.messages = vec![
        msg(1, "merge", 12364, 0, "support for MERGE", &[]),
        msg(2, "r2", 12364, 450, "Re: support for MERGE", &["merge"]),
        msg(3, "prep0", 11580, 560, prepared, &[]),
        msg(4, "prep1", 11580, 561, &re_prepared, &["prep0"]),
        topic,
        msg(
            6,
            "simon",
            12364,
            590,
            &re_prepared,
            &["prep1", "prep0", "r2"],
        ),
    ];
    let p = partition(&input);
    for m in [1, 2, 6] {
        assert_eq!(
            p.destinations[&m],
            Destination::Thread(12364),
            "message {m}"
        );
    }
    assert_eq!(p.destinations[&3], Destination::Thread(11580));
    assert_eq!(p.destinations[&4], Destination::Thread(11580));
    assert_eq!(p.destinations[&5], new("bulk"));
    assert!(p.reroots.is_empty());
    assert_eq!(p.also, BTreeMap::from([(12364, BTreeSet::from([11580]))]));
    assert_roots_free(&input, &p);
}

#[test]
fn pieces_without_corrected_ids_leave_linked_threads_apart() {
    // "storing an explicit nonce" starts a topic in reply to a thread whose
    // message, the thread's root, isn't stored. Another thread, which ingest
    // kept apart, is rooted at one of its replies, and a second topic
    // follows a year later: the thread keeps the first topic's messages,
    // under the topic.
    let mut input = Input::default();
    input.roots.insert(11641, "u0".into());
    let mut nonce = msg(1, "nonce", 11641, 0, "storing an explicit nonce", &[]);
    nonce.starts_topic = true;
    let mut checksums = msg(
        4,
        "checksums",
        11641,
        380,
        "better page-level checksums",
        &[],
    );
    checksums.starts_topic = true;
    input.messages = vec![
        nonce,
        msg(
            2,
            "r27",
            11641,
            2,
            "Re: storing an explicit nonce",
            &["nonce"],
        ),
        msg(
            3,
            "r28",
            11641,
            3,
            "Re: storing an explicit nonce",
            &["r27", "nonce"],
        ),
        checksums,
    ];
    input.outside_roots.insert("r27".into(), 12411);
    let p = partition(&input);
    for m in 1..=3 {
        assert_eq!(
            p.destinations[&m],
            Destination::Thread(11641),
            "message {m}"
        );
    }
    assert_eq!(p.destinations[&4], new("checksums"));
    assert_eq!(p.reroots, BTreeMap::from([(11641, "nonce".to_owned())]));
    assert_roots_free(&input, &p);

    // With corrected IDs, the piece joins the thread rooted at its reply.
    input.messages[1].changed = true;
    let p = partition(&input);
    for m in 1..=3 {
        assert_eq!(
            p.destinations[&m],
            Destination::Thread(12411),
            "message {m}"
        );
    }
    assert_roots_free(&input, &p);
}

#[test]
fn a_topic_joins_the_thread_rooted_at_it() {
    // Replies to a topic arrived first and got a thread rooted at it.
    let mut input = Input::default();
    input.roots.insert(5817, "fdw".into());
    let mut topic = msg(
        3,
        "wholerow",
        5817,
        2340,
        "Partition-wise join with whole row vars",
        &[],
    );
    topic.starts_topic = true;
    input.messages = vec![
        msg(
            1,
            "fdw",
            5817,
            0,
            "Expression errors with \"FOR UPDATE\" and postgres_fdw",
            &[],
        ),
        msg(
            2,
            "fdw-re",
            5817,
            1,
            "Re: Expression errors with \"FOR UPDATE\" and postgres_fdw",
            &["fdw"],
        ),
        topic,
        msg(
            4,
            "wholerow-re",
            5817,
            2341,
            "Re: Partition-wise join with whole row vars",
            &["wholerow"],
        ),
    ];
    input.outside_roots.insert("wholerow".into(), 4220);
    let p = partition(&input);
    assert_eq!(p.destinations[&1], Destination::Thread(5817));
    assert_eq!(p.destinations[&2], Destination::Thread(5817));
    assert_eq!(p.destinations[&3], Destination::Thread(4220));
    assert_eq!(p.destinations[&4], Destination::Thread(4220));
    assert_roots_free(&input, &p);
}

#[test]
fn a_reply_without_ancestors_stays_in_its_thread_with_its_subject() {
    // Two threads with one subject, which ingest kept apart, and a topic
    // that makes the first affected: its replies without ancestors stay,
    // though the other thread is nearer.
    let subject = "Foreign join pushdown vs EvalPlanQual";
    let reply = format!("Re: {subject}");
    let mut input = Input::default();
    input.roots.insert(27984, "fj".into());
    let mut topic = msg(3, "hook", 27984, 70, "Hooking at standard_join_search", &[]);
    topic.starts_topic = true;
    input.messages = vec![
        msg(1, "fj", 27984, 0, subject, &[]),
        msg(2, "fj-re", 27984, 110, &reply, &["fj"]),
        topic,
        msg(4, "kaigai", 27984, 117, &reply, &[]),
    ];
    input.outside_subjects.insert(4, vec![(27532, day(118))]);
    let p = partition(&input);
    assert_eq!(p.destinations[&4], Destination::Thread(27984));
    assert_eq!(p.destinations[&3], new("hook"));
    assert_roots_free(&input, &p);
}
