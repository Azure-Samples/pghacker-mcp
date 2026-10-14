//! The commit-history builder on generated repositories: every file at every
//! commit rebuilds to its blob, and lines are stored once.

use std::path::{Path, PathBuf};
use std::process::Command;

use pghacker_code::history::sync::{SyncOptions, sync};
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
            url: None,
            schema: SCHEMA.into(),
            paths,
            install: false,
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
            url: None,
            schema: SCHEMA.into(),
            paths: Vec::new(),
            install: false,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("refusing"), "{error:#}");
    assert!(history::store::schema_name("public").is_err());
    assert!(history::store::schema_name("pghacker").is_err());
}

#[tokio::test]
async fn an_installed_build_replaces_the_tables_in_pghacker() {
    let repo = fixture();
    let db = TestDb::create().await.unwrap();
    // The second build swaps out the first.
    for _ in 0..2 {
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
    let verified = history::verify_build(
        &db.admin_url,
        &VerifyOptions {
            git_dir: repo.path(),
            schema: "pghacker".into(),
            revs: repo.commits.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(verified.mismatches, Vec::<String>::new());
    assert_eq!(verified.commits, 9);

    let admin = db.admin().await.unwrap();
    let strings =
        |rows: Vec<tokio_postgres::Row>| -> Vec<String> { rows.iter().map(|r| r.get(0)).collect() };
    let owners = strings(
        admin
            .query(
                "SELECT DISTINCT tableowner::text FROM pg_tables \
                 WHERE schemaname = 'pghacker' AND tablename = ANY($1)",
                &[&history::store::TABLES.to_vec()],
            )
            .await
            .unwrap(),
    );
    assert_eq!(owners, vec!["pgh_indexer"]);
    let indexes = strings(
        admin
            .query(
                "SELECT indexname::text FROM pg_indexes WHERE schemaname = 'pghacker' \
                 AND tablename = 'code_line_instances' ORDER BY 1",
                &[],
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        indexes,
        vec![
            "code_line_instances_pkey",
            "code_line_instances_scope",
            "code_line_instances_trgm",
            "code_line_instances_weave"
        ]
    );
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM pg_namespace WHERE nspname = 'pghacker_history_build'"
        )
        .await,
        0
    );
    // The reader reads the installed tables through its search_path.
    let reader = db.reader().await.unwrap();
    let lines: i64 = reader
        .query_one("SELECT count(*) FROM code_line_instances", &[])
        .await
        .unwrap()
        .get(0);
    assert!(lines > 0);
    assert_eq!(
        lines,
        count(&admin, "SELECT count(*) FROM pghacker.code_line_instances").await
    );
}

/// Every table's rows in a form independent of ids and weave keys: equal
/// for two histories with the same commits.
async fn dump(client: &Client, s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        format!(
            "SELECT name || ' ' || coalesce(fork_addr::text, '-') || ' ' || lo || ' ' || hi || ' ' || head_addr FROM {s}.code_lanes ORDER BY name"
        ),
        format!(
            "SELECT c.addr || ' ' || c.sha || ' ' || l.name || ' ' || c.subject || ' ' || c.authored_at || ' ' || c.committer_name || ' ' || md5(c.message) || ' ' || c.trailers::text || ' ' || coalesce(c.patch_id, '-') || ' ' || coalesce(c.backpatch::text, '-') FROM {s}.code_commits c JOIN {s}.code_lanes l ON l.id = c.lane ORDER BY c.addr"
        ),
        format!(
            "SELECT f.addr || ' ' || f.path || ' ' || coalesce(f.old_path, '-') || ' ' || f.status || ' ' || f.added || ' ' || f.removed || ' ' || f.is_binary || ' ' || coalesce(v.path || '@' || v.lo, '-') || ' ' || coalesce(o.path || '@' || o.lo, '-') FROM {s}.code_commit_files f LEFT JOIN {s}.code_file_versions v ON v.id = f.version LEFT JOIN {s}.code_file_versions o ON o.id = f.old_version ORDER BY f.addr, f.path"
        ),
        format!("SELECT kind || ' ' || name || ' ' || addr FROM {s}.code_refs ORDER BY kind, name"),
        format!("SELECT first_path || ' ' || born FROM {s}.code_lineages ORDER BY 1"),
        format!(
            "SELECT l.first_path || '@' || l.born || ' ' || v.path || ' ' || v.blob_sha || ' ' || v.lo || ' ' || v.hi || ' ' || coalesce(v.alive::text, '-') || ' ' || v.line_count || v.is_binary || v.final_newline || v.bom FROM {s}.code_file_versions v JOIN {s}.code_lineages l ON l.id = v.lineage ORDER BY l.first_path, l.born, v.lo, v.path"
        ),
        format!(
            "SELECT l.first_path || '@' || l.born || ' ' || i.lo || ' ' || i.hi || ' ' || coalesce(i.alive::text, '-') || ' ' || i.bytes || ' ' || i.line || coalesce(' replaces ' || r.lo || ' ' || r.line, '') FROM {s}.code_line_instances i JOIN {s}.code_lineages l ON l.id = i.lineage LEFT JOIN {s}.code_line_instances r ON r.id = i.replaces ORDER BY l.first_path, l.born, i.weave"
        ),
    ] {
        for row in client.query(sql.as_str(), &[]).await.unwrap() {
            out.push(row.get(0));
        }
    }
    out
}

#[tokio::test]
async fn a_build_then_syncs_equals_one_build() {
    let mut repo = fixture();
    let db = TestDb::create().await.unwrap();
    let build_into = |schema: &'static str, git_dir: PathBuf| {
        let url = db.admin_url.clone();
        async move {
            history::build(
                &url,
                &BuildOptions {
                    git_dir,
                    master: "master".into(),
                    url: None,
                    schema: schema.into(),
                    paths: Vec::new(),
                    install: false,
                },
            )
            .await
            .unwrap()
        }
    };
    build_into("pghacker_history", repo.path()).await;
    let first_commits = repo.commits.len();

    // More history: master and an old branch move on, a branch is created,
    // files are renamed, deleted, re-added and turned binary, and a line is
    // inserted where a branch already inserted one.
    let a = std::fs::read_to_string(repo.dir.path().join("src/a.c")).unwrap();
    repo.write(
        "src/a.c",
        a.replace(
            "a.c line 2\n",
            "a.c line 2\ninserted on master 1\ninserted on master 2\ninserted on master 3\n",
        )
        .replace(
            "a.c line 12\na.c line 13\n",
            "replaced 12\nreplaced 13\nreplaced 13b\n",
        )
        .as_bytes(),
    );
    repo.commit("c6: insert after line 2");
    repo.git(&["checkout", "-q", "REL_1_STABLE"]);
    let a1 = std::fs::read_to_string(repo.dir.path().join("a.c")).unwrap();
    repo.write(
        "a.c",
        a1.replace(
            "a.c line 2\n",
            "a.c line 2\ninserted on the branch 1\ninserted on the branch 2\n",
        )
        .as_bytes(),
    );
    repo.commit("c1b3: insert after line 2 on the branch");
    repo.git(&["checkout", "-q", "master"]);
    repo.git(&["checkout", "-q", "-b", "REL_4_STABLE"]);
    repo.write("rel4.txt", b"new branch\nsecond line\nthird line\n");
    let tip = repo.commit("c6b1: a new branch");
    repo.git(&["tag", "REL_4_0", &tip]);
    repo.git(&["checkout", "-q", "master"]);
    repo.git(&["mv", "src/a.c", "src/b.c"]);
    repo.remove("crlf.txt");
    repo.write("bom.txt", b"\x00now binary");
    repo.commit("c7: rename, delete, binary");
    repo.write("crlf.txt", b"back\r\nagain\r\n");
    let s = std::fs::read_to_string(repo.dir.path().join("src/b.c")).unwrap();
    repo.write(
        "src/b.c",
        format!("/* top 1 */\n/* top 2 */\n{s}/* end 1 */\n/* end 2 */\n").as_bytes(),
    );
    repo.commit("c8: re-add, lines at both ends");

    let stats = sync(
        db.admin().await.unwrap(),
        SyncOptions {
            git_dir: repo.path(),
            schema: "pghacker_history".into(),
            batch: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(stats.commits as usize, repo.commits.len() - first_commits);
    assert_eq!(stats.new_lanes, vec!["REL_4_STABLE"]);
    // Nothing new: nothing to do.
    let again = sync(
        db.admin().await.unwrap(),
        SyncOptions {
            git_dir: repo.path(),
            schema: "pghacker_history".into(),
            batch: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(again.commits, 0);

    build_into("pghacker_history_full", repo.path()).await;
    let admin = db.admin().await.unwrap();
    let synced = dump(&admin, "pghacker_history").await;
    let built = dump(&admin, "pghacker_history_full").await;
    let differing: Vec<(&String, &String)> = synced
        .iter()
        .zip(&built)
        .filter(|(a, b)| a != b)
        .take(5)
        .collect();
    assert_eq!(differing, Vec::<(&String, &String)>::new());
    assert_eq!(synced.len(), built.len());

    let verified = history::verify_build(
        &db.admin_url,
        &VerifyOptions {
            git_dir: repo.path(),
            schema: "pghacker_history".into(),
            revs: repo.commits.clone(),
            heads: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(verified.mismatches, Vec::<String>::new());
}

/// A commit's changed files as git counts them (diff-tree --numstat -M50%):
/// "old_path -> path added removed", binary as "- -". Symlinks and changes
/// of mode alone are left out, as the history doesn't record them.
fn git_numstat(repo: &Repo, sha: &str) -> Vec<String> {
    let args = |format: &'static str| {
        [
            "diff-tree",
            "-r",
            "--root",
            "--no-commit-id",
            "-M50%",
            "-z",
            format,
            sha,
        ]
    };
    // :oldmode newmode oldsha newsha status NUL path NUL [path NUL]
    let raw = repo.git(&args("--raw"));
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    let mut skip = std::collections::HashSet::new();
    while let Some(meta) = fields.next() {
        let m: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let renamed = m[4].starts_with(['R', 'C']);
        let first = fields.next().unwrap();
        let path = if renamed {
            fields.next().unwrap()
        } else {
            first
        };
        let symlink = m[0] == "120000" || m[1] == "120000";
        let mode_only = m[2] == m[3];
        if symlink || mode_only {
            skip.insert(path.to_owned());
        }
    }
    let num = repo.git(&args("--numstat"));
    let mut fields = num.split('\0');
    let mut out = Vec::new();
    while let Some(head) = fields.next() {
        if head.is_empty() {
            continue;
        }
        let parts: Vec<&str> = head.splitn(3, '\t').collect();
        let (old, path) = if parts[2].is_empty() {
            (fields.next().unwrap(), fields.next().unwrap())
        } else {
            ("", parts[2])
        };
        if !skip.contains(path) {
            out.push(format!("{old} -> {path} {} {}", parts[0], parts[1]));
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn commit_files_and_metadata_match_git() {
    let repo = fixture();
    let db = TestDb::create().await.unwrap();
    build(&db, &repo.path(), Vec::new()).await;
    let client = db.admin().await.unwrap();
    for sha in &repo.commits {
        let ours: Vec<String> = client
            .query(
                "SELECT coalesce(f.old_path, '') || ' -> ' || f.path || ' ' || \
                        CASE WHEN f.is_binary THEN '- -' ELSE f.added || ' ' || f.removed END \
                 FROM pghacker_history.code_commit_files f \
                 JOIN pghacker_history.code_commits c ON c.addr = f.addr \
                 WHERE c.sha = $1",
                &[sha],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        let mut ours = ours;
        ours.sort();
        assert_eq!(ours, git_numstat(&repo, sha), "commit {sha}");
    }

    // Back-patches, trailers and dates on a history of their own.
    let mut r = Repo::new();
    r.write("f.c", numbered("f.c", 10).as_bytes());
    r.write("g.c", numbered("g.c", 10).as_bytes());
    let base = r.commit("Add f.c and g.c");
    r.write(
        "f.c",
        numbered("f.c", 10)
            .replace("f.c line 3\n", "f.c line three\n")
            .as_bytes(),
    );
    r.tick += 1;
    let message = "Fix overflow in f\n\nA body.\n\n\
                   Author: Jane Doe <jane@example.org>\n\
                   Reviewed-by: Tom Lane <tgl@example.org>, Robert Haas\n\
                   Discussion: https://postgr.es/m/abc@example.org\n\
                   Backpatch-through: 1";
    r.git(&["add", "-A"]);
    r.git(&[
        "commit",
        "-q",
        "--date",
        "2025-01-01T00:00:00Z",
        "-m",
        message,
    ]);
    let fix = r.git(&["rev-parse", "HEAD"]);
    r.write(
        "f.c",
        numbered("f.c", 10)
            .replace("f.c line 3\n", "f.c line three\n")
            .replace("f.c line 9\n", "f.c line nine\n")
            .as_bytes(),
    );
    let typo = r.commit("Fix typo");
    r.git(&["checkout", "-q", "-b", "REL_1_STABLE", &base]);
    r.tick += 1;
    r.git(&["cherry-pick", &fix]);
    let picked = r.git(&["rev-parse", "HEAD"]);
    // The same subject on another file: not a back-patch of the fix.
    r.write(
        "g.c",
        numbered("g.c", 10).replace("g.c line 1\n", "").as_bytes(),
    );
    let other = r.commit("Fix overflow in f");
    // The same subject and author, a different change to the same file.
    r.write(
        "f.c",
        numbered("f.c", 10)
            .replace("f.c line 3\n", "f.c line three\n")
            .replace("f.c line 8\n", "f.c line eight\n")
            .as_bytes(),
    );
    let typo_back = r.commit("Fix typo");
    // Days later, another round of the same subject: a second group.
    let more = |extra: &str| {
        numbered("f.c", 10)
            .replace("f.c line 3\n", "f.c line three\n")
            .replace(extra, "changed\n")
            .replace("f.c line 5\n", "f.c line five\n")
    };
    r.git(&["checkout", "-q", "master"]);
    r.tick = 100;
    r.write("f.c", more("f.c line 9\n").as_bytes());
    let typo2 = r.commit("Fix typo");
    r.git(&["checkout", "-q", "REL_1_STABLE"]);
    r.write("f.c", more("f.c line 8\n").as_bytes());
    let typo2_back = r.commit("Fix typo");
    let db2 = TestDb::create().await.unwrap();
    build(&db2, &r.path(), Vec::new()).await;
    let client = db2.admin().await.unwrap();
    let row = |sha: String| {
        let client = &client;
        async move {
            client
                .query_one(
                    "SELECT addr, patch_id, backpatch, trailers, message, committer_name, \
                            (authored_at AT TIME ZONE 'UTC')::text, \
                            (committed_at AT TIME ZONE 'UTC')::text \
                     FROM pghacker_history.code_commits WHERE sha = $1",
                    &[&sha],
                )
                .await
                .unwrap()
        }
    };
    let (fix_row, picked_row) = (row(fix.clone()).await, row(picked).await);
    let fix_addr: i64 = fix_row.get(0);
    let patch: Option<String> = fix_row.get(1);
    assert!(patch.is_some());
    assert_eq!(picked_row.get::<_, Option<String>>(1), patch);
    assert_eq!(fix_row.get::<_, Option<i64>>(2), Some(fix_addr));
    assert_eq!(picked_row.get::<_, Option<i64>>(2), Some(fix_addr));
    assert_eq!(row(other).await.get::<_, Option<i64>>(2), None);
    let typo_addr: i64 = row(typo.clone()).await.get(0);
    assert_eq!(row(typo).await.get::<_, Option<i64>>(2), Some(typo_addr));
    assert_eq!(
        row(typo_back).await.get::<_, Option<i64>>(2),
        Some(typo_addr)
    );
    let typo2_addr: i64 = row(typo2.clone()).await.get(0);
    assert_eq!(row(typo2).await.get::<_, Option<i64>>(2), Some(typo2_addr));
    assert_eq!(
        row(typo2_back).await.get::<_, Option<i64>>(2),
        Some(typo2_addr)
    );
    assert_eq!(row(base).await.get::<_, Option<i64>>(2), None);

    let trailers: serde_json::Value = fix_row.get(3);
    assert_eq!(
        trailers,
        serde_json::json!({
            "Author": ["Jane Doe"],
            "Reviewed-by": ["Tom Lane", "Robert Haas"],
            "Discussion": ["https://postgr.es/m/abc@example.org"],
            "Backpatch-through": ["1"],
        })
    );
    assert_eq!(fix_row.get::<_, String>(4), message);
    assert_eq!(fix_row.get::<_, String>(5), "Test");
    assert!(fix_row.get::<_, String>(6).starts_with("2025-01-01"));
    assert!(fix_row.get::<_, String>(7).starts_with("2026-01-01"));
}
