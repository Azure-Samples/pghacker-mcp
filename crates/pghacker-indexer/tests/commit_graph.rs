//! The commit history in the graph: a small git history installed beside
//! the mail, then synced, regrouped, cited and resolved, with the authors
//! changing under it. What the history's writers and the triggers record
//! must leave exactly the graph that a full sync computes.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use pghacker_code::history::sync::{SyncOptions, sync};
use pghacker_code::history::{self, BuildOptions, backpatch, links};
use pghacker_core::config::Env;
use pghacker_core::testing::TestDb;
use pghacker_indexer::settings::Settings;
use pghacker_indexer::{Indexer, anchors, extraction, graph};
use serde_json::{Value, json};
use tokio_postgres::Client;

/// A git repository whose commits each name their author, a minute apart.
struct Repo {
    dir: tempfile::TempDir,
    tick: u32,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
            tick: 0,
        };
        repo.git("Test", &["init", "-q", "-b", "master"]);
        repo
    }

    fn path(&self) -> PathBuf {
        self.dir.path().to_owned()
    }

    fn git(&self, author: &str, args: &[&str]) -> String {
        let date = format!("{} +0000", 1_767_225_600 + self.tick * 60);
        let output = Command::new("git")
            .args(args)
            .current_dir(self.dir.path())
            .env("GIT_AUTHOR_NAME", author)
            .env("GIT_AUTHOR_EMAIL", "dev@example.org")
            .env("GIT_COMMITTER_NAME", author)
            .env("GIT_COMMITTER_EMAIL", "dev@example.org")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", self.dir.path())
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn remove(&self, path: &str) {
        std::fs::remove_file(self.dir.path().join(path)).unwrap();
    }

    fn checkout(&self, args: &[&str]) {
        let mut all = vec!["checkout", "-q"];
        all.extend(args);
        self.git("Test", &all);
    }

    fn commit(&mut self, author: &str, message: &str) -> String {
        self.tick += 1;
        self.git(author, &["add", "-A"]);
        self.git(author, &["commit", "-q", "-m", message]);
        self.git(author, &["rev-parse", "HEAD"])
    }
}

const JSONTABLE: &str = "src/backend/parser/parse_jsontable.c";

/// The history: master's c0 to c3 and REL_1_STABLE's b1, which back-patches
/// c2's fix. Tom Lane's name is one author's, two authors share Bob Twice's
/// (ignoring case), and no author has Nobody Known's.
struct History {
    repo: Repo,
    c0: String,
    c1: String,
    b1: String,
    c2: String,
    c3: String,
}

fn history() -> History {
    let mut repo = Repo::new();
    repo.write(JSONTABLE, "int a;\n");
    repo.write(
        "src/include/nodes/parsenodes.h",
        "typedef struct Node Node;\n",
    );
    repo.write("README", "PostgreSQL\n");
    repo.write("contrib/old/old.c", "int old;\n");
    let c0 = repo.commit("Tom Lane", "Initial import");
    repo.write(JSONTABLE, "int a;\nint b;\n");
    let c1 = repo.commit("Bob Twice", "Teach JSON_TABLE b");
    repo.checkout(&["-b", "REL_1_STABLE", &c1]);
    repo.write(JSONTABLE, "int a;\nint b;\nint c;\n");
    let b1 = repo.commit("Tom Lane", "Fix a crash in JSON_TABLE");
    repo.checkout(&["master"]);
    repo.write(JSONTABLE, "int a;\nint b;\nint c;\n");
    let c2 = repo.commit(
        "Tom Lane",
        "Fix a crash in JSON_TABLE\n\nDiscussion: https://postgr.es/m/m1@example.org",
    );
    repo.remove("contrib/old/old.c");
    repo.write("README", "PostgreSQL, the database\n");
    let c3 = repo.commit(
        "Nobody Known",
        "Drop contrib/old, as old@example.org asked in https://postgr.es/m/m1@example.org",
    );
    History {
        repo,
        c0,
        c1,
        b1,
        c2,
        c3,
    }
}

const MAIL: &str = r#"
INSERT INTO authors (id, display_name, message_count, first_seen, last_seen) VALUES
    (1, 'Tom Lane', 1, '2026-02-01', '2026-02-01'), (2, 'Bob Twice', 1, '2026-01-01', '2026-01-01'),
    (3, 'bob twice', 1, '2026-02-02', '2026-02-02'), (4, 'Alice Hacker', 1, '2026-02-03', '2026-02-03');
INSERT INTO author_names (author_id, name, count) VALUES
    (1, 'Tom Lane', 1), (2, 'Bob Twice', 1), (3, 'bob twice', 1), (4, 'Alice Hacker', 1);
SELECT setval('authors_id_seq', 100);
INSERT INTO threads (id, list, root_message_id, subject, message_count, participant_count, started_at, last_at) VALUES
    (10, 'pgsql-hackers', 'm1@example.org', 'JSON_TABLE crash', 4, 4, '2026-01-01', '2026-02-03');
SELECT setval('threads_id_seq', 100);
INSERT INTO messages (id, message_id, list, thread_id, parent_message_id, author_id, from_name, subject, sent_at) VALUES
    (100, 'm1@example.org', 'pgsql-hackers', 10, NULL, 2, 'Bob Twice', 'JSON_TABLE crash', '2026-01-01'),
    (101, 'm2@example.org', 'pgsql-hackers', 10, 'm1@example.org', 1, 'Tom Lane', 'Re: JSON_TABLE crash', '2026-02-01'),
    (102, 'm3@example.org', 'pgsql-hackers', 10, 'm2@example.org', 3, 'bob twice', 'Re: JSON_TABLE crash', '2026-02-02'),
    (103, 'm4@example.org', 'pgsql-hackers', 10, 'm3@example.org', 4, 'Alice Hacker', 'Re: JSON_TABLE crash', '2026-02-03');
SELECT setval('messages_id_seq', 1000);
"#;

/// Builds the history in a schema of its own and installs it in pghacker.
async fn install(db: &TestDb, repo: &Repo) {
    history::build(
        &db.admin_url,
        &BuildOptions {
            git_dir: repo.path(),
            master: "master".into(),
            url: None,
            schema: "pghacker_history_build".into(),
            paths: Vec::new(),
            install: true,
        },
    )
    .await
    .unwrap();
}

/// The mail, a full sync without a history, then the history installed and
/// the graph synced fully again; m2 cites c2 by an abbreviated SHA.
async fn installed() -> (TestDb, History, Client) {
    let h = history();
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    client.batch_execute(MAIL).await.unwrap();
    client
        .execute(
            "INSERT INTO anchors (message_id, kind, value) VALUES (101, 'commit', $1)",
            &[&&h.c2[..9]],
        )
        .await
        .unwrap();
    graph::run(&mut client, Duration::from_secs(10), None)
        .await
        .unwrap();
    // Without a history, mail's citation is the only commit.
    assert_eq!(count(&client, "Commit").await, 1);
    for label in ["MODIFIES", "DISCUSSED_IN", "BACKPATCH_OF", "COMMITTED"] {
        assert_eq!(count(&client, label).await, 0, "{label}");
    }
    install(&db, &h.repo).await;
    assert_eq!(
        changes(&client).await,
        0,
        "the history's tables have no triggers"
    );
    let report = graph::run(&mut client, Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_some(), "an install asks for a full sync");
    (db, h, client)
}

async fn changes(client: &Client) -> i64 {
    client
        .query_one("SELECT count(*) FROM graph_changes", &[])
        .await
        .unwrap()
        .get(0)
}

/// The change entries waiting: (label, key, end key).
async fn entries(client: &Client) -> BTreeSet<(Option<String>, String, Option<String>)> {
    client
        .query("SELECT label, key, end_key FROM graph_changes", &[])
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

/// The SHAs that the waiting entries name.
async fn named_commits(client: &Client) -> BTreeSet<String> {
    entries(client)
        .await
        .into_iter()
        .flat_map(|(_, key, end)| [Some(key), end])
        .flatten()
        .filter_map(|key| key.strip_prefix("commit:").map(str::to_owned))
        .collect()
}

/// A label's vertices or edges, as the bookkeeping has them.
async fn count(client: &Client, label: &str) -> i64 {
    client
        .query_one(
            "SELECT (SELECT count(*) FROM graph_nodes WHERE label = $1)
                  + (SELECT count(*) FROM graph_edges WHERE label = $1)",
            &[&label],
        )
        .await
        .unwrap()
        .get(0)
}

async fn has_edge(client: &Client, label: &str, start: &str, end: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT FROM graph_edges WHERE label = $1 AND start_key = $2 AND end_key = $3)",
            &[&label, &start, &end],
        )
        .await
        .unwrap()
        .get(0)
}

/// The start keys of the edges of a label into `end`.
async fn starts(client: &Client, label: &str, end: &str) -> Vec<String> {
    client
        .query(
            "SELECT start_key FROM graph_edges WHERE label = $1 AND end_key = $2 ORDER BY 1",
            &[&label, &end],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn has_vertex(client: &Client, key: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT FROM graph_nodes WHERE key = $1)",
            &[&key],
        )
        .await
        .unwrap()
        .get(0)
}

async fn props(client: &Client, key: &str) -> Value {
    client
        .query_one("SELECT props FROM graph_nodes WHERE key = $1", &[&key])
        .await
        .unwrap()
        .get(0)
}

/// Applies the waiting changes and checks that the graph is what a full sync
/// computes.
async fn converged(client: &mut Client) {
    loop {
        let stats = graph::sync_changes(client, graph::BATCH).await.unwrap();
        if stats.changes == 0 {
            break;
        }
    }
    let drift = graph::check(client).await.unwrap();
    assert!(drift.is_empty(), "{drift:#?}");
}

/// A reconcile, once due, changes nothing.
async fn reconciled(client: &mut Client) {
    client
        .execute(
            "UPDATE graph_sync_state SET last_full_at = now() - interval '2 hours'",
            &[],
        )
        .await
        .unwrap();
    let report = graph::run(
        client,
        Duration::from_secs(10),
        Some(Duration::from_secs(3600)),
    )
    .await
    .unwrap();
    let full = report.full.expect("a reconcile");
    assert_eq!(full.changed(), 0, "{full:?}");
}

fn commit(sha: &str) -> String {
    format!("commit:{sha}")
}

async fn indexer(db: &TestDb) -> Indexer {
    let root = pghacker_core::config::repo_root().unwrap();
    let cache = tempfile::tempdir().unwrap().keep();
    let env = Env::from_pairs([("PGHACKER_INDEXER_DATABASE_URL", db.indexer_url.as_str())]);
    let mut settings = Settings::from_env(&env).unwrap();
    settings.slices_path = root.join("config/test-slices.toml");
    settings.cache_dir = cache.clone();
    settings.git_dir = cache;
    Indexer::new(settings, &env).unwrap()
}

fn args<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

/// Applies the clone's new commits to the installed history, one per
/// transaction, as the indexer's role.
async fn synced_history(db: &TestDb, repo: &Repo) -> u64 {
    sync(
        db.indexer().await.unwrap(),
        SyncOptions {
            git_dir: repo.path(),
            schema: "pghacker".into(),
            batch: 1,
        },
    )
    .await
    .unwrap()
    .commits
}

#[tokio::test]
async fn a_full_sync_projects_the_history() {
    let (db, h, mut client) = installed().await;
    // Every commit, with its metadata, and mail's citation as written.
    assert_eq!(count(&client, "Commit").await, 6);
    let c2 = props(&client, &commit(&h.c2)).await;
    assert_eq!(c2["subject"], "Fix a crash in JSON_TABLE");
    assert_eq!(c2["author"], "Tom Lane");
    assert_eq!(c2["branch"], "master");
    assert_eq!(c2["backpatch"], h.c2.as_str());
    assert!(
        c2["committed_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-01-01T00:04:00"),
        "{c2}"
    );
    let b1 = props(&client, &commit(&h.b1)).await;
    assert_eq!(
        (&b1["branch"], &b1["backpatch"]),
        (&json!("REL_1_STABLE"), &json!(h.c2))
    );
    let c0 = props(&client, &commit(&h.c0)).await;
    assert!(c0.get("backpatch").is_none(), "{c0}");
    // Subjects are kept as written, addresses and all: the history is public.
    assert_eq!(
        props(&client, &commit(&h.c3)).await["subject"],
        "Drop contrib/old, as old@example.org asked in https://postgr.es/m/m1@example.org"
    );
    let cited = commit(&h.c2[..9]);
    assert_eq!(
        props(&client, &cited).await,
        json!({ "key": cited, "sha": &h.c2[..9] })
    );
    assert!(has_edge(&client, "MENTIONS", "message:m2@example.org", &cited).await);

    // What each commit changed, and the threads that discussed it.
    assert_eq!(count(&client, "MODIFIES").await, 9);
    for (sha, path) in [
        (&h.c0, "contrib/old/old.c"),
        (&h.c0, "src/include/nodes/parsenodes.h"),
        (&h.b1, JSONTABLE),
        (&h.c3, "contrib/old/old.c"),
        (&h.c3, "README"),
    ] {
        assert!(
            has_edge(&client, "MODIFIES", &commit(sha), &format!("file:{path}")).await,
            "{sha} {path}"
        );
    }
    assert_eq!(
        starts(&client, "DISCUSSED_IN", "thread:10").await,
        [commit(&h.c2)]
    );
    // b1 back-patches c2, master's commit of the group.
    assert_eq!(count(&client, "BACKPATCH_OF").await, 1);
    assert!(has_edge(&client, "BACKPATCH_OF", &commit(&h.b1), &commit(&h.c2)).await);
    // Tom Lane is one author's name; Bob Twice is two authors'; Nobody Known none's.
    assert_eq!(count(&client, "COMMITTED").await, 3);
    for sha in [&h.c0, &h.b1, &h.c2] {
        assert_eq!(
            starts(&client, "COMMITTED", &commit(sha)).await,
            ["author:1"]
        );
    }
    for sha in [&h.c1, &h.c3] {
        assert!(starts(&client, "COMMITTED", &commit(sha)).await.is_empty());
    }

    // The history's files, a deleted one's included, loaded since the history has them.
    assert_eq!(
        props(&client, &format!("file:{JSONTABLE}")).await,
        json!({ "key": format!("file:{JSONTABLE}"), "path": JSONTABLE, "subsystem": "parser", "loaded": true })
    );
    assert_eq!(
        props(&client, "file:contrib/old/old.c").await,
        json!({ "key": "file:contrib/old/old.c", "path": "contrib/old/old.c", "subsystem": "contrib/old", "loaded": true })
    );
    assert!(
        has_edge(
            &client,
            "PART_OF",
            "file:contrib/old/old.c",
            "subsystem:contrib/old"
        )
        .await
    );
    assert!(
        has_edge(
            &client,
            "PART_OF",
            "file:src/include/nodes/parsenodes.h",
            "subsystem:nodes"
        )
        .await
    );
    assert_eq!(count(&client, "File").await, 4);
    assert_eq!(count(&client, "Subsystem").await, 3);

    // The kept counts match, and a reconcile finds no drift.
    let kept: i64 = client
        .query_one(
            "SELECT count FROM graph_label_counts WHERE label = 'COMMITTED'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 3);
    assert!(graph::check(&mut client).await.unwrap().is_empty());
    reconciled(&mut client).await;

    // get_ontology's Cypher examples run, as pgh_reader, on this graph: the
    // last finds who committed c2 and the thread that discussed it.
    let reader = db.reader().await.unwrap();
    let ontology = pghacker_api::ontology::describe(&reader, "pghacker")
        .await
        .unwrap();
    let examples = ontology["examples"]["cypher"].as_array().unwrap();
    for example in examples {
        let sql = example.as_str().unwrap();
        if let Err(error) = reader.query(sql, &[]).await {
            panic!("{sql}: {error}");
        }
    }
    let rows = reader
        .query(
            &format!(
                "SELECT committer::text, thread::text FROM ({}) e",
                examples.last().unwrap().as_str().unwrap()
            ),
            &[],
        )
        .await
        .unwrap();
    let found: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        found,
        [("Tom Lane".to_owned(), "JSON_TABLE crash".to_owned())]
    );
}

#[tokio::test]
async fn resolved_anchors_name_the_history_commits() {
    let (_db, h, mut client) = installed().await;
    let cited = commit(&h.c2[..9]);
    // With the history on, resolve_anchors gives the full SHA: the citation
    // and the history's commit become one vertex.
    assert_eq!(
        anchors::resolve_in_history(&client, "pghacker")
            .await
            .unwrap(),
        1
    );
    converged(&mut client).await;
    assert!(!has_vertex(&client, &cited).await);
    assert!(
        has_edge(
            &client,
            "MENTIONS",
            "message:m2@example.org",
            &commit(&h.c2)
        )
        .await
    );
    assert_eq!(count(&client, "Commit").await, 5);
    assert_eq!(
        props(&client, &commit(&h.c2)).await["subject"],
        "Fix a crash in JSON_TABLE"
    );
    // A citation the history lacks stays as written.
    client
        .batch_execute(
            "INSERT INTO anchors (message_id, kind, value) VALUES (103, 'commit', 'abcdef1234');",
        )
        .await
        .unwrap();
    assert_eq!(
        anchors::resolve_in_history(&client, "pghacker")
            .await
            .unwrap(),
        0
    );
    converged(&mut client).await;
    assert!(
        has_edge(
            &client,
            "MENTIONS",
            "message:m4@example.org",
            "commit:abcdef1234"
        )
        .await
    );
    // Unresolved again, the citation is its own vertex again.
    client
        .batch_execute("UPDATE anchors SET resolved = NULL WHERE kind = 'commit'")
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(has_edge(&client, "MENTIONS", "message:m2@example.org", &cited).await);
    reconciled(&mut client).await;
}

#[tokio::test]
async fn a_history_sync_records_only_its_commits() {
    let (db, mut h, mut client) = installed().await;
    assert_eq!(changes(&client).await, 0);
    // Alice adds a scanner on master and stamps the branch.
    h.repo.write("src/backend/parser/scan.l", "%%\n");
    h.repo
        .write(JSONTABLE, "int a;\nint b;\nint c;\nint scanner;\n");
    let c4 = h.repo.commit("Alice Hacker", "Add a scanner");
    h.repo.checkout(&["REL_1_STABLE"]);
    h.repo.write("README", "PostgreSQL 1.1\n");
    let b2 = h.repo.commit("Alice Hacker", "Stamp 1.1");
    h.repo.checkout(&["master"]);
    assert_eq!(synced_history(&db, &h.repo).await, 2);

    // The entries name the new commits alone: their vertices, COMMITTED and
    // MODIFIES edges, and their files.
    assert_eq!(
        named_commits(&client).await,
        BTreeSet::from([c4.clone(), b2.clone()])
    );
    let recorded = entries(&client).await;
    let entry = |label: Option<&str>, key: &str, end: Option<&str>| {
        (
            label.map(str::to_owned),
            key.to_owned(),
            end.map(str::to_owned),
        )
    };
    for expected in [
        entry(None, &commit(&c4), None),
        entry(None, &commit(&b2), None),
        entry(Some("COMMITTED"), "author:4", Some(&commit(&c4))),
        entry(Some("COMMITTED"), "author:4", Some(&commit(&b2))),
        entry(
            Some("MODIFIES"),
            &commit(&c4),
            Some("file:src/backend/parser/scan.l"),
        ),
        entry(Some("MODIFIES"), &commit(&b2), Some("file:README")),
        entry(None, "file:src/backend/parser/scan.l", None),
        entry(
            Some("PART_OF"),
            "file:src/backend/parser/scan.l",
            Some("subsystem:parser"),
        ),
    ] {
        assert!(
            recorded.contains(&expected),
            "{expected:?} in {recorded:#?}"
        );
    }
    converged(&mut client).await;
    let c4_props = props(&client, &commit(&c4)).await;
    assert_eq!(
        (&c4_props["author"], &c4_props["branch"]),
        (&json!("Alice Hacker"), &json!("master"))
    );
    assert_eq!(props(&client, &commit(&b2)).await["branch"], "REL_1_STABLE");
    assert!(
        has_edge(
            &client,
            "MODIFIES",
            &commit(&c4),
            "file:src/backend/parser/scan.l"
        )
        .await
    );
    assert_eq!(
        starts(&client, "COMMITTED", &commit(&b2)).await,
        ["author:4"]
    );
    assert_eq!(
        props(&client, "file:src/backend/parser/scan.l").await["loaded"],
        true
    );
    assert!(
        has_edge(
            &client,
            "PART_OF",
            "file:src/backend/parser/scan.l",
            "subsystem:parser"
        )
        .await
    );

    // A sync with nothing new records nothing.
    assert_eq!(synced_history(&db, &h.repo).await, 0);
    assert_eq!(changes(&client).await, 0);

    // But mail citing an old commit in a new thread links them at the next
    // sync, which records that link alone.
    client
        .batch_execute(&format!(
            "INSERT INTO threads (id, list, root_message_id, subject) VALUES (11, 'pgsql-hackers', 'm5@example.org', 'b, again');
             INSERT INTO messages (id, message_id, list, thread_id, author_id, subject, sent_at)
                 VALUES (104, 'm5@example.org', 'pgsql-hackers', 11, 4, 'b, again', '2026-03-01');
             INSERT INTO anchors (message_id, kind, value) VALUES (104, 'commit', '{}');",
            &h.c1[..12]
        ))
        .await
        .unwrap();
    converged(&mut client).await;
    assert_eq!(synced_history(&db, &h.repo).await, 0);
    assert_eq!(
        entries(&client).await,
        BTreeSet::from([entry(
            Some("DISCUSSED_IN"),
            &commit(&h.c1),
            Some("thread:11")
        )])
    );
    converged(&mut client).await;
    assert_eq!(
        starts(&client, "DISCUSSED_IN", "thread:11").await,
        [commit(&h.c1)]
    );
    reconciled(&mut client).await;
}

#[tokio::test]
async fn regroups_move_backpatch_of() {
    let (db, mut h, mut client) = installed().await;
    // The same patch on REL_1_STABLE and on a new REL_2_STABLE, under
    // another subject there: the first is the group's first.
    let tagged = "typedef struct Node { int tag; } Node;\n";
    h.repo.checkout(&["REL_1_STABLE"]);
    h.repo.write("src/include/nodes/parsenodes.h", tagged);
    let b2 = h.repo.commit("Tom Lane", "Tag the nodes");
    h.repo.checkout(&["-b", "REL_2_STABLE", &h.c3]);
    h.repo.write("src/include/nodes/parsenodes.h", tagged);
    let d1 = h.repo.commit("Tom Lane", "Tag the nodes for 2");
    h.repo.checkout(&["master"]);
    assert_eq!(synced_history(&db, &h.repo).await, 2);
    converged(&mut client).await;
    assert!(has_edge(&client, "BACKPATCH_OF", &commit(&d1), &commit(&b2)).await);
    assert_eq!(props(&client, &commit(&d1)).await["branch"], "REL_2_STABLE");

    // Then on master, whose commit becomes the group's first: the regroup
    // records every commit of the group, with its edge before and after.
    h.repo.write("src/include/nodes/parsenodes.h", tagged);
    let c4 = h.repo.commit("Tom Lane", "Tag the nodes");
    assert_eq!(synced_history(&db, &h.repo).await, 1);
    let recorded = entries(&client).await;
    for (start, end) in [(&d1, &b2), (&d1, &c4), (&b2, &c4)] {
        let expected = (
            Some("BACKPATCH_OF".to_owned()),
            commit(start),
            Some(commit(end)),
        );
        assert!(recorded.contains(&expected), "{expected:?}");
    }
    assert_eq!(
        named_commits(&client).await,
        BTreeSet::from([b2.clone(), d1.clone(), c4.clone()])
    );
    converged(&mut client).await;
    let mut members = vec![commit(&b2), commit(&d1)];
    members.sort();
    assert_eq!(starts(&client, "BACKPATCH_OF", &commit(&c4)).await, members);
    assert!(!has_edge(&client, "BACKPATCH_OF", &commit(&d1), &commit(&b2)).await);
    assert_eq!(props(&client, &commit(&b2)).await["backpatch"], c4.as_str());

    // A regroup by hand, once d1's patch looks like no other.
    client
        .execute(
            "UPDATE code_commits SET patch_id = NULL WHERE sha = $1",
            &[&d1],
        )
        .await
        .unwrap();
    backpatch::regroup(&client, "pghacker").await.unwrap();
    assert_eq!(
        named_commits(&client).await,
        BTreeSet::from([d1.clone(), c4.clone()])
    );
    converged(&mut client).await;
    assert!(!has_edge(&client, "BACKPATCH_OF", &commit(&d1), &commit(&c4)).await);
    assert!(
        props(&client, &commit(&d1))
            .await
            .get("backpatch")
            .is_none()
    );
    reconciled(&mut client).await;
}

#[tokio::test]
async fn committers_follow_the_authors_names() {
    let (db, h, mut client) = installed().await;
    let indexer = indexer(&db).await;
    let committers =
        async |client: &Client, sha: &str| starts(client, "COMMITTED", &commit(sha)).await;

    // Merging Bob Twice's two authors makes the name one author's; undoing
    // the merge shares it again.
    let merged = extraction::merge_authors(
        &indexer,
        args(json!({ "from_id": 3, "into_id": 2, "reason": "Same person" })),
    )
    .await
    .unwrap();
    converged(&mut client).await;
    assert_eq!(committers(&client, &h.c1).await, ["author:2"]);
    extraction::undo_merge(&indexer, args(json!({ "merge_id": merged["merge_id"] })))
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(committers(&client, &h.c1).await.is_empty());

    // A new sender with Tom Lane's name, in capitals, shares it; once gone,
    // the name is Tom's again.
    client
        .batch_execute(
            "INSERT INTO authors (id, display_name, message_count) VALUES (200, 'TOM LANE', 1);
             INSERT INTO author_names (author_id, name, count) VALUES (200, 'TOM LANE', 1);",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    for sha in [&h.c0, &h.b1, &h.c2] {
        assert!(committers(&client, sha).await.is_empty());
    }
    client
        .batch_execute("DELETE FROM authors WHERE id = 200")
        .await
        .unwrap();
    converged(&mut client).await;
    for sha in [&h.c0, &h.b1, &h.c2] {
        assert_eq!(committers(&client, sha).await, ["author:1"]);
    }

    // Alice sends a message as Nobody Known: the name becomes hers, as
    // ingest counts it.
    client
        .batch_execute(
            "INSERT INTO author_names (author_id, name, count) VALUES (4, 'Nobody Known', 1);
             UPDATE authors SET message_count = message_count + 1 WHERE id = 4;",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    assert_eq!(committers(&client, &h.c3).await, ["author:4"]);
    // Merged into Tom, Alice's names are his.
    extraction::merge_authors(
        &indexer,
        args(json!({ "from_id": 4, "into_id": 1, "reason": "Same person" })),
    )
    .await
    .unwrap();
    converged(&mut client).await;
    assert_eq!(committers(&client, &h.c3).await, ["author:1"]);
    // A name dropped from the tables without its author changing is found
    // too, at the next batch.
    client
        .batch_execute(
            "DELETE FROM author_names WHERE name = 'Nobody Known';
             UPDATE threads SET subject = 'Another change' WHERE id = 10;",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    assert!(committers(&client, &h.c3).await.is_empty());
    reconciled(&mut client).await;
}

#[tokio::test]
async fn a_names_committer_changes_while_its_first_commit_does() {
    // Alice's first commit is on master; her back-patch to REL_1_STABLE,
    // which forked earlier, has a lower address. Her name gains a second
    // author before the graph has seen that commit.
    let (db, mut h, mut client) = installed().await;
    h.repo.write("src/backend/parser/scan.l", "%%\n");
    let c4 = h.repo.commit("Alice Hacker", "Add a scanner");
    assert_eq!(synced_history(&db, &h.repo).await, 1);
    converged(&mut client).await;
    assert_eq!(
        starts(&client, "COMMITTED", &commit(&c4)).await,
        ["author:4"]
    );
    h.repo.checkout(&["REL_1_STABLE"]);
    h.repo.write("README", "PostgreSQL 1.1\n");
    let b2 = h.repo.commit("Alice Hacker", "Stamp 1.1");
    h.repo.checkout(&["master"]);
    assert_eq!(synced_history(&db, &h.repo).await, 1);
    client
        .batch_execute(
            "INSERT INTO authors (id, display_name, message_count) VALUES (200, 'alice hacker', 1);
             INSERT INTO author_names (author_id, name, count) VALUES (200, 'alice hacker', 1);",
        )
        .await
        .unwrap();
    converged(&mut client).await;
    for sha in [&c4, &b2] {
        assert!(starts(&client, "COMMITTED", &commit(sha)).await.is_empty());
    }
    // Once the other author is gone, the name is Alice's again.
    client
        .batch_execute("DELETE FROM authors WHERE id = 200")
        .await
        .unwrap();
    converged(&mut client).await;
    for sha in [&c4, &b2] {
        assert_eq!(
            starts(&client, "COMMITTED", &commit(sha)).await,
            ["author:4"]
        );
    }
    reconciled(&mut client).await;
}

#[tokio::test]
async fn a_full_sync_repairs_what_an_older_image_leaves() {
    let (_db, _h, mut client) = installed().await;
    // An older image's full sync drops the counts of the labels it doesn't
    // know, and their bookkeeping with the commits' vertices, but leaves
    // their AGE rows.
    client
        .batch_execute(
            "DELETE FROM graph_label_counts WHERE label IN ('BACKPATCH_OF', 'COMMITTED');
             DELETE FROM graph_edges WHERE label IN ('BACKPATCH_OF', 'COMMITTED');",
        )
        .await
        .unwrap();
    let drift = graph::check(&mut client).await.unwrap();
    assert!(
        drift
            .age
            .iter()
            .any(|row| row.starts_with("COMMITTED edge ") && row.ends_with("has no bookkeeping")),
        "{drift:#?}"
    );
    // Rolled forward, the sync sees a label without a count and runs a full
    // sync, which removes the orphans and projects the edges again.
    let report = graph::run(&mut client, Duration::from_secs(10), None)
        .await
        .unwrap();
    assert!(report.full.is_some());
    let drift = graph::check(&mut client).await.unwrap();
    assert!(drift.is_empty(), "{drift:#?}");
    assert_eq!(count(&client, "COMMITTED").await, 3);
    assert_eq!(count(&client, "BACKPATCH_OF").await, 1);
}

/// xorshift64*: the same changes for the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn of<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

/// Names for authors: the history's author names in any case, and others.
const NAMES: &[&str] = &[
    "Tom Lane",
    "tom lane",
    "Bob Twice",
    "BOB TWICE",
    "Nobody Known",
    "Alice Hacker",
    "Someone Else",
];

async fn pick(client: &Client, rng: &mut Rng, sql: &str) -> Option<i64> {
    let rows = client.query(sql, &[]).await.unwrap();
    (!rows.is_empty()).then(|| rows[rng.below(rows.len() as u64) as usize].get(0))
}

/// One random change, as the indexer's writers make them: authors and their
/// names, merges and their undoing, commit citations and their resolution,
/// regroups, link refreshes and history syncs; or part of what's waiting,
/// applied.
async fn change(
    client: &mut Client,
    indexer: &Indexer,
    db: &TestDb,
    repo: &mut Repo,
    shas: &mut Vec<String>,
    rng: &mut Rng,
    n: u64,
) {
    let author = pick(
        client,
        rng,
        "SELECT id FROM authors WHERE merged_into IS NULL ORDER BY id",
    )
    .await;
    let message = pick(client, rng, "SELECT id FROM messages ORDER BY id").await;
    let sha = &shas[rng.below(shas.len() as u64) as usize].clone();
    let name = rng.of(NAMES);
    let sql = match rng.below(15) {
        // A new sender, as ingest creates one.
        0 | 1 => format!(
            "INSERT INTO authors (id, display_name, message_count) VALUES ({0}, '{name}', 1);
             INSERT INTO author_names (author_id, name, count) VALUES ({0}, '{name}', 1);",
            1000 + n
        ),
        // A known sender under another name, as ingest counts it.
        2 | 3 => {
            let Some(author) = author else { return };
            format!(
                "INSERT INTO author_names (author_id, name, count) VALUES ({author}, '{name}', 1)
                     ON CONFLICT (author_id, name) DO UPDATE SET count = author_names.count + 1;
                 UPDATE authors a SET message_count = a.message_count + 1,
                     display_name = (SELECT n.name FROM author_names n WHERE n.author_id = a.id
                                     ORDER BY n.count DESC, n.name LIMIT 1)
                 WHERE a.id = {author};"
            )
        }
        4 => {
            let other = pick(
                client,
                rng,
                "SELECT id FROM authors WHERE merged_into IS NULL ORDER BY id",
            )
            .await;
            if let (Some(from), Some(into)) = (author, other)
                && from != into
            {
                extraction::merge_authors(
                    indexer,
                    args(json!({ "from_id": from, "into_id": into, "reason": "Same person" })),
                )
                .await
                .unwrap();
            }
            return;
        }
        5 => {
            let merge = pick(
                client,
                rng,
                "SELECT l.id FROM merge_log l JOIN authors a ON a.id = l.from_id
                 WHERE l.kind = 'author' AND l.reverted_at IS NULL AND a.merged_into = l.into_id
                   AND NOT EXISTS (SELECT FROM merge_log x WHERE x.kind = 'author' AND x.reverted_at IS NULL
                                   AND x.id > l.id AND (x.from_id = l.into_id OR x.into_id = l.from_id))
                 ORDER BY l.id",
            )
            .await;
            if let Some(merge) = merge {
                extraction::undo_merge(indexer, args(json!({ "merge_id": merge })))
                    .await
                    .unwrap();
            }
            return;
        }
        // A sender with no messages, gone.
        6 => "DELETE FROM authors a WHERE a.id = (SELECT max(id) FROM authors x WHERE x.id >= 1000
                  AND x.merged_into IS NULL AND NOT EXISTS (SELECT FROM authors y WHERE y.merged_into = x.id)
                  AND NOT EXISTS (SELECT FROM messages m WHERE m.author_id = x.id))"
            .to_owned(),
        // A citation of a commit, abbreviated or in full, or none the history has.
        7 => {
            let Some(message) = message else { return };
            let value = match rng.below(3) {
                0 => sha[..9].to_owned(),
                1 => sha.clone(),
                _ => "abcdef1234".to_owned(),
            };
            format!(
                "INSERT INTO anchors (message_id, kind, value) VALUES ({message}, 'commit', '{value}')
                 ON CONFLICT DO NOTHING"
            )
        }
        8 => "DELETE FROM anchors WHERE ctid IN (SELECT ctid FROM anchors WHERE kind = 'commit'
                                                 ORDER BY id DESC LIMIT 1)"
            .to_owned(),
        // Citations resolved, as resolve_anchors does with the history on,
        // or unresolved.
        9 => {
            if rng.below(2) == 0 {
                anchors::resolve_in_history(&*client, "pghacker")
                    .await
                    .unwrap();
                return;
            }
            "UPDATE anchors SET resolved = NULL WHERE ctid IN (SELECT ctid FROM anchors
                 WHERE kind = 'commit' AND resolved IS NOT NULL ORDER BY id LIMIT 1)"
                .to_owned()
        }
        // The links refreshed, as each history sync does.
        10 => {
            links::refresh(&*client, "pghacker").await.unwrap();
            return;
        }
        // A commit's patch made like another's, or its own again, and the
        // groups made again.
        11 => {
            let other = &shas[rng.below(shas.len() as u64) as usize];
            let like = rng.below(2) == 0;
            client
                .execute(
                    "UPDATE code_commits SET patch_id = CASE WHEN $3
                         THEN (SELECT patch_id FROM code_commits WHERE sha = $2) ELSE md5(sha) END
                     WHERE sha = $1",
                    &[sha, other, &like],
                )
                .await
                .unwrap();
            backpatch::regroup(&*client, "pghacker").await.unwrap();
            return;
        }
        12 => {
            for _ in 0..=rng.below(3) {
                graph::sync_changes(client, 1 + rng.below(5) as i64)
                    .await
                    .unwrap();
            }
            return;
        }
        // A commit on master or on the older branch, under any name, synced
        // as the hourly job does: a back-patch can be a name's first commit
        // by address. As often as not, a sender with that name, in other
        // case, arrives before the graph has seen the commit.
        13 => {
            let branch = rng.of(&["master", "REL_1_STABLE"]);
            let committer = rng.of(&[
                "Tom Lane",
                "Bob Twice",
                "Alice Hacker",
                "Nobody Known",
                "Someone Else",
            ]);
            repo.checkout(&[branch]);
            repo.write("README", &format!("Change {n}\n"));
            shas.push(repo.commit(committer, &format!("Change {n}")));
            repo.checkout(&["master"]);
            synced_history(db, repo).await;
            if rng.below(2) == 0 {
                format!(
                    "INSERT INTO authors (id, display_name, message_count) VALUES ({0}, lower('{committer}'), 1);
                     INSERT INTO author_names (author_id, name, count) VALUES ({0}, lower('{committer}'), 1);",
                    1000 + n
                )
            } else {
                return;
            }
        }
        _ => {
            if rng.below(4) == 0 {
                graph::sync(client).await.unwrap();
            }
            return;
        }
    };
    client.batch_execute(&sql).await.unwrap();
}

#[tokio::test]
async fn random_changes_end_as_the_full_sync_would() {
    for seed in [3, 17, 2027] {
        eprintln!("seed {seed}");
        let (db, mut h, mut client) = installed().await;
        let indexer = indexer(&db).await;
        // Alice's and Someone Else's first commits are on master, after the
        // branch forked: a back-patch of theirs comes first by address.
        h.repo.write("README", "Alice's\n");
        let alice = h.repo.commit("Alice Hacker", "Alice's first");
        h.repo.write("README", "Someone's\n");
        let someone = h.repo.commit("Someone Else", "Someone's first");
        synced_history(&db, &h.repo).await;
        let mut shas = vec![
            h.c0.clone(),
            h.c1.clone(),
            h.b1.clone(),
            h.c2.clone(),
            h.c3.clone(),
            alice,
            someone,
        ];
        let mut rng = Rng(seed);
        for n in 0..150 {
            change(
                &mut client,
                &indexer,
                &db,
                &mut h.repo,
                &mut shas,
                &mut rng,
                n,
            )
            .await;
            if n % 10 == 9 {
                converged(&mut client).await;
            }
        }
        converged(&mut client).await;
        reconciled(&mut client).await;
    }
}
