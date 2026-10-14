//! The commit-history builder on generated repositories: every file at every
//! commit rebuilds to its blob, and lines are stored once.

use std::path::{Path, PathBuf};
use std::process::Command;

use pghacker_code::history::{self, BuildOptions, VerifyOptions, addr};
use pghacker_core::testing::TestDb;
use tokio_postgres::Client;

const SCHEMA: &str = "pghacker_history";

struct Repo {
    dir: tempfile::TempDir,
    tick: u32,
    commits: Vec<String>,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
            tick: 0,
            commits: Vec::new(),
        };
        repo.git(&["init", "-q", "-b", "master"]);
        repo
    }

    fn path(&self) -> PathBuf {
        self.dir.path().to_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        let date = format!("{} +0000", 1_767_225_600 + self.tick * 60);
        let output = Command::new("git")
            .args(args)
            .current_dir(self.dir.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.org")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.org")
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

    fn write(&self, path: &str, data: &[u8]) {
        let path = self.dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    fn remove(&self, path: &str) {
        std::fs::remove_file(self.dir.path().join(path)).unwrap();
    }

    fn commit(&mut self, message: &str) -> String {
        self.tick += 1;
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        let sha = self.git(&["rev-parse", "HEAD"]);
        self.commits.push(sha.clone());
        sha
    }
}

/// `n` numbered lines with a prefix, each ending in a newline.
fn numbered(prefix: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{prefix} line {i}\n")).collect()
}

/// A history with the builder's edge cases.
fn fixture() -> Repo {
    let mut r = Repo::new();
    let mut a = numbered("a.c", 20);
    r.write("a.c", a.as_bytes());
    r.write("b.txt", b"bee\n");
    r.write("crlf.txt", b"x\r\ny\r\n");
    r.write("nonl.txt", b"no newline");
    r.write("bin.dat", b"\x00\x01");
    r.write("bom.txt", "\u{feff}with a bom\n".as_bytes());
    r.write("empty.txt", b"");
    std::os::unix::fs::symlink("a.c", r.dir.path().join("link.c")).unwrap();
    r.commit("c0: the first files");

    a = a.replace("a.c line 3\n", "a.c line three\n");
    a.push_str("a.c appended\n");
    r.write("a.c", a.as_bytes());
    r.write("nonl.txt", b"no newline\n");
    r.write("bin.dat", b"now text\n");
    r.write("b.txt", b"\x00bee");
    r.write("crlf.txt", b"x\r\nz\r\ny\r\n");
    let c1 = r.commit("c1: edits");
    r.git(&["tag", "v1", &c1]);

    // A release branch, back-patching a.c.
    r.git(&["checkout", "-q", "-b", "REL_1_STABLE", &c1]);
    let a_branch = a.replace("a.c line 1\n", "a.c line one (fixed)\n");
    r.write("a.c", a_branch.as_bytes());
    r.write("branch-only.txt", b"only on the branch\n");
    r.commit("c1b1: back-patch");
    r.git(&["checkout", "-q", "master"]);

    // Rename with a small change, delete a file.
    a = a.replace("a.c line 10\n", "a.c line ten\n");
    r.remove("a.c");
    r.write("src/a.c", a.as_bytes());
    r.remove("empty.txt");
    r.write("bom.txt", b"with a bom\n");
    r.commit("c2: move a.c");

    r.git(&["checkout", "-q", "REL_1_STABLE"]);
    let a_branch = a_branch.replace("a.c line 20\n", "");
    r.write("a.c", a_branch.as_bytes());
    let tip = r.commit("c1b2: drop a line on the branch");
    r.git(&["tag", "-a", "-m", "release 1.1", "REL_1_1", &tip]);
    r.git(&["checkout", "-q", "master"]);

    // Delete b.txt, then the same fix as the branch's (a cherry-pick).
    r.remove("b.txt");
    a = a.replace("a.c line 1\n", "a.c line one (fixed)\n");
    r.write("src/a.c", a.as_bytes());
    let c3 = r.commit("c3: cherry-pick the fix, delete b.txt");

    r.git(&["checkout", "-q", "-b", "REL_2_STABLE", &c3]);
    let a2 = a.replace("a.c line 7\n", "");
    r.write("src/a.c", a2.as_bytes());
    r.commit("c3b1: drop a line on the second branch");
    r.git(&["checkout", "-q", "master"]);

    // Re-add b.txt; a mode-only change; lines at both ends.
    r.write("b.txt", b"bee again\n");
    r.commit("c4: b.txt is back");
    r.git(&["branch", "REL_3_STABLE"]);
    a = format!("/* header */\n{a}/* footer */\n");
    r.write("src/a.c", a.as_bytes());
    let mut perms = std::fs::metadata(r.dir.path().join("crlf.txt"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(r.dir.path().join("crlf.txt"), perms).unwrap();
    r.commit("c5: header and footer, crlf.txt executable");
    r
}

async fn build(db: &TestDb, git_dir: &Path, paths: Vec<String>) -> history::BuildStats {
    history::build(
        &db.admin_url,
        &BuildOptions {
            git_dir: git_dir.to_owned(),
            master: "master".into(),
            schema: SCHEMA.into(),
            paths,
        },
    )
    .await
    .unwrap()
}

async fn count(client: &Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).await.unwrap().get(0)
}

#[tokio::test]
async fn every_file_at_every_commit_rebuilds_to_its_blob() {
    let repo = fixture();
    let db = TestDb::create().await.unwrap();
    let stats = build(&db, &repo.path(), Vec::new()).await;
    assert_eq!(stats.stored.commits, 9);
    assert_eq!(stats.lanes, 4);
    // master, REL_1_STABLE, REL_2_STABLE, REL_3_STABLE; v1, REL_1_1.
    assert_eq!(stats.refs, 6);
    assert_eq!(stats.walk.renames, 1);

    let verified = history::verify_build(
        &db.admin_url,
        &VerifyOptions {
            git_dir: repo.path(),
            schema: SCHEMA.into(),
            revs: repo.commits.clone(),
            heads: true,
            scans: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(verified.mismatches, Vec::<String>::new());
    assert_eq!(verified.commits, 9);
    assert!(verified.files > 40, "{verified:?}");

    let admin = db.admin().await.unwrap();
    // A line no commit touched is one instance, across the rename too.
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pghacker_history.code_line_instances WHERE line = 'a.c line 5'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &admin,
            "SELECT count(DISTINCT lineage) FROM pghacker_history.code_file_versions \
             WHERE path IN ('a.c', 'src/a.c')"
        )
        .await,
        1
    );
    // b.txt was deleted and added again: a new lineage.
    assert_eq!(
        count(
            &admin,
            "SELECT count(DISTINCT lineage) FROM pghacker_history.code_file_versions \
             WHERE path = 'b.txt'"
        )
        .await,
        2
    );
    // The back-patched line died on the branch first: alive with a hole.
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pghacker_history.code_line_instances \
             WHERE line = 'a.c line 1' AND alive IS NOT NULL"
        )
        .await,
        1
    );
    // The fix exists twice: once per lane that made it.
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pghacker_history.code_line_instances \
             WHERE line = 'a.c line one (fixed)'"
        )
        .await,
        2
    );
    // Symlinks are left out.
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pghacker_history.code_file_versions WHERE path = 'link.c'"
        )
        .await,
        0
    );
    // A mode-only change makes no version.
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pghacker_history.code_file_versions WHERE path = 'crlf.txt'"
        )
        .await,
        2
    );
    // The branch without commits of its own heads at its fork point.
    let head: i64 = admin
        .query_one(
            "SELECT head_addr FROM pghacker_history.code_lanes WHERE name = 'REL_3_STABLE'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(addr::is_master(head));
    // Weave keys are distinct among the lines of any one version.
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pghacker_history.code_commits c, LATERAL ( \
                 SELECT l.lineage, l.weave FROM pghacker_history.code_line_instances l \
                 WHERE l.lo <= c.addr AND l.hi > c.addr \
                   AND (l.alive IS NULL OR l.alive @> c.addr) \
                 GROUP BY 1, 2 HAVING count(*) > 1) d"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn path_prefixes_slice_the_history() {
    let repo = fixture();
    let db = TestDb::create().await.unwrap();
    build(&db, &repo.path(), vec!["src".into()]).await;
    let admin = db.admin().await.unwrap();
    let paths: Vec<String> = admin
        .query(
            "SELECT DISTINCT path FROM pghacker_history.code_file_versions ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(paths, vec!["src/a.c"]);
    let verified = history::verify_build(
        &db.admin_url,
        &VerifyOptions {
            git_dir: repo.path(),
            schema: SCHEMA.into(),
            revs: repo.commits.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(verified.mismatches, Vec::<String>::new());
}

#[tokio::test]
async fn refuses_to_drop_a_schema_it_did_not_make() {
    let repo = fixture();
    let db = TestDb::create().await.unwrap();
    let admin = db.admin().await.unwrap();
    admin
        .batch_execute("CREATE SCHEMA pghacker_history; CREATE TABLE pghacker_history.keep (x int)")
        .await
        .unwrap();
    let error = history::build(
        &db.admin_url,
        &BuildOptions {
            git_dir: repo.path(),
            master: "master".into(),
            schema: SCHEMA.into(),
            paths: Vec::new(),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("refusing"), "{error:#}");
    assert!(history::store::schema_name("public").is_err());
    assert!(history::store::schema_name("pghacker").is_err());
}
