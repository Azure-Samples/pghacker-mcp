//! Loading commits into the code tables, on a fresh database.

use std::path::Path;
use std::process::Command;

use pghacker_code::{RepoSource, SnapshotSpec, load_snapshot, materialize};
use pghacker_core::testing::TestDb;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.org")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.org")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A repository with the loader's edge cases; returns its commit.
fn fixture(dir: &Path) -> String {
    git(dir, &["init", "-q", "-b", "main"]);
    let write = |path: &str, data: &[u8]| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    };
    write(
        "src/backend/parser/gram.y",
        b"%token IDENT\n%%\nstmt: IDENT ;\n",
    );
    write("src/backend/parser/crlf.c", b"int a;\r\nint b;\r\n");
    write("src/backend/parser/no_newline.c", b"int c;");
    write("src/backend/parser/empty.c", b"");
    write("src/backend/parser/bom.c", "\u{feff}int d;\n".as_bytes());
    write("src/backend/parser/blob.bin", b"\x00\x01\x02");
    write(
        "src/backend/parser/latin1.txt",
        &[b'c', b'a', b'f', 0xe9, b'\n'],
    );
    write(
        "src/include/nodes/parsenodes.h",
        b"typedef struct Query Query;\n",
    );
    write("doc/README", b"outside the configured paths\n");
    std::os::unix::fs::symlink("gram.y", dir.join("src/backend/parser/link.y")).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "fixture"]);
    git(dir, &["rev-parse", "HEAD"])
}

async fn lines(client: &tokio_postgres::Client, path: &str) -> Vec<String> {
    client
        .query(
            "SELECT l.line FROM code_lines l JOIN code_files f ON f.id = l.file_id
             WHERE f.path = $1 ORDER BY l.line_no",
            &[&path],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

#[tokio::test]
async fn loads_a_local_commit_by_the_rules() {
    let repo = tempfile::tempdir().unwrap();
    let sha = fixture(repo.path());
    let spec = SnapshotSpec {
        repo: "fixture".into(),
        git_ref: "main".into(),
        sha: sha.clone(),
        version: Some("19".into()),
        paths: vec!["src/backend/parser".into(), "src/include/nodes/".into()],
        source: RepoSource::Local {
            path: repo.path().to_path_buf(),
        },
        source_id: None,
    };
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let stats = load_snapshot(&mut client, &spec).await.unwrap();
    assert_eq!(stats.files, 8);
    assert_eq!(stats.text_files, 6);
    assert_eq!(stats.binary_files, 2);
    assert_eq!(stats.skipped_links, 1);
    assert_eq!(stats.lines, 3 + 2 + 1 + 1 + 1);
    assert!(!stats.reused);

    assert_eq!(
        lines(&client, "src/backend/parser/crlf.c").await,
        ["int a;\r", "int b;\r"]
    );
    assert_eq!(lines(&client, "src/backend/parser/bom.c").await, ["int d;"]);
    assert!(
        lines(&client, "src/backend/parser/empty.c")
            .await
            .is_empty()
    );
    let row = client
        .query_one(
            "SELECT final_newline, subsystem FROM code_files WHERE path = 'src/backend/parser/no_newline.c'",
            &[],
        )
        .await
        .unwrap();
    assert!(!row.get::<_, bool>(0));
    assert_eq!(row.get::<_, Option<String>>(1).as_deref(), Some("parser"));
    let binary: Vec<String> = client
        .query(
            "SELECT path FROM code_files WHERE is_binary ORDER BY path",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        binary,
        [
            "src/backend/parser/blob.bin",
            "src/backend/parser/latin1.txt"
        ]
    );
    let state: String = client
        .query_one(
            "SELECT state FROM code_snapshots WHERE id = $1",
            &[&stats.snapshot_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(state, "ready");

    let again = load_snapshot(&mut client, &spec).await.unwrap();
    assert!(again.reused);
    assert_eq!(again.snapshot_id, stats.snapshot_id);

    let out = tempfile::tempdir().unwrap();
    let tree = out.path().join("tree");
    assert_eq!(materialize(&spec, &tree).unwrap(), 6);
    assert_eq!(
        std::fs::read(tree.join("src/backend/parser/crlf.c")).unwrap(),
        b"int a;\r\nint b;\r\n"
    );
    assert!(!tree.join("src/backend/parser/blob.bin").exists());
    assert!(!tree.join("doc/README").exists());
    assert_eq!(materialize(&spec, &tree).unwrap(), 6);
}

/// Fetches the pinned PG19 commit from GitHub: `cargo test -p pghacker-code -- --ignored`.
#[tokio::test]
#[ignore = "fetches from GitHub"]
async fn loads_the_pg19_subset_from_github() {
    let cache = pghacker_core::config::repo_root()
        .unwrap()
        .join(".runtime/git");
    let spec = SnapshotSpec {
        repo: "postgres".into(),
        git_ref: "REL_19_STABLE".into(),
        sha: "2359968b449a0fd53cfc04d4a644ae02370adec6".into(),
        version: Some("19".into()),
        paths: vec![
            "src/backend/parser".into(),
            "src/backend/utils/adt".into(),
            "src/include/nodes".into(),
            "contrib/pg_trgm".into(),
        ],
        source: RepoSource::Remote {
            url: "https://github.com/postgres/postgres.git".into(),
            cache_dir: cache,
        },
        source_id: None,
    };
    let db = TestDb::create().await.unwrap();
    let mut client = db.indexer().await.unwrap();
    let stats = load_snapshot(&mut client, &spec).await.unwrap();
    println!("{stats:?}");
    assert_eq!(stats.files, 206);
    assert_eq!(stats.binary_files, 0);
}
