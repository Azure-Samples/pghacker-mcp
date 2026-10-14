//! The indexer against the commit history: anchors resolved to virtual
//! paths the code tools read, and full commit SHAs; and add_source answered
//! from the history instead of loading a snapshot.

use std::path::Path;
use std::process::Command;

use pghacker_code::history::{BuildOptions, build};
use pghacker_core::testing::TestDb;
use pghacker_indexer::anchors::resolve_in_history;
use pghacker_indexer::sources::{AddSourceArgs, in_history};
use serde_json::{Value, json};
use tokio_postgres::GenericClient;

fn git(dir: &Path, tick: u32, args: &[&str]) -> String {
    let date = format!("{} +0000", 1_767_225_600 + tick * 60);
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.org")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.org")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn anchors_resolve_to_virtual_paths_and_full_shas() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let write = |path: &str, text: &str| {
        std::fs::create_dir_all(d.join(path).parent().unwrap()).unwrap();
        std::fs::write(d.join(path), text).unwrap();
    };
    let commit = |tick: u32, message: &str| {
        git(d, tick, &["add", "-A"]);
        git(d, tick, &["commit", "-q", "-m", message]);
        git(d, tick, &["rev-parse", "HEAD"])
    };
    git(d, 0, &["init", "-q", "-b", "master"]);
    write("src/a.c", "a\n");
    write("src/gone.c", "gone\n");
    let c0 = commit(1, "c0");
    git(d, 2, &["checkout", "-q", "-b", "REL_1_STABLE", &c0]);
    std::fs::remove_file(d.join("src/gone.c")).unwrap();
    commit(2, "b1");
    git(d, 3, &["checkout", "-q", "master"]);
    std::fs::remove_file(d.join("src/gone.c")).unwrap();
    write("src/new.c", "new\n");
    let c1 = commit(3, "c1");

    let db = TestDb::create().await.unwrap();
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: d.to_owned(),
            master: "master".into(),
            url: None,
            schema: "pghacker_history".into(),
            paths: Vec::new(),
            install: false,
        },
    )
    .await
    .unwrap();
    let admin = db.admin().await.unwrap();
    admin
        .batch_execute(&format!(
            "INSERT INTO pghacker.threads (id, list, root_message_id) VALUES (1, 'l', 'm@x');
             INSERT INTO pghacker.messages (id, message_id, list, thread_id) VALUES (1, 'm@x', 'l', 1);
             INSERT INTO pghacker.anchors (message_id, kind, value) VALUES
                 (1, 'file', 'src/a.c'), (1, 'file', 'src/new.c'), (1, 'file', 'src/gone.c'),
                 (1, 'file', 'src/never.c'), (1, 'commit', '{}'), (1, 'commit', 'abc12');",
            &c1[..9]
        ))
        .await
        .unwrap();
    assert_eq!(
        resolve_in_history(&admin, "pghacker_history")
            .await
            .unwrap(),
        4
    );
    let resolved: Vec<(String, Option<String>)> = admin
        .query(
            "SELECT value, resolved FROM pghacker.anchors ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        resolved,
        [
            (
                "src/a.c".to_owned(),
                Some("/branch/REL_1_STABLE/src/a.c".to_owned())
            ),
            (
                "src/new.c".to_owned(),
                Some("/branch/master/src/new.c".to_owned())
            ),
            (
                "src/gone.c".to_owned(),
                Some(format!("/commit/{c0}/src/gone.c"))
            ),
            ("src/never.c".to_owned(), None),
            (c1[..9].to_owned(), Some(c1.clone())),
            ("abc12".to_owned(), None),
        ]
    );
    // Nothing changes the second time.
    assert_eq!(
        resolve_in_history(&admin, "pghacker_history")
            .await
            .unwrap(),
        0
    );
}

async fn add(client: &impl GenericClient, schema: &str, args: Value) -> anyhow::Result<Value> {
    let args: AddSourceArgs = serde_json::from_value(args).unwrap();
    in_history(client, schema, args.name.trim(), &args).await
}

#[tokio::test]
async fn add_source_finds_versions_in_the_history_and_loads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, 0, &["init", "-q", "-b", "master"]);
    std::fs::write(d.join("a.c"), "a\n").unwrap();
    git(d, 1, &["add", "-A"]);
    git(d, 1, &["commit", "-q", "-m", "c0"]);
    let c0 = git(d, 1, &["rev-parse", "HEAD"]);
    git(d, 2, &["checkout", "-q", "-b", "REL_1_STABLE"]);
    std::fs::write(d.join("a.c"), "a\nb\n").unwrap();
    git(d, 2, &["commit", "-q", "-am", "b1"]);
    let b1 = git(d, 2, &["rev-parse", "HEAD"]);
    git(d, 3, &["tag", "REL_1_0", &c0]);

    let db = TestDb::create().await.unwrap();
    let url = "https://git.postgresql.org/git/postgresql.git";
    build(
        &db.admin_url,
        &BuildOptions {
            git_dir: d.to_owned(),
            master: "master".into(),
            url: Some(url.into()),
            schema: "pghacker_history".into(),
            paths: Vec::new(),
            install: false,
        },
    )
    .await
    .unwrap();
    let admin = db.admin().await.unwrap();
    let s = "pghacker_history";
    let covered = |reply: &Value| {
        assert_eq!(reply["covered"], true, "{reply}");
        assert_eq!(reply["jobs_queued"], 0);
        (
            reply["revision"].as_str().unwrap().to_owned(),
            reply["sha"].as_str().unwrap().to_owned(),
        )
    };
    let pg = |more: Value| {
        let mut args = json!({ "kind": "postgres_version" });
        args.as_object_mut()
            .unwrap()
            .extend(more.as_object().unwrap().clone());
        args
    };
    // A major version is its stable branch, else master; a branch or tag,
    // or a commit, as named.
    let reply = add(&admin, s, pg(json!({ "name": "1" }))).await.unwrap();
    assert_eq!(covered(&reply), ("REL_1_STABLE".into(), b1.clone()));
    assert_eq!(reply["paths"], "the whole tree");
    let reply = add(&admin, s, pg(json!({ "name": "2" }))).await.unwrap();
    assert_eq!(covered(&reply), ("master".into(), c0.clone()));
    let reply = add(&admin, s, pg(json!({ "name": "1", "branch": "REL_1_0" })))
        .await
        .unwrap();
    assert_eq!(covered(&reply), ("REL_1_0".into(), c0.clone()));
    let reply = add(
        &admin,
        s,
        pg(json!({ "name": "master", "sha": b1.to_uppercase() })),
    )
    .await
    .unwrap();
    assert_eq!(covered(&reply), (b1.clone(), b1.clone()));

    // What the history lacks wakes its sync; nothing else is queued.
    admin
        .batch_execute(
            "INSERT INTO pghacker.jobs (kind, key, run_after)
             VALUES ('code_history_sync', 'recurring', now() + interval '1 hour')",
        )
        .await
        .unwrap();
    let jobs =
        "SELECT count(*), bool_and(run_after <= now()) FILTER (WHERE kind = 'code_history_sync')
                FROM pghacker.jobs";
    let before: i64 = admin.query_one(jobs, &[]).await.unwrap().get(0);
    let reply = add(&admin, s, pg(json!({ "name": "1", "branch": "REL_9_9" })))
        .await
        .unwrap();
    assert_eq!(reply["covered"], false, "{reply}");
    let note = reply["note"].as_str().unwrap();
    assert!(note.contains("REL_9_9") && note.contains(url), "{note}");
    let (after, due): (i64, bool) = admin
        .query_one(jobs, &[])
        .await
        .map(|r| (r.get(0), r.get(1)))
        .unwrap();
    assert_eq!((after, due), (before, true));
    let reply = add(&admin, s, pg(json!({ "name": "1", "sha": "f".repeat(40) })))
        .await
        .unwrap();
    assert_eq!(reply["covered"], false);
    assert!(reply["note"].as_str().unwrap().contains(&"f".repeat(40)));

    // Bad arguments, and a schema with no history.
    for (args, error) in [
        (json!({ "name": "abc" }), "major version"),
        (json!({ "name": "1", "sha": "abc123" }), "40-character"),
        (
            json!({ "name": "1", "branch": "--upload-pack=x" }),
            "isn't a branch",
        ),
    ] {
        let failed = add(&admin, s, pg(args)).await.unwrap_err().to_string();
        assert!(failed.contains(error), "{failed}");
    }
    let failed = add(&admin, "pghacker", pg(json!({ "name": "1" })))
        .await
        .unwrap_err();
    assert!(
        format!("{failed:#}").contains("no commit history installed"),
        "{failed:#}"
    );
}
