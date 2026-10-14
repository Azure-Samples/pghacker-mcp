//! Anchors resolved against the commit history: virtual paths the code
//! tools read, and full commit SHAs.

use std::path::Path;
use std::process::Command;

use pghacker_code::history::{BuildOptions, build};
use pghacker_core::testing::TestDb;
use pghacker_indexer::anchors::resolve_in_history;

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
